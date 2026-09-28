#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use lash_core::SessionId;
use lash_core::TurnId;
use lash_core::testing::TestTurnDrive as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use lash_core::facade_support::{
    LashRuntime, LlmTransportError, Provider, ProviderComponents, ProviderHandle, ProviderOptions,
    SingleProviderResolver, TurnFinish, TurnOutcome,
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
    AdmittedScope, AwaitEventResolver, CheckpointKind, DriverAction, DriverContextView,
    GenerationOptions, HostTurnProtocol, LlmOutputPart, LlmRequest, LlmRequestScope, LlmResponse,
    ModelSpec, ProtocolBuildInput, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    ScopedEffectController, SessionPolicy, ToolAttemptOutcome, ToolCall, ToolCallOutcome,
    ToolContract, ToolDefinition, ToolFailureClass, ToolManifest, ToolProvider, ToolRetryStatus,
    TurnDriverConfig, TurnDriverPreamble, TurnInput,
};

fn test_runtime_owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("panic-test-worker", "panic-test-boot")
}
use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

/// The Restate server double (FIG-3668): every scope opens through
/// [`lash_restate_test::RestateTestBackend::open_handler`].
async fn double(seed: u64) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate server double")
}

static PANIC_MODE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Records every effect outcome of the lent handler's controller for one
/// turn.
struct RecordingEffectController<'a> {
    inner: ScopedEffectController<'a>,
    outcomes: std::sync::Mutex<Vec<RuntimeEffectOutcome>>,
}

/// A recorder over `handler`'s scope controller.
fn recording_controller<'a>(
    handler: &'a lash_restate_test::OpenHandler,
) -> RecordingEffectController<'a> {
    RecordingEffectController {
        inner: handler.scoped(),
        outcomes: std::sync::Mutex::new(Vec::new()),
    }
}

#[async_trait]
impl AwaitEventResolver for RecordingEffectController<'_> {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.controller().await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .controller()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner.controller().await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner
            .controller()
            .resolve_await_event(key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.inner.controller().peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.inner
            .controller()
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &lash_core::SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &lash_core::SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .retire_await_events_for_scope(scope)
            .await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<bool, lash_core::RuntimeError> {
        self.inner
            .controller()
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .reinstate_await_event_scope(scope)
            .await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<bool, lash_core::RuntimeError> {
        self.inner
            .controller()
            .await_event_scope_is_retired(scope)
            .await
    }
}

#[async_trait]
impl RuntimeEffectController for RecordingEffectController<'_> {
    async fn read_recorded_journal(
        &self,
        range: &lash_core::RecordedKeyRange,
    ) -> Result<lash_core::RecordedJournal, RuntimeEffectControllerError> {
        self.inner.controller().read_recorded_journal(range).await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let outcome = self
            .inner
            .controller()
            .execute_effect(envelope, local_executor)
            .await?;
        self.outcomes.lock_recover().push(outcome.clone());
        Ok(outcome)
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.controller().open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: std::sync::Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.controller().register_group_executors(executors)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .await_next_settlement(handle, cancel)
            .await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner
            .controller()
            .read_group_settlement(group_key, rank)
            .await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .close_effect_group(handle, disposition)
            .await
    }
    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner
            .controller()
            .commit_group_child_final(commit)
            .await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
    }
}

