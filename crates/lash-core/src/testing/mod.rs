//! In-tree test fixtures shared across the lash crate's test modules.
//!
//! Cuts down on per-test-module `MockSessionManager` boilerplate by
//! providing a configurable mock implementation plus a couple of small
//! builders for common policy / turn fixtures.

/// Backend-agnostic conformance suites for durable-backend traits
/// (`ProcessRegistry`, …). Run the same suite against every implementation —
/// production and in-memory double alike — so the trait contract has a single
/// source of truth and the doubles cannot silently drift.
pub mod conformance;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::llm::transport::LlmTransportError;
use crate::llm::types::{LlmRequest, LlmResponse};
use crate::plugin::{PluginError, SessionCreateRequest, SessionHandle, SessionSnapshot};
use crate::provider::{Provider, ProviderComponents, ProviderHandle};
use crate::session_model::{ConversationRecord, SessionHistoryRecord};
use crate::{
    AssembledTurn, AssistantOutput, ExecutionSummary, ModelSpec, OutputState, ProcessRegistry,
    ProviderOptions, RuntimeSessionState, SessionPolicy, TokenUsage, TurnFinish, TurnOutcome,
    TurnStop,
};

type CompletionFuture =
    Pin<Box<dyn Future<Output = Result<LlmResponse, LlmTransportError>> + Send>>;
type CompletionFn = dyn Fn(LlmRequest) -> CompletionFuture + Send + Sync;
type SerializeConfigFn = dyn Fn() -> serde_json::Value + Send + Sync;

fn empty_provider_config() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

/// Configurable provider fixture used by lash's own tests and shared
/// with downstream plugin crates through `lash_core::testing`.
#[derive(Clone)]
pub struct TestProvider {
    kind: &'static str,
    requires_streaming: bool,
    options: ProviderOptions,
    serialize_config: Arc<SerializeConfigFn>,
    complete: Arc<CompletionFn>,
}

impl std::fmt::Debug for TestProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestProvider")
            .field("kind", &self.kind)
            .field("requires_streaming", &self.requires_streaming)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Default for TestProvider {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl TestProvider {
    pub fn builder() -> TestProviderBuilder {
        TestProviderBuilder::new()
    }

    pub fn into_handle(self) -> ProviderHandle {
        ProviderHandle::new(ProviderComponents::new(Box::new(self)))
    }
}

pub struct TestProviderBuilder {
    provider: TestProvider,
}

impl TestProviderBuilder {
    pub fn new() -> Self {
        Self {
            provider: TestProvider {
                kind: "test",
                requires_streaming: false,
                options: ProviderOptions::default(),
                serialize_config: Arc::new(empty_provider_config),
                complete: Arc::new(|_request| {
                    Box::pin(async {
                        Err(LlmTransportError::new(
                            "TestProvider::complete was called without a test completion handler",
                        ))
                    })
                }),
            },
        }
    }

    pub fn kind(mut self, kind: &'static str) -> Self {
        self.provider.kind = kind;
        self
    }

    pub fn requires_streaming(mut self, requires_streaming: bool) -> Self {
        self.provider.requires_streaming = requires_streaming;
        self
    }

    pub fn options(mut self, options: ProviderOptions) -> Self {
        self.provider.options = options;
        self
    }

    pub fn serialize_config<F>(mut self, serialize_config: F) -> Self
    where
        F: Fn() -> serde_json::Value + Send + Sync + 'static,
    {
        self.provider.serialize_config = Arc::new(serialize_config);
        self
    }

    pub fn complete<F, Fut>(mut self, complete: F) -> Self
    where
        F: Fn(LlmRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<LlmResponse, LlmTransportError>> + Send + 'static,
    {
        self.provider.complete = Arc::new(move |request| Box::pin(complete(request)));
        self
    }

    pub fn complete_error(mut self, message: impl Into<String>) -> Self {
        let message = Arc::new(message.into());
        self.provider.complete = Arc::new(move |_request| {
            let message = Arc::clone(&message);
            Box::pin(async move { Err(LlmTransportError::new(message.as_str())) })
        });
        self
    }

    pub fn build(self) -> TestProvider {
        self.provider
    }
}

impl Default for TestProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for TestProvider {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        (self.serialize_config)()
    }

    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        (self.complete)(request).await
    }

    fn requires_streaming(&self) -> bool {
        self.requires_streaming
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// Build a `SessionPolicy` populated with the canonical stub provider
/// + model used by lash's in-tree tests.
pub fn mock_session_policy() -> SessionPolicy {
    SessionPolicy {
        provider_id: "stub".to_string(),
        model: ModelSpec::from_token_limits("mock-model", Default::default(), 200_000, None)
            .expect("valid mock model spec"),
        ..Default::default()
    }
}

/// A `ToolContext` backed by a default [`MockSessionManager`], suitable for
/// unit-testing a `ToolProvider` in isolation. Use [`mock_tool_context_with_host`]
/// when the tool under test interacts with session services and needs a
/// configured `MockSessionManager`.
pub fn mock_tool_context() -> crate::ToolContext<'static> {
    mock_tool_context_with_host(Arc::new(MockSessionManager::default()))
}

/// Like [`mock_tool_context`], but with the grant execution binding populated.
/// Use this for provider tests that need to assert grant-only routing behavior.
pub fn mock_tool_context_with_execution_binding(
    binding: serde_json::Value,
) -> crate::ToolContext<'static> {
    mock_tool_context().with_tool_execution_binding(binding)
}

