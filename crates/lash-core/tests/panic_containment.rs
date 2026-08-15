use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use lash_core::facade_support::{
    InMemorySessionStoreFactory, InlineRuntimeEffectController, LashRuntime, LlmTransportError,
    Provider, ProviderComponents, ProviderHandle, ProviderOptions, SessionTurnRequest,
    SingleProviderResolver, TurnFinish, TurnOutcome, shared_parts,
};
use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, PluginSpec,
    ProtocolDriverPlugin, ProtocolSessionPlugin, SessionPlugin, StaticPluginFactory,
};
use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, PendingToolCall, ProtocolDriverHandle,
    WaitingExecState, WaitingLlmState,
};
use lash_core::{
    AwaitEventResolver, CheckpointKind, DriverAction, DriverContextView, ExecutionScope,
    GenerationOptions, HostTurnProtocol, LlmOutputPart, LlmRequest, LlmRequestScope, LlmResponse,
    ModelSpec, PluginOptions, ProtocolBuildInput, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, ScopedEffectController, SessionCreateRequest, SessionPluginSource,
    SessionPolicy, SessionStartPoint, ToolCall, ToolCallOutcome, ToolContract, ToolDefinition,
    ToolFailureClass, ToolManifest, ToolProvider, ToolResult, ToolRetryDisposition,
    TurnDriverConfig, TurnDriverPreamble, TurnInput,
};

fn test_runtime_owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("panic-test-worker", "panic-test-boot")
}
use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

static PANIC_MODE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct RecordingEffectController {
    inner: InlineRuntimeEffectController,
    outcomes: std::sync::Mutex<Vec<RuntimeEffectOutcome>>,
}

impl AwaitEventResolver for RecordingEffectController {}

#[async_trait]
impl RuntimeEffectController for RecordingEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let outcome = self.inner.execute_effect(envelope, local_executor).await?;
        self.outcomes.lock_recover().push(outcome.clone());
        Ok(outcome)
    }
}

impl RecordingEffectController {
    fn provider_panic_projection(&self) -> (Option<String>, String, String) {
        self.outcomes
            .lock_recover()
            .iter()
            .find_map(|outcome| {
                let RuntimeEffectOutcome::LlmCall {
                    result,
                    call_record,
                    ..
                } = outcome
                else {
                    return None;
                };
                let error = result.as_ref().as_ref().expect_err("typed LLM failure");
                let attempt_code = call_record
                    .as_ref()
                    .and_then(|record| record.attempts.first())
                    .and_then(|attempt| attempt.error.as_ref())
                    .and_then(|error| error.provider_code.as_deref())
                    .expect("typed provider attempt code");
                Some((
                    error.code.clone(),
                    error.message.clone(),
                    attempt_code.to_string(),
                ))
            })
            .expect("recorded provider panic outcome")
    }
}

#[derive(Clone, Debug)]
struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    fn kind(&self) -> &'static str {
        "panic-provider"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        panic!("provider payload only")
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct ClassifierKeywordPanicProvider;

#[async_trait]
impl Provider for ClassifierKeywordPanicProvider {
    fn kind(&self) -> &'static str {
        "classifier-keyword-panic"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        panic!("safety context length does not exist")
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct PanicOnceProvider {
    panic_next: Arc<AtomicBool>,
}

#[async_trait]
impl Provider for PanicOnceProvider {
    fn kind(&self) -> &'static str {
        "panic-once-provider"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        if self.panic_next.swap(false, Ordering::SeqCst) {
            panic!("provider turn payload only");
        }
        Ok(text_response("next turn works"))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct ScriptedProvider {
    responses: Arc<Vec<LlmResponse>>,
    next: Arc<AtomicUsize>,
}

impl ScriptedProvider {
    fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: Arc::new(responses),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn into_handle(self) -> ProviderHandle {
        ProviderHandle::new(ProviderComponents::new(Box::new(self)))
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn kind(&self) -> &'static str {
        "scripted-panic-containment"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .responses
            .get(index)
            .unwrap_or_else(|| panic!("unexpected scripted provider call {index}"))
            .clone())
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

struct PanicTool;

#[async_trait]
impl ToolProvider for PanicTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![panic_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "panic_tool").then(|| Arc::new(panic_tool_definition().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
        panic!("tool payload only")
    }
}

struct MinimalProtocolFactory;

impl PluginFactory for MinimalProtocolFactory {
    fn id(&self) -> &'static str {
        "panic-containment-protocol"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(MinimalProtocolPlugin))
    }
}

struct MinimalProtocolPlugin;

impl SessionPlugin for MinimalProtocolPlugin {
    fn id(&self) -> &'static str {
        "panic-containment-protocol"
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        registrar
            .protocol()
            .session(Arc::new(MinimalProtocolSession))?;
        registrar
            .protocol()
            .protocol_driver(Arc::new(MinimalProtocolDriver))
    }
}