impl RecordingEffectController<'_> {
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
                    .and_then(|error| error.code.as_ref())
                    .expect("typed provider attempt code")
                    .namespaced();
                Some((
                    error.code.as_ref().map(|code| code.to_string()),
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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolAttemptOutcome {
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
            config: TurnDriverConfig::chat(Arc::new(MinimalProtocolDriver), false),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            tool_names_fingerprint: input.tool_catalog.tool_names_fingerprint(),
            execution_title: Arc::from("Execution"),
            execution_prompt: Arc::from(""),
            prompt_contributions: input.extra_prompt_contributions,
            writer_formats: input.writer_formats,
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
        instructions: None,
        model: "panic-test-model".to_string(),
        messages: Vec::new(),
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
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn recording_turn_scope<'a>(
    controller: &'a RecordingEffectController<'a>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> ScopedEffectController<'a> {
    ScopedEffectController::borrowed(controller, AdmittedScope::turn(session_id, turn_id))
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

    assert_eq!(
        failure.error.code.as_ref().map(|code| code.to_string()),
        Some("lash:provider_panicked".to_string())
    );
    assert_eq!(failure.error.message, "provider payload only");
    assert!(!failure.error.is_retryable());
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

    assert_eq!(
        failure.error.code.as_ref().map(|code| code.to_string()),
        Some("lash:provider_panicked".to_string())
    );
    assert_eq!(failure.error.kind, lash_core::ProviderFailureKind::Unknown);
    assert!(!failure.error.is_retryable());
}

#[tokio::test]
async fn tool_panic_is_recorded_and_the_session_runs_its_next_turn() {
    let double = double(1).await;
    let backend = double.lash_backend();
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
    let mut host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider));
    let plugin = Arc::new(StaticPluginFactory::new(
        "panic-tool-test",
        PluginSpec::new().with_tool_provider(Arc::new(PanicTool)),
    ));
    let mut runtime = Box::pin(
        LashRuntime::builder(host, test_runtime_owner())
            .with_session_id("tool-panic-session")
            .with_policy(policy("scripted-panic-containment"))
            .with_plugin_factories(vec![protocol_factory(), plugin])
            .build(),
    )
    .await
    .expect("runtime");

    let first_handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            SessionId::from("tool-panic-session"),
            TurnId::from("tool-panic-turn"),
        ))
        .await
        .expect("open the scope's handler");
    let first = runtime
        .drive_turn(
            TurnInput::text("call the tool"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                first_handler.scoped(),
            ),
        )
        .await
        .expect("turn survives tool panic");
    first_handler
        .close()
        .await
        .expect("close the scope's handler");
    let ToolCallOutcome::Failure(failure) = &first.tool_calls[0].output.outcome else {
        panic!("tool panic must be recorded as a failure")
    };
    assert_eq!(failure.class, ToolFailureClass::Internal);
    assert_eq!(failure.code, "tool_panicked");
    assert_eq!(failure.message, "tool payload only");
    assert_eq!(failure.retry, ToolRetryStatus::Never);
    assert_eq!(first.assistant_output.safe_text, "turn recovered");

    let next_handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            SessionId::from("tool-panic-session"),
            TurnId::from("after-tool-panic"),
        ))
        .await
        .expect("open the scope's handler");
    let next = runtime
        .drive_turn(
            TurnInput::text("continue"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                next_handler.scoped(),
            ),
        )
        .await
        .expect("next turn");
    next_handler
        .close()
        .await
        .expect("close the scope's handler");
    assert_eq!(next.assistant_output.safe_text, "next turn works");
}

#[tokio::test]
async fn provider_panic_records_the_typed_attempt_releases_the_lease_and_next_turn_succeeds() {
    let double = double(2).await;
    let backend = double.lash_backend();
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicOnceProvider {
        panic_next: Arc::new(AtomicBool::new(true)),
    })));
    let mut host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider));
    let mut runtime = Box::pin(
        LashRuntime::builder(host, test_runtime_owner())
            .with_session_id("provider-panic-session")
            .with_policy(policy("panic-once-provider"))
            .with_plugin_factories(vec![protocol_factory()])
            .build(),
    )
    .await
    .expect("runtime");

    let failed_handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            SessionId::from("provider-panic-session"),
            TurnId::from("provider-panic-turn"),
        ))
        .await
        .expect("open the scope's handler");
    let failed = runtime
        .drive_turn(
            TurnInput::text("panic provider"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                failed_handler.scoped(),
            ),
        )
        .await
        .expect("provider panic terminates the turn cleanly");
    failed_handler
        .close()
        .await
        .expect("close the scope's handler");
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
            .and_then(|error| error.code.as_ref())
            .map(|code| code.namespaced()),
        Some("lash:provider_panicked".to_string())
    );

    // A second turn can acquire the same session lane immediately: the first
    // turn's lease was released on its typed failure path.
    let next_handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            SessionId::from("provider-panic-session"),
            TurnId::from("after-provider-panic"),
        ))
        .await
        .expect("open the scope's handler");
    let next = runtime
        .drive_turn(
            TurnInput::text("continue"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                next_handler.scoped(),
            ),
        )
        .await
        .expect("next turn");
    next_handler
        .close()
        .await
        .expect("close the scope's handler");
    assert_eq!(next.assistant_output.safe_text, "next turn works");
}