/// Like [`mock_tool_context`], but lets the caller supply the host. Useful
/// when a tool reads from the host (snapshots, tool state, lifecycle hooks)
/// and the test wants to assert against captured interactions.
pub fn mock_tool_context_with_host<T>(host: Arc<T>) -> crate::ToolContext<'static>
where
    T: crate::plugin::SessionStateService
        + crate::plugin::SessionLifecycleService
        + crate::plugin::SessionGraphService
        + 'static,
{
    mock_tool_context_with_host_and_direct_completions(
        host,
        crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
    )
}

pub fn mock_tool_context_with_host_and_direct_completions<T>(
    host: Arc<T>,
    direct_completions: crate::DirectCompletionClient<'static>,
) -> crate::ToolContext<'static>
where
    T: crate::plugin::SessionStateService
        + crate::plugin::SessionLifecycleService
        + crate::plugin::SessionGraphService
        + 'static,
{
    let sessions: Arc<dyn crate::plugin::SessionStateService> = host.clone();
    let session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService> = host.clone();
    let session_graph: Arc<dyn crate::plugin::SessionGraphService> = host;
    crate::tool_provider::ToolContext::__for_testing(
        "test-session".to_string(),
        sessions,
        session_lifecycle,
        session_graph,
        Arc::new(crate::UnavailableProcessService),
        Arc::new(crate::SessionAttachmentStore::in_memory()),
        direct_completions,
        None,
    )
}

struct EmptyToolProvider;

#[async_trait::async_trait]
impl crate::ToolProvider for EmptyToolProvider {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        crate::ToolResult::err(serde_json::json!(format!(
            "test tool provider has no tool `{}`",
            call.name
        )))
    }
}

pub fn code_execution_context_with_tool_catalog(
    tool_catalog: crate::ToolCatalog,
) -> crate::RuntimeExecutionContext<'static> {
    code_execution_context_with_tool_provider_catalog_and_trigger_router(
        Arc::new(EmptyToolProvider),
        tool_catalog,
        None,
    )
}

pub fn code_execution_context_with_tool_provider_and_catalog(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
) -> crate::RuntimeExecutionContext<'static> {
    code_execution_context_with_tool_provider_catalog_and_trigger_router(
        provider,
        tool_catalog,
        None,
    )
}

fn code_execution_context_with_tool_catalog_and_trigger_router(
    tool_catalog: crate::ToolCatalog,
    trigger_router: Option<crate::TriggerRouter>,
) -> crate::RuntimeExecutionContext<'static> {
    code_execution_context_with_tool_provider_catalog_and_trigger_router(
        Arc::new(EmptyToolProvider),
        tool_catalog,
        trigger_router,
    )
}

fn code_execution_context_with_tool_provider_catalog_and_trigger_router(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    trigger_router: Option<crate::TriggerRouter>,
) -> crate::RuntimeExecutionContext<'static> {
    code_execution_context_with_tool_provider_catalog_trigger_router_and_effect_controller(
        provider,
        tool_catalog,
        trigger_router,
        Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        ),
    )
}

fn code_execution_context_with_tool_provider_catalog_trigger_router_and_effect_controller(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    trigger_router: Option<crate::TriggerRouter>,
    effect_controller: Arc<dyn crate::RuntimeEffectController>,
) -> crate::RuntimeExecutionContext<'static> {
    let plugins = crate::plugin::PluginHost::new(test_code_protocol_factories())
        .build_session("test-session", None)
        .expect("test plugin session");
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
    let execution_env_spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::default(),
    );
    let attachment_store: Arc<crate::SessionAttachmentStore> =
        Arc::new(crate::SessionAttachmentStore::in_memory());
    let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
        plugins,
        tools: provider,
        tool_catalog: Arc::new(tool_catalog),
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        process_cancel_ability: Arc::new(crate::DefaultProcessCancelAbility),
        trigger_router,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(effect_controller),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: execution_env_spec.clone(),
        session_id: "test-session".to_string(),
        agent_frame_id: String::new(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::clone(&attachment_store),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    });
    crate::RuntimeExecutionContext::new(
        "test-session".to_string(),
        dispatch,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    )
    .with_execution_env_spec(execution_env_spec)
}

pub fn code_execution_context() -> crate::RuntimeExecutionContext<'static> {
    code_execution_context_with_tool_catalog(crate::ToolCatalog::from_tool_definitions(Vec::new()))
}

/// Build an empty code-execution context carrying the stable parent invocation
/// that production installs around an `ExecCode` effect.
pub fn code_execution_context_with_invocation(
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'static> {
    code_execution_context().with_parent_invocation(invocation)
}