struct MinimalProtocolSession;

#[async_trait]
impl ProtocolSessionPlugin for MinimalProtocolSession {}

struct MinimalProtocolDriver;

impl ProtocolDriverPlugin for MinimalProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        TurnDriverPreamble {
            config: TurnDriverConfig::chat(
                Arc::new(MinimalProtocolDriver),
                false,
                Arc::new(|message_id, max_turns| lash_core::Message {
                    id: message_id,
                    role: lash_core::MessageRole::System,
                    parts: shared_parts(vec![lash_core::Part::error(
                        "turn-limit".to_string(),
                        format!("turn limit {max_turns}"),
                    )]),
                    origin: None,
                }),
            ),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            tool_names_fingerprint: input.tool_catalog.tool_names_fingerprint(),
            execution_prompt: Arc::from(""),
            prompt_contributions: input.extra_prompt_contributions,
        }
    }
}

impl ProtocolDriverHandle<HostTurnProtocol> for MinimalProtocolDriver {
    fn prepare_protocol_iteration(&self, context: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::StartLlm {
            request: context.project_llm_request(true),
            driver_state: None,
        }]
    }

    fn handle_llm_success(
        &self,
        _context: DriverContextView<'_>,
        _waiting: WaitingLlmState<HostTurnProtocol>,
        response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let mut text = String::new();
        let mut calls = Vec::new();
        for part in response.parts {
            match part {
                LlmOutputPart::Text { text: part, .. } => text.push_str(&part),
                LlmOutputPart::Reasoning { .. } => {}
                LlmOutputPart::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    replay,
                } => calls.push(PendingToolCall {
                    call_id,
                    tool_name,
                    args: serde_json::from_str(&input_json).expect("tool input"),
                    replay,
                }),
            }
        }
        if calls.is_empty() {
            vec![DriverAction::Finish(TurnOutcome::Finished(
                TurnFinish::AssistantMessage { text },
            ))]
        } else {
            vec![DriverAction::StartTools { calls }]
        }
    }

    fn handle_tool_results(
        &self,
        _context: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        vec![
            DriverAction::AdvanceProtocolIteration,
            DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::AfterWork,
                on_empty: CheckpointResumeAction::PrepareIteration,
            },
        ]
    }

    fn handle_exec_result(
        &self,
        _context: DriverContextView<'_>,
        _waiting: WaitingExecState<HostTurnProtocol>,
        _result: Result<lash_core::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

fn protocol_factory() -> Arc<dyn PluginFactory> {
    Arc::new(MinimalProtocolFactory)
}

fn panic_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:panic_tool",
        "panic_tool",
        "panic for containment testing",
        ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "panic-test-model".to_string(),
        messages: Vec::new(),
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: Default::default(),
        generation: GenerationOptions::default(),
        scope: LlmRequestScope::new("panic-test", "panic-test:frame", "panic-test:request"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

fn policy(provider_id: &str) -> SessionPolicy {
    SessionPolicy {
        provider_id: provider_id.to_string(),
        model: ModelSpec::builder("panic-test-model")
            .context_window_tokens(32_000)
            .build()
            .expect("valid model"),
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        full_text: text.to_string(),
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn turn_scope(session_id: &str, turn_id: &str) -> ScopedEffectController<'static> {
    ScopedEffectController::shared(
        Arc::new(InlineRuntimeEffectController::default()),
        ExecutionScope::turn(session_id, turn_id),
    )
    .expect("turn scope")
}

fn recording_turn_scope(
    controller: Arc<RecordingEffectController>,
    session_id: &str,
    turn_id: &str,
) -> ScopedEffectController<'static> {
    ScopedEffectController::shared(controller, ExecutionScope::turn(session_id, turn_id))
        .expect("recording turn scope")
}

#[tokio::test]
async fn provider_panic_is_typed_and_non_retryable() {
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let mut provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let failure = provider
        .complete(request())
        .await
        .expect_err("typed failure");

    assert_eq!(failure.error.code.as_deref(), Some("provider_panicked"));
    assert_eq!(failure.error.message, "provider payload only");
    assert!(!failure.error.retryable);
    assert_eq!(failure.call_record.attempts.len(), 1);
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.reason.as_deref()),
        Some("not_retryable")
    );
}