#[tokio::test]
async fn provider_panic_effect_is_identical_before_quiet_return_or_loud_reraise() {
    let double = double(3).await;
    let backend = double.lash_backend();
    use futures_util::FutureExt as _;

    let _mode = PANIC_MODE.lock().await;

    let quiet_handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            SessionId::from("quiet-provider-record-session"),
            TurnId::from("quiet-provider-record-turn"),
        ))
        .await
        .expect("open the scope's handler");
    let quiet_controller = recording_controller(&quiet_handler);
    let quiet_provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let mut quiet_host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    quiet_host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(quiet_provider));
    let mut quiet_runtime = Box::pin(
        LashRuntime::builder(quiet_host, test_runtime_owner())
            .with_session_id("quiet-provider-record-session")
            .with_policy(policy("panic-provider"))
            .with_plugin_factories(vec![protocol_factory()])
            .build(),
    )
    .await
    .expect("quiet runtime");

    lash_core::panic_containment::set_loud(false);
    quiet_runtime
        .drive_turn(
            TurnInput::text("record provider panic quietly"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                recording_turn_scope(
                    &quiet_controller,
                    &SessionId::from("quiet-provider-record-session"),
                    &TurnId::from("quiet-provider-record-turn"),
                ),
            ),
        )
        .await
        .expect("quiet provider panic is typed");
    let quiet = quiet_controller.provider_panic_projection();

    let loud_handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            SessionId::from("loud-provider-record-session"),
            TurnId::from("loud-provider-record-turn"),
        ))
        .await
        .expect("open the scope's handler");
    let loud_controller = recording_controller(&loud_handler);
    let loud_provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let mut loud_host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    loud_host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(loud_provider));
    let mut loud_runtime = Box::pin(
        LashRuntime::builder(loud_host, test_runtime_owner())
            .with_session_id("loud-provider-record-session")
            .with_policy(policy("panic-provider"))
            .with_plugin_factories(vec![protocol_factory()])
            .build(),
    )
    .await
    .expect("loud runtime");

    lash_core::panic_containment::set_loud(true);
    let loud_result = std::panic::AssertUnwindSafe(loud_runtime.drive_turn(
        TurnInput::text("record provider panic loudly"),
        lash_core::facade_support::TurnOptions::new(
            CancellationToken::new(),
            recording_turn_scope(
                &loud_controller,
                &SessionId::from("loud-provider-record-session"),
                &TurnId::from("loud-provider-record-turn"),
            ),
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
    drop(quiet_controller);
    drop(loud_controller);
    quiet_handler
        .close()
        .await
        .expect("close the scope's handler");
    loud_handler
        .close()
        .await
        .expect("close the scope's handler");
}

#[tokio::test]
async fn provider_turn_panic_reaches_the_harness_when_loud() {
    let double = double(4).await;
    let backend = double.lash_backend();
    use futures_util::FutureExt as _;

    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(true);
    let provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let mut host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider));
    let mut runtime = Box::pin(
        LashRuntime::builder(host, test_runtime_owner())
            .with_session_id("loud-provider-panic-session")
            .with_policy(policy("panic-provider"))
            .with_plugin_factories(vec![protocol_factory()])
            .build(),
    )
    .await
    .expect("runtime");

    let panic_handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            SessionId::from("loud-provider-panic-session"),
            TurnId::from("loud-provider-panic-turn"),
        ))
        .await
        .expect("open the scope's handler");
    let panic = std::panic::AssertUnwindSafe(runtime.drive_turn(
        TurnInput::text("panic provider loudly"),
        lash_core::facade_support::TurnOptions::new(
            CancellationToken::new(),
            panic_handler.scoped(),
        ),
    ))
    .catch_unwind()
    .await;
    lash_core::panic_containment::set_loud(false);
    panic_handler
        .close()
        .await
        .expect("close the scope's handler");
    assert!(panic.is_err(), "loud provider panic must reach the harness");
}