/// Build the stable invocation installed around an `ExecCode` effect.
pub fn exec_code_invocation(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: impl Into<String>,
    replay_key: impl Into<String>,
) -> crate::RuntimeInvocation {
    crate::RuntimeInvocation::effect(
        crate::RuntimeScope::for_turn(session_id, turn_id, turn_index, protocol_iteration),
        effect_id,
        crate::RuntimeEffectKind::ExecCode,
        replay_key,
    )
}

pub fn code_execution_context_with_trigger_store(
    trigger_store: Arc<dyn crate::TriggerStore>,
) -> crate::RuntimeExecutionContext<'static> {
    code_execution_context_with_tool_catalog_and_trigger_router(
        crate::ToolCatalog::from_tool_definitions(Vec::new()),
        Some(crate::TriggerRouter::new(trigger_store, None, None)),
    )
}

/// Build a code-execution context whose trigger operations pass through the
/// supplied effect controller before reaching `trigger_store`.
pub fn code_execution_context_with_trigger_store_and_effect_controller(
    trigger_store: Arc<dyn crate::TriggerStore>,
    effect_controller: Arc<dyn crate::RuntimeEffectController>,
) -> crate::RuntimeExecutionContext<'static> {
    code_execution_context_with_tool_provider_catalog_trigger_router_and_effect_controller(
        Arc::new(EmptyToolProvider),
        crate::ToolCatalog::from_tool_definitions(Vec::new()),
        Some(crate::TriggerRouter::new(trigger_store, None, None)),
        effect_controller,
    )
}

/// Convenience helper for the common tool-test shape: build a
/// [`mock_tool_context`], wrap `name` + `args` in a `ToolCall`, and `await`
/// the provider's `execute`. Use this for unit tests that don't need to
/// inspect host interactions; call `mock_tool_context()` directly and
/// construct `ToolCall` manually for more involved scenarios.
pub async fn run_tool<P>(tool: &P, name: &str, args: &serde_json::Value) -> crate::ToolResult
where
    P: crate::ToolProvider + ?Sized,
{
    let context = mock_tool_context();
    tool.execute(crate::ToolCall {
        name,
        args,
        context: &context,
        progress: None,
    })
    .await
}

/// Build an empty `AssembledTurn` whose assistant text is `summary`.
pub fn mock_assembled_turn(session_id: &str, summary: &str) -> AssembledTurn {
    AssembledTurn {
        state: SessionSnapshot {
            session_id: session_id.to_string(),
            policy: SessionPolicy::default(),
            ..Default::default()
        },
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: summary.to_string(),
        }),
        cancellation: None,
        assistant_output: AssistantOutput {
            safe_text: summary.to_string(),
            raw_text: summary.to_string(),
            state: OutputState::Usable,
        },
        execution: ExecutionSummary::default(),
        token_usage: TokenUsage::default(),
        children_usage: Vec::new(),
        llm_calls: Vec::new(),
        tool_calls: Vec::new(),
        errors: Vec::new(),
    }
}

/// Configurable mock for host capability traits. Tests override
/// the snapshot, tool catalog, and turn outcome via the builder
/// methods; mutations (`create_session`, `close_session`)
/// are recorded so tests can assert against them.
pub type RecordedSessionTurn = (String, String, Option<String>, crate::ExecutionScope);

pub struct MockSessionManager {
    pub snapshot: SessionSnapshot,
    pub tool_catalog: Vec<serde_json::Value>,
    pub turn: AssembledTurn,
    pub tool_registry: Option<crate::ToolRegistry>,
    pub process_registry: Arc<crate::TestLocalProcessRegistry>,
    pub created: Mutex<Vec<SessionCreateRequest>>,
    pub closed: Mutex<Vec<String>>,
    pub turns: Mutex<Vec<RecordedSessionTurn>>,
}

impl Default for MockSessionManager {
    fn default() -> Self {
        Self {
            snapshot: RuntimeSessionState::default().to_snapshot(),
            tool_catalog: Vec::new(),
            turn: mock_assembled_turn("root", ""),
            tool_registry: None,
            process_registry: Arc::new(crate::TestLocalProcessRegistry::default()),
            created: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            turns: Mutex::new(Vec::new()),
        }
    }
}

impl MockSessionManager {
    #[allow(dead_code)]
    pub fn with_snapshot(mut self, snapshot: SessionSnapshot) -> Self {
        self.snapshot = snapshot;
        self
    }

    pub fn with_tool_catalog(mut self, catalog: Vec<serde_json::Value>) -> Self {
        self.tool_catalog = catalog;
        self
    }

    pub fn with_turn(mut self, turn: AssembledTurn) -> Self {
        self.turn = turn;
        self
    }

    #[allow(dead_code)]
    pub fn with_tool_registry(mut self, tool_registry: crate::ToolRegistry) -> Self {
        self.tool_registry = Some(tool_registry);
        self
    }

    /// Snapshot of the requests captured by `create_session`. Panics if
    /// the lock is poisoned (a panic from another test thread).
    pub fn created_snapshot(&self) -> Vec<SessionCreateRequest> {
        self.created.lock().expect("created lock").clone()
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionStateService for MockSessionManager {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }

    async fn snapshot_session(&self, _session_id: &str) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }
    async fn tool_catalog(&self, _session_id: &str) -> Result<Vec<serde_json::Value>, PluginError> {
        Ok(self.tool_catalog.clone())
    }
    async fn tool_state(&self, _session_id: &str) -> Result<crate::ToolState, PluginError> {
        self.tool_registry
            .as_ref()
            .map(crate::ToolRegistry::export_state)
            .ok_or_else(|| {
                PluginError::Session("tool state is unavailable in this session".to_string())
            })
    }