#[tokio::test]
async fn manufactured_provider_panic_bypasses_text_classification() {
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let mut provider = ProviderHandle::new(ProviderComponents::new(Box::new(
        ClassifierKeywordPanicProvider,
    )));
    let failure = provider
        .complete(request())
        .await
        .expect_err("typed failure");

    assert_eq!(failure.error.code.as_deref(), Some("provider_panicked"));
    assert_eq!(failure.error.kind, lash_core::ProviderFailureKind::Unknown);
    assert!(!failure.error.retryable);
}

#[tokio::test]
async fn tool_panic_is_recorded_and_the_session_runs_its_next_turn() {
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let provider = ScriptedProvider::new(vec![
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "panic-call".to_string(),
                tool_name: "panic_tool".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        text_response("turn recovered"),
        text_response("next turn works"),
    ])
    .into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider));
    let plugin = Arc::new(StaticPluginFactory::new(
        "panic-tool-test",
        PluginSpec::new().with_tool_provider(Arc::new(PanicTool)),
    ));
    let mut runtime = Box::pin(
        LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            test_runtime_owner(),
        )
        .with_session_id("tool-panic-session")
        .with_policy(policy("scripted-panic-containment"))
        .with_plugin_factories(vec![protocol_factory(), plugin])
        .with_runtime_host(host)
        .build(),
    )
    .await
    .expect("runtime");

    let first = runtime
        .run_turn_assembled(
            TurnInput::text("call the tool"),
            CancellationToken::new(),
            turn_scope("tool-panic-session", "tool-panic-turn"),
        )
        .await
        .expect("turn survives tool panic");
    let ToolCallOutcome::Failure(failure) = &first.tool_calls[0].output.outcome else {
        panic!("tool panic must be recorded as a failure")
    };
    assert_eq!(failure.class, ToolFailureClass::Internal);
    assert_eq!(failure.code, "tool_panicked");
    assert_eq!(failure.message, "tool payload only");
    assert_eq!(failure.retry, ToolRetryDisposition::Never);
    assert_eq!(first.assistant_output.safe_text, "turn recovered");

    let next = runtime
        .run_turn_assembled(
            TurnInput::text("continue"),
            CancellationToken::new(),
            turn_scope("tool-panic-session", "after-tool-panic"),
        )
        .await
        .expect("next turn");
    assert_eq!(next.assistant_output.safe_text, "next turn works");
}

#[tokio::test]
async fn child_turn_panic_is_typed_and_the_parent_remains_alive() {
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let provider = ScriptedProvider::new(vec![text_response("parent still alive")]).into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider));
    let panic_once = Arc::new(AtomicBool::new(true));
    let plugin = Arc::new(StaticPluginFactory::new(
        "child-panic-test",
        PluginSpec::new().with_prompt_contributor(Arc::new(move |_context| {
            let panic_once = Arc::clone(&panic_once);
            Box::pin(async move {
                if panic_once.swap(false, Ordering::SeqCst) {
                    panic!("child turn payload only");
                }
                Ok(Vec::new())
            })
        })),
    ));
    let mut runtime = Box::pin(
        LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            test_runtime_owner(),
        )
        .with_session_id("parent-session")
        .with_policy(policy("scripted-panic-containment"))
        .with_plugin_factories(vec![protocol_factory(), plugin])
        .with_runtime_host(host)
        .with_session_store_factory(Arc::new(InMemorySessionStoreFactory::new()))
        .build(),
    )
    .await
    .expect("runtime");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let child = lifecycle
        .create_session(
            SessionCreateRequest::child_session(
                "parent-session",
                SessionStartPoint::Empty,
                PluginOptions::default(),
            )
            .with_session_id("panicking-child")
            .with_plugin_source(SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("create child");
    let error = lifecycle
        .start_turn(
            SessionTurnRequest::new(
                &child.session_id,
                "panicking-child-turn",
                TurnInput::text("panic"),
                turn_scope(&child.session_id, "panicking-child-turn"),
            )
            .expect("child turn request"),
        )
        .await
        .expect_err("typed child failure");
    assert!(
        error
            .to_string()
            .contains("child_turn_panicked: child turn payload only")
    );

    let parent = runtime
        .run_turn_assembled(
            TurnInput::text("continue parent"),
            CancellationToken::new(),
            turn_scope("parent-session", "parent-after-child-panic"),
        )
        .await
        .expect("parent survives child panic");
    assert_eq!(parent.assistant_output.safe_text, "parent still alive");
}

#[tokio::test]
async fn provider_panic_records_the_typed_attempt_releases_the_lease_and_next_turn_succeeds() {
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicOnceProvider {
        panic_next: Arc::new(AtomicBool::new(true)),
    })));
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider));
    let mut runtime = Box::pin(
        LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            test_runtime_owner(),
        )
        .with_session_id("provider-panic-session")
        .with_policy(policy("panic-once-provider"))
        .with_plugin_factories(vec![protocol_factory()])
        .with_runtime_host(host)
        .build(),
    )
    .await
    .expect("runtime");

    let failed = runtime
        .run_turn_assembled(
            TurnInput::text("panic provider"),
            CancellationToken::new(),
            turn_scope("provider-panic-session", "provider-panic-turn"),
        )
        .await
        .expect("provider panic terminates the turn cleanly");
    assert!(matches!(
        failed.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError)
    ));
    let attempt = failed
        .llm_calls
        .first()
        .and_then(|call| call.attempts.first())
        .expect("provider panic attempt record");
    assert_eq!(
        attempt
            .error
            .as_ref()
            .and_then(|error| error.provider_code.as_deref()),
        Some("provider_panicked")
    );

    // A second turn can acquire the same session lane immediately: the first
    // turn's lease was released on its typed failure path.
    let next = runtime
        .run_turn_assembled(
            TurnInput::text("continue"),
            CancellationToken::new(),
            turn_scope("provider-panic-session", "after-provider-panic"),
        )
        .await
        .expect("next turn");
    assert_eq!(next.assistant_output.safe_text, "next turn works");
}

#[tokio::test]
async fn provider_panic_effect_is_identical_before_quiet_return_or_loud_reraise() {
    use futures_util::FutureExt as _;

    let _mode = PANIC_MODE.lock().await;

    let quiet_controller = Arc::new(RecordingEffectController::default());
    let quiet_provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let mut quiet_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    quiet_host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(quiet_provider));
    let mut quiet_runtime = Box::pin(
        LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            test_runtime_owner(),
        )
        .with_session_id("quiet-provider-record-session")
        .with_policy(policy("panic-provider"))
        .with_plugin_factories(vec![protocol_factory()])
        .with_runtime_host(quiet_host)
        .build(),
    )
    .await
    .expect("quiet runtime");

    lash_core::panic_containment::set_loud(false);
    quiet_runtime
        .run_turn_assembled(
            TurnInput::text("record provider panic quietly"),
            CancellationToken::new(),
            recording_turn_scope(
                Arc::clone(&quiet_controller),
                "quiet-provider-record-session",
                "quiet-provider-record-turn",
            ),
        )
        .await
        .expect("quiet provider panic is typed");
    let quiet = quiet_controller.provider_panic_projection();

    let loud_controller = Arc::new(RecordingEffectController::default());
    let loud_provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let mut loud_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    loud_host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(loud_provider));
    let mut loud_runtime = Box::pin(
        LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            test_runtime_owner(),
        )
        .with_session_id("loud-provider-record-session")
        .with_policy(policy("panic-provider"))
        .with_plugin_factories(vec![protocol_factory()])
        .with_runtime_host(loud_host)
        .build(),
    )
    .await
    .expect("loud runtime");

    lash_core::panic_containment::set_loud(true);
    let loud_result = std::panic::AssertUnwindSafe(loud_runtime.run_turn_assembled(
        TurnInput::text("record provider panic loudly"),
        CancellationToken::new(),
        recording_turn_scope(
            Arc::clone(&loud_controller),
            "loud-provider-record-session",
            "loud-provider-record-turn",
        ),
    ))
    .catch_unwind()
    .await;
    lash_core::panic_containment::set_loud(false);

    assert!(loud_result.is_err(), "loud mode must re-raise");
    assert_eq!(
        loud_controller.provider_panic_projection(),
        quiet,
        "loudness changes propagation only after the identical typed effect is recorded"
    );
}

#[tokio::test]
async fn provider_turn_panic_reaches_the_harness_when_loud() {
    use futures_util::FutureExt as _;

    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(true);
    let provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider));
    let mut runtime = Box::pin(
        LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            test_runtime_owner(),
        )
        .with_session_id("loud-provider-panic-session")
        .with_policy(policy("panic-provider"))
        .with_plugin_factories(vec![protocol_factory()])
        .with_runtime_host(host)
        .build(),
    )
    .await
    .expect("runtime");

    let panic = std::panic::AssertUnwindSafe(runtime.run_turn_assembled(
        TurnInput::text("panic provider loudly"),
        CancellationToken::new(),
        turn_scope("loud-provider-panic-session", "loud-provider-panic-turn"),
    ))
    .catch_unwind()
    .await;
    lash_core::panic_containment::set_loud(false);
    assert!(panic.is_err(), "loud provider panic must reach the harness");
}