    async fn apply_tool_state(
        &self,
        _session_id: &str,
        snapshot: crate::ToolState,
    ) -> Result<u64, PluginError> {
        let Some(tool_registry) = self.tool_registry.as_ref() else {
            return Err(PluginError::Session(
                "tool state mutation is unavailable in this session".to_string(),
            ));
        };
        tool_registry
            .apply_state(snapshot)
            .map_err(|err| PluginError::Session(err.to_string()))
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionLifecycleService for MockSessionManager {
    async fn create_session(
        &self,
        request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        self.created
            .lock()
            .expect("created lock")
            .push(request.clone());
        Ok(SessionHandle {
            session_id: request
                .session_id
                .clone()
                .unwrap_or_else(|| "child".to_string()),
            parent_session_id: request.relation.parent_session_id().map(ToOwned::to_owned),
            policy: request.policy.unwrap_or_else(mock_session_policy),
        })
    }

    async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
        self.closed
            .lock()
            .expect("closed lock")
            .push(session_id.to_string());
        Ok(())
    }
    async fn start_turn(
        &self,
        request: crate::SessionTurnRequest<'_>,
    ) -> Result<AssembledTurn, PluginError> {
        let (turn, scoped_effect_controller) = request.into_parts();
        self.turns.lock().expect("turns lock").push((
            turn.session_id,
            turn.turn_id,
            turn.input.trace_turn_id,
            scoped_effect_controller.execution_scope().clone(),
        ));
        Ok(self.turn.clone())
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionGraphService for MockSessionManager {}

#[async_trait::async_trait]
impl crate::ProcessService for MockSessionManager {
    async fn start(
        &self,
        session_id: &str,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        let id = registration.id.clone();
        // This mock stands in as the executor, so it completes the row under the
        // authority its declared disposition permits: externally-owned rows close
        // via their external owner, lash-executed rows via the workflow-key path.
        let authority = if registration.disposition == crate::RecoveryDisposition::ExternallyOwned {
            crate::ProcessCompletionAuthority::external_owner()
        } else {
            crate::ProcessCompletionAuthority::workflow_key(&id)
        };
        self.process_registry.register_process(registration).await?;
        if let Some(descriptor) = options.descriptor {
            let session_scope = crate::SessionScope::new(session_id);
            self.process_registry
                .grant_handle(&session_scope, &id, descriptor)
                .await?;
        }
        self.process_registry
            .complete_process(
                &id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!({
                        "state": "completed"
                    }),
                )),
                authority,
            )
            .await
            .map(crate::ProcessCompletionOutcome::into_record)
    }

    async fn await_process(
        &self,
        process_id: &str,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, PluginError> {
        let registry: Arc<dyn crate::ProcessRegistry> = self.process_registry.clone();
        crate::ProcessAwaiter::polling(registry)
            .await_terminal(process_id)
            .await
    }

    async fn list_visible(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::runtime::ProcessHandleGrantEntry>, PluginError> {
        let session_scope = scope
            .agent_frame_id
            .as_deref()
            .map(|frame_id| crate::SessionScope::for_agent_frame(session_id, frame_id))
            .unwrap_or_else(|| crate::SessionScope::new(session_id));
        match mode {
            crate::ProcessListMode::Live => {
                self.process_registry
                    .list_live_handle_grants(&session_scope)
                    .await
            }
            crate::ProcessListMode::All => {
                self.process_registry
                    .list_handle_grants(&session_scope)
                    .await
            }
        }
    }

    async fn validate_visible(
        &self,
        session_id: &str,
        handle_ids: &[String],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let session_scope = scope
            .agent_frame_id
            .as_deref()
            .map(|frame_id| crate::SessionScope::for_agent_frame(session_id, frame_id))
            .unwrap_or_else(|| crate::SessionScope::new(session_id));
        for handle_id in handle_ids {
            if !self
                .process_registry
                .has_handle_grant(&session_scope, handle_id)
                .await?
            {
                return Err(PluginError::Session(format!(
                    "process handle `{handle_id}` is not live or visible in this session"
                )));
            }
        }
        Ok(())
    }

    async fn cancel(
        &self,
        _session_id: &str,
        process_id: &str,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        crate::InlineRuntimeEffectController::request_process_cancel(
            self.process_registry.clone(),
            process_id,
            Some("requested by test".to_string()),
        )
        .await
    }

    async fn signal(
        &self,
        _session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let event_type = crate::process_signal_event_type(&signal_name)?;
        self.process_registry
            .append_event(
                process_id,
                crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
                    format!("process:{process_id}:signal.{signal_name}:{signal_id}"),
                ),
            )
            .await
            .map(|result| result.event)
    }

    async fn transfer(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: Vec<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let from_scope = scope
            .agent_frame_id
            .as_deref()
            .map(|frame_id| crate::SessionScope::for_agent_frame(from_session_id, frame_id))
            .unwrap_or_else(|| crate::SessionScope::new(from_session_id));
        let to_scope = crate::SessionScope::new(to_session_id);
        self.process_registry
            .transfer_handle_grants(&from_scope, &to_scope, &process_ids)
            .await
    }
}
// ─────────────────────────────────────────────────────────────────────
// Test protocol plugin fakes.
//
// Exposed publicly under the `testing` feature so downstream plugin
// crates can wire a minimal fake protocol plugin into integration tests
// without depending on concrete protocol crates.
// ─────────────────────────────────────────────────────────────────────
pub use test_protocol_fakes::{test_code_protocol_factories, test_standard_protocol_factories};

mod test_protocol_fakes {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::plugin::{
        PluginFactory, PluginRegistrar, PluginSessionContext, ProtocolDriverPlugin,
        ProtocolRuntimeContext, ProtocolSessionContext, ProtocolSessionPlugin, SessionPlugin,
    };
    use crate::sansio::{
        CompletedToolCall, ProtocolDriverHandle, WaitingExecState, WaitingLlmState,
    };
    use crate::{
        DriverAction, DriverContextView, ExecResponse, ProtocolBuildInput, TurnDriverConfig,
        TurnDriverPreamble,
    };
    use lash_sansio::llm::types::LlmResponse;

    pub fn test_standard_protocol_factories() -> Vec<Arc<dyn PluginFactory>> {
        vec![Arc::new(TestProtocolFactory {
            id: "protocol_standard",
            include_batch: true,
            decode_code_create_options: false,
        })]
    }

    pub fn test_code_protocol_factories() -> Vec<Arc<dyn PluginFactory>> {
        vec![Arc::new(TestProtocolFactory {
            id: "protocol_code",
            include_batch: false,
            decode_code_create_options: true,
        })]
    }

    struct TestProtocolFactory {
        id: &'static str,
        include_batch: bool,
        decode_code_create_options: bool,
    }

    impl PluginFactory for TestProtocolFactory {
        fn id(&self) -> &'static str {
            self.id
        }

        fn build(
            &self,
            _ctx: &PluginSessionContext,
        ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            Ok(Arc::new(TestProtocolPlugin {
                id: self.id,
                include_batch: self.include_batch,
                decode_code_create_options: self.decode_code_create_options,
            }))
        }
    }

    struct TestProtocolPlugin {
        id: &'static str,
        include_batch: bool,
        decode_code_create_options: bool,
    }

    impl SessionPlugin for TestProtocolPlugin {
        fn id(&self) -> &'static str {
            self.id
        }

        fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            reg.protocol().session(Arc::new(TestProtocolSession {
                decode_code_create_options: self.decode_code_create_options,
            }))?;
            if self.include_batch {
                reg.tools().provider(Arc::new(TestProtocolTools))?;
            }
            reg.protocol()
                .protocol_driver(Arc::new(TestProtocolDriver))?;
            Ok(())
        }
    }

    struct TestProtocolSession {
        decode_code_create_options: bool,
    }

    #[async_trait]
    impl ProtocolSessionPlugin for TestProtocolSession {
        async fn initialize_session(
            &self,
            _ctx: ProtocolSessionContext<'_>,
        ) -> Result<(), crate::SessionError> {
            Ok(())
        }

        fn configure_runtime_on_materialize(
            &self,
            mut ctx: ProtocolRuntimeContext<'_>,
            materialization: crate::plugin::ProtocolSessionMaterialization<'_>,
        ) -> Result<(), crate::SessionError> {
            if !self.decode_code_create_options {
                return Ok(());
            }
            if let Some(extras) = materialization
                .plugin_options
                .decode::<TestCodeCreateExtras>("code_protocol")
                .map_err(|err| {
                    crate::SessionError::Protocol(format!(
                        "invalid test code create options: {err}"
                    ))
                })?
            {
                let options = crate::ProtocolTurnOptions::typed(extras)?;
                ctx.set_protocol_turn_options(options);
            }
            Ok(())
        }
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    #[serde(default, deny_unknown_fields)]
    struct TestCodeCreateExtras {
        termination: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_answer_format: Option<serde_json::Value>,
    }

    impl Default for TestCodeCreateExtras {
        fn default() -> Self {
            Self {
                termination: default_test_code_termination(),
                final_answer_format: None,
            }
        }
    }

    fn default_test_code_termination() -> serde_json::Value {
        serde_json::json!({
            "kind": "finish_required",
            "schema": null,
        })
    }

    struct TestProtocolTools;

    #[async_trait]
    impl crate::ToolProvider for TestProtocolTools {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![test_batch_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "batch").then(|| Arc::new(test_batch_tool_definition().contract()))
        }

        async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
            match call.name {
                "batch" => execute_test_batch(call.context, call.args).await,
                _ => crate::ToolResult::err_fmt(format_args!("Unknown tool: {}", call.name)),
            }
        }
    }

    /// Minimal `batch` tool definition used by lash's own tests. Mirrors the
    /// standard protocol plugin's batch schema, but lives here so lash's tests
    /// don't need a dev-dep on that plugin crate.
    fn test_batch_tool_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:batch",
            "batch",
            "Execute up to 25 independent tool calls concurrently.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_calls": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 25,
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": { "type": "string" },
                                "parameters": { "type": "object", "additionalProperties": true }
                            },
                            "required": ["tool", "parameters"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["tool_calls"],
                "additionalProperties": false,
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
    }

    /// Minimal batch executor used by lash's own tests (mirrors the
    /// behavior of `lash-protocol-standard`'s `execute_batch_tool_call`).
    async fn execute_test_batch(
        context: &crate::ToolContext<'_>,
        args: &serde_json::Value,
    ) -> crate::ToolResult {
        const MAX: usize = 25;
        let Some(raw_calls) = args.get("tool_calls").and_then(|v| v.as_array()) else {
            return crate::ToolResult::err_fmt("Missing required parameter: tool_calls");
        };
        if raw_calls.is_empty() {
            return crate::ToolResult::err_fmt("Invalid tool_calls: expected at least one call");
        }

        let mut results = Vec::new();
        let mut parallel_specs = Vec::new();
        let dispatch = context.dispatch();
        for (index, item) in raw_calls.iter().enumerate().take(MAX) {
            let Some(obj) = item.as_object() else {
                return crate::ToolResult::err_fmt(format_args!(
                    "Invalid tool_calls[{index}]: expected object with tool and parameters"
                ));
            };
            let Some(tool) = obj
                .get("tool")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                return crate::ToolResult::err_fmt(format_args!(
                    "Invalid tool_calls[{index}].tool: expected non-empty string"
                ));
            };
            if tool == "batch" {
                results.push(serde_json::json!({
                    "index": index,
                    "tool": tool,
                    "success": false,
                    "duration_ms": 0,
                    "error": "Tool 'batch' is not allowed inside batch",
                }));
                continue;
            }
            let parameters = obj
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let Some(manifest) = dispatch.callable_tool_manifest(tool) else {
                results.push(serde_json::json!({
                    "index": index,
                    "tool": tool,
                    "success": false,
                    "duration_ms": 0,
                    "error": format!("Tool '{tool}' is unavailable in this session"),
                }));
                continue;
            };
            parallel_specs.push((
                index,
                crate::ToolInvocation::new(format!("test-batch:{index}"), manifest.id, parameters),
            ));
        }

        let outcomes = dispatch
            .batch(
                parallel_specs
                    .iter()
                    .map(|(_, invocation)| invocation.clone())
                    .collect(),
            )
            .await;
        for ((index, invocation), outcome) in parallel_specs.into_iter().zip(outcomes) {
            let tool_label = invocation.label();
            let tool_record = outcome.record.unwrap_or(crate::ToolCallRecord {
                call_id: Some(invocation.id),
                tool: tool_label,
                args: invocation.args,
                output: outcome.output,
                duration_ms: 0,
            });
            let mut record = serde_json::Map::new();
            record.insert("index".to_string(), serde_json::json!(index));
            record.insert("tool".to_string(), serde_json::json!(tool_record.tool));
            record.insert(
                "success".to_string(),
                serde_json::json!(tool_record.output.is_success()),
            );
            record.insert(
                "duration_ms".to_string(),
                serde_json::json!(tool_record.duration_ms),
            );
            record.insert(
                if tool_record.output.is_success() {
                    "result"
                } else {
                    "error"
                }
                .to_string(),
                tool_record.output.value_for_projection(),
            );
            results.push(serde_json::Value::Object(record));
        }

        for overflow_index in MAX..raw_calls.len() {
            results.push(serde_json::json!({
                "index": overflow_index,
                "tool": raw_calls
                    .get(overflow_index)
                    .and_then(|item| item.get("tool"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown"),
                "success": false,
                "duration_ms": 0,
                "error": "Maximum of 25 tool calls allowed in batch",
            }));
        }

        results.sort_by_key(|r| {
            r.get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(u64::MAX)
        });
        crate::ToolResult::ok(serde_json::json!({ "results": results }))
    }

    struct TestProtocolDriver;

    impl ProtocolDriverPlugin for TestProtocolDriver {
        fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
            let tool_names = input.tool_catalog.tool_names();
            let tool_names_fingerprint = input.tool_catalog.tool_names_fingerprint();
            TurnDriverPreamble {
                config: TurnDriverConfig::chat(
                    Arc::new(TestDriver),
                    false,
                    Arc::new(test_turn_limit_final_message),
                ),
                tool_specs: input.tool_catalog.model_tool_specs(),
                tool_names,
                tool_names_fingerprint,
                execution_prompt: Arc::from(""),
                prompt_contributions: input.extra_prompt_contributions,
            }
        }
    }

    fn test_turn_limit_final_message(message_id: String, max_turns: usize) -> crate::Message {
        crate::Message {
            id: message_id.clone(),
            role: crate::MessageRole::System,
            parts: crate::shared_parts(vec![crate::Part {
                id: format!("{message_id}.p0"),
                kind: crate::PartKind::Error,
                content: format!("Turn limit reached ({max_turns}) before a final test response."),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: crate::PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        }
    }

    /// Minimal Standard-style driver used by lash's own test suite. Mirrors
    /// the parts of the real `lash-protocol-standard::StandardDriver` that
    /// production tests depend on: extract tool calls + assistant text from
    /// the LLM response, append the assistant message, dispatch tools, and
    /// finish-checkpoint when there are no tools. Reasoning parts are
    /// surfaced but without the interleave ordering the real driver uses —
    /// no test asserts that ordering.
    struct TestDriver;

    impl ProtocolDriverHandle<crate::HostTurnProtocol> for TestDriver {
        fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
            vec![DriverAction::StartLlm {
                request: ctx.project_llm_request(true),
                driver_state: None,
            }]
        }

        fn handle_llm_success(
            &self,
            ctx: DriverContextView<'_>,
            _waiting: WaitingLlmState<crate::HostTurnProtocol>,
            llm_response: LlmResponse,
            text_streamed: bool,
        ) -> Vec<DriverAction> {
            use crate::sansio::{CheckpointResumeAction, PendingToolCall};
            use crate::{
                CheckpointKind, Message, MessageRole, Part, PartKind, PruneState,
                SessionStreamEvent,
            };
            use lash_sansio::llm::types::LlmOutputPart;
            use lash_sansio::session_model::make_error_event;

            let parts = crate::normalized_response_parts(&llm_response);
            let mut assistant_text = String::new();
            let mut tool_calls: Vec<(
                String,
                String,
                String,
                Option<lash_sansio::llm::types::ProviderReplayMeta>,
            )> = Vec::new();
            let mut actions = Vec::new();

            for part in parts {
                match part {
                    LlmOutputPart::Text { text, .. } => {
                        if !text.is_empty() {
                            let previous_len = assistant_text.len();
                            crate::append_assistant_text_part(&mut assistant_text, &text);
                            if !text_streamed {
                                actions.push(DriverAction::Emit(SessionStreamEvent::TextDelta {
                                    content: assistant_text[previous_len..].to_string(),
                                }));
                            }
                        }
                    }
                    LlmOutputPart::Reasoning { .. } => {}
                    LlmOutputPart::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                        replay,
                    } => {
                        tool_calls.push((call_id, tool_name, input_json, replay));
                    }
                }
            }

            actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
                protocol_iteration: ctx.protocol_iteration(),
                content: assistant_text.clone(),
                duration_ms: 0,
            }));

            if tool_calls.is_empty() {
                if assistant_text.trim().is_empty() {
                    actions.push(DriverAction::Emit(make_error_event(
                        "llm_provider",
                        Some("empty_response"),
                        "Model returned no assistant text or tool calls.",
                        None,
                    )));
                    actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                        TurnStop::ProviderError,
                    )));
                    return actions;
                }
                let asst_id = format!(
                    "m_standard_{}_{}_assistant",
                    ctx.turn_id(),
                    ctx.protocol_iteration()
                );
                let outcome_text = assistant_text.clone();
                let parts_out = vec![Part {
                    id: format!("{asst_id}.p0"),
                    kind: PartKind::Prose,
                    content: assistant_text,
                    attachment: None,
                    tool_call_id: None,
                    tool_name: None,
                    tool_replay: None,
                    prune_state: PruneState::Intact,
                    reasoning_meta: None,
                    response_meta: None,
                }];
                actions.push(DriverAction::AppendEvents(vec![
                    SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
                        id: asst_id,
                        role: MessageRole::Assistant,
                        parts: lash_sansio::shared_parts(parts_out),
                        origin: None,
                    })),
                ]));
                actions.push(DriverAction::StartCheckpoint {
                    checkpoint: CheckpointKind::BeforeCompletion,
                    on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                        TurnFinish::AssistantMessage { text: outcome_text },
                    )),
                });
                return actions;
            }

            let asst_id = format!(
                "m_standard_{}_{}_assistant",
                ctx.turn_id(),
                ctx.protocol_iteration()
            );
            let mut assistant_parts = Vec::new();
            if !assistant_text.trim().is_empty() {
                assistant_parts.push(Part {
                    id: format!("{}.p{}", asst_id, assistant_parts.len()),
                    kind: PartKind::Prose,
                    content: assistant_text,
                    attachment: None,
                    tool_call_id: None,
                    tool_name: None,
                    tool_replay: None,
                    prune_state: PruneState::Intact,
                    reasoning_meta: None,
                    response_meta: None,
                });
            }
            let mut calls = Vec::new();
            for (call_id, tool_name, input_json, replay) in tool_calls {
                assistant_parts.push(Part {
                    id: format!("{}.p{}", asst_id, assistant_parts.len()),
                    kind: PartKind::ToolCall,
                    content: input_json.clone(),
                    attachment: None,
                    tool_call_id: Some(call_id.clone()),
                    tool_name: Some(tool_name.clone()),
                    tool_replay: replay.clone(),
                    prune_state: PruneState::Intact,
                    reasoning_meta: None,
                    response_meta: None,
                });
                let args = serde_json::from_str::<serde_json::Value>(&input_json)
                    .unwrap_or_else(|_| serde_json::json!({}));
                calls.push(PendingToolCall {
                    call_id,
                    tool_name,
                    args,
                    replay,
                });
            }
            if !assistant_parts.is_empty() {
                actions.push(DriverAction::AppendEvents(vec![
                    SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
                        id: asst_id,
                        role: MessageRole::Assistant,
                        parts: lash_sansio::shared_parts(assistant_parts),
                        origin: None,
                    })),
                ]));
            }
            actions.push(DriverAction::StartTools { calls });
            actions
        }

        fn handle_tool_results(
            &self,
            ctx: DriverContextView<'_>,
            completed: Vec<CompletedToolCall>,
        ) -> Vec<DriverAction> {
            use crate::sansio::CheckpointResumeAction;
            use crate::{
                CheckpointKind, Message, MessageRole, Part, PartKind, PruneState,
                SessionStreamEvent,
            };
            use lash_sansio::session_model::reassign_part_ids;
            let mut actions = Vec::new();
            let mut result_parts = Vec::new();
            let mut terminal_outcome = None;
            for outcome in completed {
                if terminal_outcome.is_none() && outcome.output.is_success() {
                    terminal_outcome = match outcome.output.control.as_ref() {
                        Some(crate::ToolControl::SwitchAgentFrame {
                            frame_id,
                            initial_nodes,
                            task: Some(task),
                        }) if !frame_id.trim().is_empty() && !task.trim().is_empty() => {
                            Some(TurnOutcome::AgentFrameSwitch {
                                frame_id: frame_id.clone(),
                                task: task.clone(),
                                initial_nodes: initial_nodes.clone(),
                            })
                        }
                        Some(crate::ToolControl::Finish { value }) => {
                            Some(TurnOutcome::Finished(TurnFinish::ToolValue {
                                tool_name: outcome.tool_name.clone(),
                                value: value.to_json_value(),
                            }))
                        }
                        Some(crate::ToolControl::Fail { failure }) => {
                            Some(TurnOutcome::Stopped(TurnStop::ToolError {
                                tool_name: outcome.tool_name.clone(),
                                value: failure.to_json_value(),
                            }))
                        }
                        _ => None,
                    };
                }
                for part in &outcome.model_return.parts {
                    match part {
                        lash_sansio::ModelToolReturnPart::Text { text } => {
                            if text.is_empty() {
                                continue;
                            }
                            result_parts.push(Part {
                                id: String::new(),
                                kind: PartKind::ToolResult,
                                content: text.clone(),
                                attachment: None,
                                tool_call_id: Some(outcome.call_id.clone()),
                                tool_name: Some(outcome.tool_name.clone()),
                                tool_replay: None,
                                prune_state: PruneState::Intact,
                                reasoning_meta: None,
                                response_meta: None,
                            });
                        }
                        lash_sansio::ModelToolReturnPart::Attachment(source) => {
                            result_parts.push(Part {
                                id: String::new(),
                                kind: PartKind::Attachment,
                                content: String::new(),
                                attachment: Some(lash_sansio::PartAttachment {
                                    source: source.clone(),
                                }),
                                tool_call_id: Some(outcome.call_id.clone()),
                                tool_name: Some(outcome.tool_name.clone()),
                                tool_replay: None,
                                prune_state: PruneState::Intact,
                                reasoning_meta: None,
                                response_meta: None,
                            });
                        }
                    }
                }
            }
            if !result_parts.is_empty() {
                let user_id = format!(
                    "m_standard_{}_{}_tool_results",
                    ctx.turn_id(),
                    ctx.protocol_iteration()
                );
                reassign_part_ids(&user_id, &mut result_parts);
                actions.push(DriverAction::AppendEvents(vec![
                    SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
                        id: user_id,
                        role: MessageRole::User,
                        parts: lash_sansio::shared_parts(result_parts),
                        origin: None,
                    })),
                ]));
            }
            if let Some(outcome) = terminal_outcome {
                actions.push(DriverAction::Finish(outcome));
                return actions;
            }
            actions.push(DriverAction::AdvanceProtocolIteration);
            let next_protocol_iteration = ctx.protocol_iteration() + 1;
            if let Some(max_turns) = ctx.max_turns()
                && next_protocol_iteration >= ctx.protocol_run_offset() + max_turns
            {
                let message_id = format!(
                    "m_standard_{}_{next_protocol_iteration}_turn_limit",
                    ctx.turn_id()
                );
                actions.push(DriverAction::AppendEvents(vec![
                    SessionHistoryRecord::Conversation(ConversationRecord::from_message(
                        test_turn_limit_final_message(message_id, max_turns),
                    )),
                ]));
                actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                    TurnStop::MaxTurns,
                )));
                let _ = SessionStreamEvent::Done;
                return actions;
            }
            actions.push(DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::AfterWork,
                on_empty: CheckpointResumeAction::PrepareIteration,
            });
            actions
        }

        fn handle_exec_result(
            &self,
            _ctx: DriverContextView<'_>,
            _waiting: WaitingExecState<crate::HostTurnProtocol>,
            _result: Result<ExecResponse, String>,
        ) -> Vec<DriverAction> {
            Vec::new()
        }
    }
} // mod test_protocol_fakes
