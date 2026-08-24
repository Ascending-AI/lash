//! In-tree test fixtures shared across the lash crate's test modules.
//!
//! Cuts down on per-test-module `MockSessionManager` boilerplate by
//! providing a configurable mock implementation plus a couple of small
//! builders for common policy / turn fixtures.

use lash_sansio::sync::MutexExt;
// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and
// rustdoc then resolves the whole merged doc — including the submodule's own
// intra-doc links — against *this* module's scope, where none of the linked
// items exist.
pub mod attempt_sentinel;
pub mod behavior_transcript;
pub mod checkpoint_observer;
pub mod conformance;
mod execution_context_builder;
mod live_replay;
pub mod sansio_transcript;

pub(crate) use execution_context_builder::*;

use crate::facade_support::ScopedEffectControllerFacadeOps;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::llm::transport::LlmTransportError;
use crate::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent};
use crate::plugin::{PluginError, SessionCreateRequest, SessionHandle, SessionSnapshot};
use crate::provider::{Provider, ProviderComponents, ProviderHandle};
use crate::session_model::{ConversationRecord, SessionHistoryRecord};
use crate::{
    AssembledTurn, AssistantOutput, ModelSpec, OutputState, ProcessRegistry, ProviderOptions,
    RuntimeSessionState, SessionPolicy, TokenUsage, TurnExecutionMetrics, TurnFinish, TurnOutcome,
    TurnStop,
};

/// Generous claim bounds for store/runtime conformance tests whose subject is
/// not batching policy. Batching-specific tests construct exact policies.
///
/// The drain policy is deliberately [`DrainMode::All`](crate::DrainMode::All)
/// rather than the shipped one-row default: these suites exercise the store's
/// coalescing laws, and a one-row drain would hide them. Tests whose subject is
/// the drain policy itself set it explicitly — including
/// `queued_work_redrive_ignores_a_changed_drain_policy`, which pins the shipped
/// default on the successor. This pin cannot mask an exact-selection defect:
/// exact claims bypass the configured policy entirely
/// ([`select_exact_turn_work_claim_prefix`](crate::store::queued_work::select_exact_turn_work_claim_prefix)).
pub fn queued_work_claim_policy(max_rows: usize) -> crate::QueuedWorkClaimPolicy {
    crate::QueuedWorkClaimPolicy {
        max_context_tokens: usize::MAX / 4,
        action_token_reserve: 1,
        max_rows,
        max_pending_age_ms: u64::MAX,
        drain_policy: crate::runtime::shared_drain_mode_policy(crate::DrainMode::All),
    }
}

/// Fresh test executor host identity used by runtime construction.
///
/// Each call represents a distinct boot/executor recovery attempt so crash
/// matrices cannot accidentally reenter a predecessor's lease.
pub fn runtime_lease_owner() -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(
        "lash-core-test-worker",
        format!("lash-core-test-boot:{}", uuid::Uuid::new_v4()),
    )
}

/// Synthesize the response produced when a plugin aborts an in-flight LLM
/// stream after `events` have reached core's stream accumulator.
///
/// This test seam deliberately uses the production accumulator and its
/// empty-response branch: on the abort path, those accumulated parts are the
/// whole response rather than gap-fill input for a provider completion.
pub fn response_synthesized_from_aborted_stream(events: &[LlmStreamEvent]) -> LlmResponse {
    crate::runtime::response_synthesized_from_aborted_stream(events)
}

/// Controllable epoch clock shared by store and runtime conformance tests.
#[derive(Debug)]
pub struct TestClock(std::sync::atomic::AtomicU64);

impl TestClock {
    pub fn new(timestamp_ms: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(timestamp_ms))
    }

    pub fn advance(&self, duration_ms: u64) {
        self.0
            .fetch_add(duration_ms, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set(&self, timestamp_ms: u64) {
        self.0
            .store(timestamp_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::Clock for TestClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.timestamp_ms()),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

/// Production-equivalent logical payload accounting for one runtime commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeCommitBudgetMeasurement {
    /// Graph-node rows written by the commit.
    pub graph_rows: usize,
    /// Attachment-manifest rows stamped as adopted by the commit.
    pub adopted_intent_rows: usize,
    /// Saturating sum of graph and attachment-adoption rows.
    pub total_rows: usize,
    /// Persisted JSON encoding of the session configuration, including prompt.
    pub session_config_bytes: usize,
    /// Sum of the persisted JSON encoding of each graph node.
    pub graph_delta_bytes: usize,
    /// Named-MessagePack size of the hydrated checkpoint.
    pub checkpoint_bytes: usize,
    /// Raw UTF-8 byte length of the committed attachment ids.
    pub attachment_manifest_bytes: usize,
    /// Saturating sum of the four budgeted components.
    pub total_bytes: usize,
}

/// Measure a commit with the exact accounting used by
/// [`crate::RuntimeCommit::validate_budget`].
pub fn measure_runtime_commit_budget(
    commit: &crate::RuntimeCommit,
) -> Result<RuntimeCommitBudgetMeasurement, crate::StoreError> {
    let measurement = commit.measure_budget()?;
    Ok(RuntimeCommitBudgetMeasurement {
        graph_rows: measurement.graph_rows,
        adopted_intent_rows: measurement.adopted_intent_rows,
        total_rows: measurement.total_rows,
        session_config_bytes: measurement.session_config_bytes,
        graph_delta_bytes: measurement.graph_delta_bytes,
        checkpoint_bytes: measurement.checkpoint_bytes,
        attachment_manifest_bytes: measurement.attachment_manifest_bytes,
        total_bytes: measurement.total_bytes,
    })
}

/// Stage a protocol-owned execution-state root and its complete keyed leaf set
/// into resident session state, exactly as the runtime's turn boundary does.
///
/// The staging mutator itself is runtime-owned (ADR 0051's "neither" class), so
/// protocol crates reach it through this fixture rather than through
/// `RuntimeSessionState`'s public surface.
pub fn stage_execution_state_components(
    state: &mut RuntimeSessionState,
    snapshot: crate::plugin::ExecutionStateSnapshot,
) -> Result<(), crate::StoreError> {
    state.set_execution_state_components(snapshot)
}

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
    generation_retry_guarantee: crate::provider::GenerationRetryGuarantee,
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
                generation_retry_guarantee: crate::provider::GenerationRetryGuarantee::None,
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

    #[cfg(test)]
    pub(crate) fn generation_retry_guarantee(
        mut self,
        guarantee: crate::provider::GenerationRetryGuarantee,
    ) -> Self {
        self.provider.generation_retry_guarantee = guarantee;
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

    fn route_identity(&self, model: &str) -> crate::ProviderRouteIdentity {
        crate::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
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

    fn generation_retry_guarantee(
        &self,
        _request: &LlmRequest,
    ) -> crate::provider::GenerationRetryGuarantee {
        self.generation_retry_guarantee
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
        model: ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid mock model spec"),
        ..SessionPolicy::new(crate::TurnBudget::Unbounded)
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

/// Build the sealed leaf-attempt context every recorded tool body receives.
///
/// Tool bodies take [`crate::AttemptContext`], never [`crate::ToolContext`], so
/// this is the context a unit test hands a provider's `execute` or
/// `execute_attempt`.
pub fn mock_attempt_context() -> crate::AttemptContext<'static> {
    mock_attempt_context_from(&mock_tool_context())
}

impl<'run> crate::AttemptContext<'run> {
    /// Test-only projection of a mock tool context, with no reserved
    /// completion key: a test harness is not the attempt coordinator.
    #[doc(hidden)]
    pub fn __for_testing(
        context: &crate::ToolContext<'run>,
        execution_scope_id: impl Into<String>,
    ) -> Self {
        Self::from_tool_context(
            context,
            execution_scope_id.into(),
            None,
            crate::tool_provider::AttemptCompletionSupport::NotDeclared,
        )
    }
}

/// Project an existing mock host context into the leaf-attempt context. Use
/// this when the test also needs the host handles the [`mock_tool_context`]
/// carries.
pub fn mock_attempt_context_from<'run>(
    context: &crate::ToolContext<'run>,
) -> crate::AttemptContext<'run> {
    crate::AttemptContext::from_tool_context(
        context,
        "test-turn".to_string(),
        None,
        crate::tool_provider::AttemptCompletionSupport::NotDeclared,
    )
}

/// Like [`mock_attempt_context`], but with the grant execution binding
/// populated, for provider tests that assert grant-only routing behavior.
pub fn mock_attempt_context_with_execution_binding(
    binding: serde_json::Value,
) -> crate::AttemptContext<'static> {
    mock_attempt_context_from(&mock_tool_context_with_execution_binding(binding))
}

/// Like [`mock_attempt_context`], but lets the caller supply the host.
pub fn mock_attempt_context_with_host<T>(host: Arc<T>) -> crate::AttemptContext<'static>
where
    T: crate::plugin::SessionStateService
        + crate::plugin::SessionLifecycleService
        + crate::plugin::SessionGraphService
        + 'static,
{
    mock_attempt_context_from(&mock_tool_context_with_host(host))
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

pub struct EmptyToolProvider;

#[async_trait::async_trait]
impl crate::ToolProvider for EmptyToolProvider {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        crate::ToolOutcome::err(serde_json::json!(format!(
            "test tool provider has no tool `{}`",
            call.name
        )))
    }
}

pub fn code_execution_context_with_tool_catalog(
    tool_catalog: crate::ToolCatalog,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .tool_catalog(tool_catalog)
        .build()
        .into_runtime()
}

pub fn code_execution_context_with_tool_provider_and_catalog(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .provider(provider)
        .tool_catalog(tool_catalog)
        .build()
        .into_runtime()
}

/// Build a code-execution context whose process service, effect controller,
/// and process-environment store are shared with a real test process worker.
pub fn code_execution_context_with_process_dependencies(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    trigger_router: Option<crate::TriggerRouter>,
    processes: Arc<dyn crate::ProcessService>,
    effect_controller: Arc<dyn crate::RuntimeEffectController>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .provider(provider)
        .tool_catalog(tool_catalog)
        .trigger_router(trigger_router)
        .processes(processes)
        .shared_effect_controller(effect_controller)
        .process_env_store(process_env_store)
        .execution_env_spec(execution_env_spec)
        .build()
        .into_runtime()
}

pub fn code_execution_context() -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new().build().into_runtime()
}

/// Build an empty code-execution context carrying the stable parent invocation
/// that production installs around an `ExecCode` effect.
pub fn code_execution_context_with_invocation(
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

/// Build a code-execution context with a concrete tool surface and the stable
/// parent invocation production installs around an `ExecCode` effect.
pub fn code_execution_context_with_tool_provider_catalog_and_invocation(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .provider(provider)
        .tool_catalog(tool_catalog)
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
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
    TestExecutionContextBuilder::new()
        .trigger_router(Some(crate::TriggerRouter::new(trigger_store, None, None)))
        .build()
        .into_runtime()
}

/// Build a code-execution context whose trigger operations pass through the
/// supplied effect controller before reaching `trigger_store`.
pub fn code_execution_context_with_trigger_store_and_effect_controller(
    trigger_store: Arc<dyn crate::TriggerStore>,
    effect_controller: Arc<dyn crate::RuntimeEffectController>,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .trigger_router(Some(crate::TriggerRouter::new(trigger_store, None, None)))
        .shared_effect_controller(effect_controller)
        .build()
        .into_runtime()
}

/// Builds the production-shaped `ToolContext` installed while a controller-owned
/// `ToolAttempt` local executor is open. Durable-adapter tests use this to prove
/// that tool-facing clients refuse before entering a nested controller command.
pub fn atomic_tool_context_with_services<'run>(
    scoped_effect_controller: crate::ScopedEffectController<'run>,
    session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService>,
    processes: Arc<dyn crate::ProcessService>,
    trigger_router: Option<crate::TriggerRouter>,
    parent_invocation: crate::RuntimeInvocation,
) -> crate::ToolContext<'run> {
    crate::ToolContext::from_dispatch(build_atomic_tool_dispatch(
        TestExecutionContextBuilder::new()
            .session_id("atomic-tool-test-session")
            .session_lifecycle(session_lifecycle)
            .processes(processes)
            .trigger_router(trigger_router)
            .borrowed_effect_controller(scoped_effect_controller)
            .dispatch_parent_invocation(parent_invocation),
    ))
    .build()
}

fn build_atomic_tool_dispatch<'run>(
    builder: TestExecutionContextBuilder<'run>,
) -> Arc<crate::tool_dispatch::ToolDispatchContext<'run>> {
    builder
        .shared_session_host(Arc::new(MockSessionManager::default()))
        .build()
        .dispatch
}

#[derive(Debug)]
struct FrozenToolCoordinatorClock(std::time::Instant);

#[async_trait::async_trait]
impl crate::Clock for FrozenToolCoordinatorClock {
    fn now(&self) -> std::time::Instant {
        self.0
    }

    fn timestamp_ms(&self) -> u64 {
        1_700_000_000_000
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.timestamp_ms() as i64)
            .expect("frozen coordinator timestamp")
    }

    async fn sleep(&self, _duration: std::time::Duration) {}

    async fn sleep_until(&self, _deadline: std::time::Instant) {}
}

/// Execute one opted-in provider through the production attempt coordinator,
/// intent drain, and model-return projection against supplied durable services.
///
/// This is public for **conformance-suite embedders** that compare the same
/// provider and intent contract across durable backends.
pub async fn coordinate_tool_provider_with_services(
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    processes: Arc<dyn crate::ProcessService>,
    session_id: &str,
    definition: crate::ToolDefinition,
    provider: Arc<dyn crate::ToolProvider>,
    call: crate::PreparedToolCall,
) -> Result<crate::sansio::CompletedToolCall, String> {
    let parent_invocation = crate::RuntimeInvocation::effect(
        crate::RuntimeScope::for_turn(session_id, scoped_effect_controller.scope_id(), 1, 0),
        format!("tool-batch:{}", call.call_id),
        crate::RuntimeEffectKind::ToolBatch,
        format!("tool-batch:{}", call.call_id),
    );
    let dispatch = build_atomic_tool_dispatch(
        TestExecutionContextBuilder::new()
            .session_id(session_id)
            .session_lifecycle(Arc::new(MockSessionManager::default()))
            .processes(processes)
            .provider(provider)
            .tool_catalog(crate::ToolCatalog::from_tool_definitions(vec![definition]))
            .borrowed_effect_controller(scoped_effect_controller)
            .dispatch_parent_invocation(parent_invocation.clone())
            .clock(Arc::new(FrozenToolCoordinatorClock(
                std::time::Instant::now(),
            ))),
    );
    let tool_context = crate::ToolContext::from_dispatch(Arc::clone(&dispatch))
        .prepared_call(&call)
        .cancellation_token(Some(tokio_util::sync::CancellationToken::new()))
        .build();
    let coordinated = crate::tool_dispatch::coordinate_tool_invocation(
        dispatch.as_ref(),
        call.clone(),
        None,
        crate::ToolRetryPolicy::Never,
        crate::tool_dispatch::ToolAttemptEffectIdentity::Batch {
            parent: parent_invocation,
            replay_suffix: call.call_id.clone(),
        },
        tool_context.cancellation_token().cloned(),
        None,
        None,
        |completion_key| {
            crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                Arc::clone(&dispatch),
                tool_context.clone(),
                completion_key,
            )
        },
    )
    .await;
    let crate::tool_dispatch::ToolCallLaunch::Done(outcome) = coordinated.launch else {
        return Err("the literal differential provider unexpectedly deferred".to_string());
    };
    let outcome = *outcome;
    let mut model_return = dispatch
        .plugins
        .project_tool_result(crate::plugin::ToolResultProjectionContext {
            session_id: session_id.to_string(),
            call_id: call.call_id.clone(),
            tool_name: outcome.record.tool.clone(),
            args: outcome.record.args.clone(),
            output: outcome.record.output.clone(),
            duration_ms: outcome.record.duration_ms,
        })
        .await
        .map_err(|error| error.to_string())?;
    model_return.parts.extend(
        outcome
            .intent_outcomes
            .iter()
            .map(|intent| crate::ModelToolReturnPart::text(intent.model_addendum())),
    );
    Ok(crate::sansio::CompletedToolCall {
        call_id: call.call_id,
        tool_name: outcome.record.tool,
        args: outcome.record.args,
        output: outcome.record.output,
        model_return,
        duration_ms: outcome.record.duration_ms,
        intent_outcomes: outcome.intent_outcomes,
        replay: call.replay,
    })
}

/// Execute a recorded tool-intent drain through the production process-command
/// route while retaining a small, backend-neutral differential-test surface.
pub async fn execute_tool_intents_with_services(
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    processes: Arc<dyn crate::ProcessService>,
    session_id: &str,
    tool_call_id: &str,
    intents: &crate::ToolIntents,
) -> Vec<crate::ToolIntentExecutionOutcome> {
    let parent_invocation = crate::RuntimeInvocation::effect(
        crate::RuntimeScope::new(session_id),
        format!("tool-intent-drain:{tool_call_id}"),
        crate::RuntimeEffectKind::ToolBatch,
        format!("tool-intent-drain:{tool_call_id}"),
    );
    let dispatch = build_atomic_tool_dispatch(
        TestExecutionContextBuilder::new()
            .session_id(session_id)
            .session_lifecycle(Arc::new(MockSessionManager::default()))
            .processes(processes)
            .borrowed_effect_controller(scoped_effect_controller)
            .dispatch_parent_invocation(parent_invocation),
    );
    crate::tool_dispatch::execute_final_tool_intents(
        dispatch.as_ref(),
        Some(tool_call_id),
        intents,
        None,
    )
    .await
}

/// A `ProcessService` that applies the production command-runner guard, then
/// routes **every** `ProcessCommand` through the operation scope's effect
/// controller exactly as `ProcessCommandRunner` does.
///
/// The FIG-1127 review found that stubbing the sibling routes here made the
/// harness structurally incapable of reaching `await_process`, `cancel`,
/// `signal`, `list_visible` and `transfer` — the routes that turned out to be
/// unguarded. The stubs are gone: this service now mirrors the production
/// routing decision per method, so a route that journals in production journals
/// here too.
struct EffectBackedProcessService {
    registry: Arc<dyn crate::ProcessRegistry>,
}

impl EffectBackedProcessService {
    async fn execute(
        &self,
        scope: crate::ProcessOpScope<'_>,
        command: crate::ProcessCommand,
    ) -> Result<crate::ProcessEffectOutcome, crate::PluginError> {
        let effect_id = command.effect_id();
        // Production derives the nested invocation from the parent invocation
        // the ToolContext carries (`process_effect_invocation`), so a nested
        // command inherits the attempt's replay-key lineage. Use the same
        // helper, not a lookalike.
        let invocation = crate::runtime::causal::process_effect_invocation(
            "atomic-tool-test-session",
            scope.parent_invocation.clone(),
            &effect_id,
        );
        let controller = scope.controller();
        let (proxy, requests) = crate::runtime::effect::EffectTaskController::scoped(
            controller,
            scope.effect_controller.scoped().execution_scope().clone(),
        )
        .map_err(crate::RuntimeEffectControllerError::from)?;
        let local_executor =
            crate::RuntimeEffectLocalExecutor::processes(Arc::clone(&self.registry), None)
                .with_process_effect_controller(
                    proxy
                        .owned_controller()
                        .expect("effect-task proxy owns its controller"),
                );
        let outcome = crate::runtime::effect::drive_effect_controller_task(
            controller,
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::process(command),
            ),
            local_executor,
            requests,
        )
        .await?;
        outcome.into_process().map_err(crate::PluginError::from)
    }
}

#[async_trait::async_trait]
impl crate::ProcessService for EffectBackedProcessService {
    async fn list_visible_for_attempt(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        match mode {
            crate::ProcessListMode::Live => self.registry.list_live_observed_by(session_id).await,
            crate::ProcessListMode::All => self.registry.list_observed_by(session_id).await,
        }
    }

    async fn start_from_request(
        &self,
        _session_id: &str,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        let observers = request.observers.clone();
        let env_ref = request
            .env_spec
            .as_ref()
            .map(|_| crate::ProcessExecutionEnvRef::new("process-env:atomic-tool-test"));
        let registration = request.into_registration(env_ref);
        let command = crate::ProcessCommand::Start {
            registration,
            observers,
            env_spec: None,
            execution_context: Box::new(crate::ProcessExecutionContext::default()),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Start { record } => {
                Ok(crate::ProcessHandleView::from_record(*record))
            }
            _ => unreachable!("start command returns start outcome"),
        }
    }

    async fn start_from_recorded_intent(
        &self,
        session_id: &str,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        self.start_from_request(session_id, request, scope).await
    }

    async fn start(
        &self,
        _session_id: &str,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = crate::ProcessCommand::Start {
            registration,
            observers: options.initial_observers,
            env_spec: None,
            execution_context: Box::new(crate::ProcessExecutionContext::default()),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Start { record } => Ok(*record),
            _ => unreachable!("start command returns start outcome"),
        }
    }

    /// Production writes the terminal through the registry, not the controller
    /// (`complete_external_process`), so this route journals nothing.
    async fn complete_external(
        &self,
        _session_id: &str,
        process_id: &str,
        await_output: crate::ProcessAwaitOutput,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessCompletionOutcome, crate::PluginError> {
        self.registry
            .complete_process(
                process_id,
                await_output,
                crate::ProcessCompletionAuthority::ExternalOwner,
            )
            .await
    }

    async fn await_process(
        &self,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        let command = crate::ProcessCommand::Await {
            process_id: process_id.to_string(),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Await { output } => Ok(*output),
            _ => unreachable!("await command returns await outcome"),
        }
    }

    async fn list_visible(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let command = crate::ProcessCommand::List {
            session_scope: crate::SessionScope::new(session_id),
            mode,
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::List { entries } => Ok(entries),
            _ => unreachable!("list command returns list outcome"),
        }
    }

    /// Production authorizes visibility with a registry observer read
    /// (`validate_process_handles_observed_inner`), so this route journals
    /// nothing.
    async fn validate_visible(
        &self,
        session_id: &str,
        process_ids: &[String],
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        for process_id in process_ids {
            if !self.registry.is_observer(session_id, process_id).await? {
                return Err(crate::PluginError::Session(format!(
                    "process handle `{process_id}` is not visible in this session"
                )));
            }
        }
        Ok(())
    }

    async fn cancel(
        &self,
        _session_id: &str,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = crate::ProcessCommand::Cancel {
            process_id: process_id.to_string(),
            reason: Some("requested by tool".to_string()),
            replay: None,
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn cancel_recorded_intent(
        &self,
        _session_id: &str,
        process_id: &str,
        reason: Option<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = crate::ProcessCommand::Cancel {
            process_id: process_id.to_string(),
            reason,
            replay: None,
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn finish_recorded_intent_parent(
        &self,
        _session_id: &str,
        identity: crate::ToolIntentIdentity,
        process_id: String,
        policy: crate::ProcessParentEndPolicy,
        reason: String,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ToolIntentParentEndOutcome, crate::PluginError> {
        match self
            .execute(
                scope,
                crate::ProcessCommand::ParentEnd {
                    identity,
                    process_id,
                    policy,
                    reason,
                },
            )
            .await?
        {
            crate::ProcessEffectOutcome::ParentEnd { outcome } => Ok(*outcome),
            _ => unreachable!("parent-end command returns parent-end outcome"),
        }
    }

    async fn signal_possessed(
        &self,
        _session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let event_type = crate::process_signal_event_type(&signal_name)?;
        let request = crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
            format!("process:{process_id}:signal.{signal_name}:{signal_id}"),
        );
        let command = crate::ProcessCommand::Signal {
            process_id: process_id.to_string(),
            signal_name,
            signal_id,
            request,
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Signal { event } => Ok(*event),
            _ => unreachable!("signal command returns signal outcome"),
        }
    }

    async fn signal_recorded_intent(
        &self,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.signal_possessed(
            session_id,
            process_id,
            signal_name,
            signal_id,
            payload,
            scope,
        )
        .await
    }

    async fn emit_event(
        &self,
        _session_id: &str,
        process_id: &str,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let command = crate::ProcessCommand::EmitEvent {
            process_id: process_id.to_string(),
            request: crate::ProcessEventAppendRequest::new(event_type, payload)
                .with_replay_key(replay_key),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::EmitEvent { event, .. } => Ok(*event),
            _ => unreachable!("emit-event command returns emit-event outcome"),
        }
    }

    async fn emit_event_recorded_intent(
        &self,
        session_id: &str,
        process_id: &str,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.emit_event(
            session_id, process_id, event_type, replay_key, payload, scope,
        )
        .await
    }

    async fn transfer(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: Vec<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        let command = crate::ProcessCommand::Transfer {
            from_scope: crate::SessionScope::new(from_session_id),
            to_scope: crate::SessionScope::new(to_session_id),
            process_ids,
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Transfer => Ok(()),
            _ => unreachable!("transfer command returns transfer outcome"),
        }
    }
}

/// Builds a process service whose starts cross the supplied operation scope's
/// effect controller, matching the production durable process-start route.
pub fn effect_backed_process_service(
    registry: Arc<dyn crate::ProcessRegistry>,
) -> Arc<dyn crate::ProcessService> {
    Arc::new(EffectBackedProcessService { registry })
}

/// Convenience helper for the common tool-test shape: build a
/// [`mock_tool_context`], wrap `name` + `args` in a `ToolCall`, and `await`
/// the provider's `execute`. Use this for unit tests that don't need to
/// inspect host interactions; call `mock_tool_context()` directly and
/// construct `ToolCall` manually for more involved scenarios.
pub async fn run_tool<P>(tool: &P, name: &str, args: &serde_json::Value) -> crate::ToolOutcome
where
    P: crate::ToolProvider + ?Sized,
{
    let host = mock_tool_context();
    let context = mock_attempt_context_from(&host);
    tool.execute(crate::ToolCall {
        name,
        args,
        context: &context,
    })
    .await
}

/// Build an empty `AssembledTurn` whose assistant text is `summary`.
pub fn mock_assembled_turn(session_id: &str, summary: &str) -> AssembledTurn {
    AssembledTurn {
        state: SessionSnapshot {
            session_id: session_id.to_string(),
            policy: SessionPolicy::new(crate::TurnBudget::Unbounded),
            ..SessionSnapshot::new(SessionPolicy::new(crate::TurnBudget::Unbounded))
        },
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: summary.to_string(),
        }),
        assistant_output: AssistantOutput {
            safe_text: summary.to_string(),
            raw_text: summary.to_string(),
            state: OutputState::Usable,
        },
        execution: TurnExecutionMetrics::default(),
        token_usage: TokenUsage::default(),
        children_usage: Vec::new(),
        llm_calls: Vec::new(),
        tool_calls: Vec::new(),
        errors: Vec::new(),
        turn_input_acceptance: None,
        turn_cancel_input_outcome: Default::default(),
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
            snapshot: RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
            .to_snapshot(),
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
        self.created.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionStateService for MockSessionManager {
    async fn turn_scope(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<crate::ExecutionScope, PluginError> {
        Ok(crate::ExecutionScope::turn(session_id, turn_id))
    }

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
        self.created.lock_recover().push(request.clone());
        Ok(SessionHandle {
            session_id: request
                .session_id
                .clone()
                .unwrap_or_else(|| "child".to_string()),
            parent_session_id: request.relation.parent_session_id().map(ToOwned::to_owned),
            policy: request.policy.unwrap_or_else(mock_session_policy),
            observed_processes: Vec::new(),
        })
    }

    async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
        self.closed.lock_recover().push(session_id.to_string());
        Ok(())
    }
    async fn start_turn(
        &self,
        request: crate::SessionTurnRequest<'_>,
    ) -> Result<AssembledTurn, PluginError> {
        let (turn, scoped_effect_controller) = request.into_parts();
        self.turns.lock_recover().push((
            turn.session_id,
            turn.turn_id.to_string(),
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
    async fn start_from_recorded_intent(
        &self,
        session_id: &str,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, PluginError> {
        let observers = request.observers.clone();
        let env_ref = request
            .env_spec
            .as_ref()
            .map(|_| crate::ProcessExecutionEnvRef::new("process-env:mock-recorded-intent"));
        let record = self
            .start(
                session_id,
                request.into_registration(env_ref),
                crate::ProcessStartOptions::new().with_initial_observers(observers),
                scope,
            )
            .await?;
        Ok(crate::ProcessHandleView::from_record(record))
    }

    async fn finish_recorded_intent_parent(
        &self,
        _session_id: &str,
        _identity: crate::ToolIntentIdentity,
        _process_id: String,
        _policy: crate::ProcessParentEndPolicy,
        _reason: String,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ToolIntentParentEndOutcome, PluginError> {
        Err(PluginError::Session(
            "recorded parent-end commands are unavailable in this mock".to_string(),
        ))
    }

    async fn start(
        &self,
        _session_id: &str,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        let id = registration.id.clone();
        // This mock stands in as the executor, so it completes the row under the
        // authority its declared disposition permits: externally-owned rows close
        // via their external owner, lash-executed rows via the workflow-key path.
        let authority = if registration.disposition == crate::RecoveryContract::ExternallyOwned {
            crate::ProcessCompletionAuthority::external_owner()
        } else {
            crate::ProcessCompletionAuthority::workflow_key(&id)
        };
        let observers = options.initial_observers;
        self.process_registry
            .register_process_with_observers(registration, &observers)
            .await?;
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
    ) -> Result<Vec<crate::ProcessRecord>, PluginError> {
        let _ = scope;
        match mode {
            crate::ProcessListMode::Live => {
                self.process_registry
                    .list_live_observed_by(session_id)
                    .await
            }
            crate::ProcessListMode::All => self.process_registry.list_observed_by(session_id).await,
        }
    }

    async fn validate_visible(
        &self,
        session_id: &str,
        handle_ids: &[String],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let _ = scope;
        for handle_id in handle_ids {
            match self
                .process_registry
                .is_observer(session_id, handle_id)
                .await
            {
                Ok(true) | Err(PluginError::ProcessNoLongerRetained { .. }) => {}
                Ok(false) => {
                    return Err(PluginError::Session(format!(
                        "process handle `{handle_id}` is not live or visible in this session"
                    )));
                }
                Err(error) => return Err(error),
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
            None,
        )
        .await
    }

    async fn cancel_recorded_intent(
        &self,
        _session_id: &str,
        process_id: &str,
        reason: Option<String>,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        crate::InlineRuntimeEffectController::request_process_cancel(
            self.process_registry.clone(),
            process_id,
            reason,
            None,
        )
        .await
    }

    async fn signal_possessed(
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

    async fn signal_recorded_intent(
        &self,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.signal_possessed(
            session_id,
            process_id,
            signal_name,
            signal_id,
            payload,
            scope,
        )
        .await
    }

    async fn emit_event_recorded_intent(
        &self,
        _session_id: &str,
        process_id: &str,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.process_registry
            .append_event(
                process_id,
                crate::ProcessEventAppendRequest::new(event_type, payload)
                    .with_replay_key(replay_key),
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
        let _ = scope;
        self.process_registry
            .transfer_observers(
                from_session_id,
                to_session_id,
                &process_ids,
                crate::ProcessObserverBy::host("mock-transfer"),
            )
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
#[cfg(test)]
pub(crate) use test_protocol_fakes::test_standard_protocol_factory_with_runtime_state;
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
            id: "test_protocol",
            include_batch: true,
            decode_code_create_options: false,
            session_override: None,
            code_executor: None,
        })]
    }

    #[cfg(test)]
    pub(crate) fn test_standard_protocol_factory_with_runtime_state(
        session: Arc<dyn ProtocolSessionPlugin>,
        code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
    ) -> Arc<dyn PluginFactory> {
        Arc::new(TestProtocolFactory {
            id: "test_protocol",
            include_batch: true,
            decode_code_create_options: false,
            session_override: Some(session),
            code_executor,
        })
    }

    pub fn test_code_protocol_factories() -> Vec<Arc<dyn PluginFactory>> {
        vec![Arc::new(TestProtocolFactory {
            id: "protocol_code",
            include_batch: false,
            decode_code_create_options: true,
            session_override: None,
            code_executor: None,
        })]
    }

    struct TestProtocolFactory {
        id: &'static str,
        include_batch: bool,
        decode_code_create_options: bool,
        session_override: Option<Arc<dyn ProtocolSessionPlugin>>,
        code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
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
                session_override: self.session_override.clone(),
                code_executor: self.code_executor.clone(),
            }))
        }
    }

    struct TestProtocolPlugin {
        id: &'static str,
        include_batch: bool,
        decode_code_create_options: bool,
        session_override: Option<Arc<dyn ProtocolSessionPlugin>>,
        code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
    }

    impl SessionPlugin for TestProtocolPlugin {
        fn id(&self) -> &'static str {
            self.id
        }

        fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            reg.protocol()
                .session(self.session_override.clone().unwrap_or_else(|| {
                    Arc::new(TestProtocolSession {
                        decode_code_create_options: self.decode_code_create_options,
                    })
                }))?;
            if let Some(code_executor) = self.code_executor.as_ref() {
                reg.execution().code_executor(code_executor.clone())?;
            }
            if self.include_batch {
                reg.tools().orchestrating(test_batch_orchestrating_tool())?;
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

    /// The test `batch` tool registers in the runtime-owned orchestration lane,
    /// exactly like `lash-protocol-standard`'s: nesting tool dispatch is not
    /// something a recorded leaf attempt can do.
    fn test_batch_orchestrating_tool() -> crate::tool_provider::orchestration::OrchestratingToolDef
    {
        let implementation: Arc<
            dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
        > = Arc::new(TestProtocolBatchTool);
        // SAFETY: lash-core owns this test-only batch contract and its body.
        unsafe {
            crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(
                implementation,
            )
        }
    }

    struct TestProtocolBatchTool;

    #[async_trait]
    impl crate::tool_provider::orchestration::OrchestratingToolImplementation
        for TestProtocolBatchTool
    {
        fn manifest(&self) -> crate::ToolManifest {
            test_batch_tool_definition().manifest()
        }

        fn contract(&self) -> Arc<crate::ToolContract> {
            Arc::new(test_batch_tool_definition().contract())
        }

        async fn execute(
            &self,
            args: &serde_json::Value,
            context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
        ) -> crate::ToolOutcome {
            execute_test_batch(context, args).await
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
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
        args: &serde_json::Value,
    ) -> crate::ToolOutcome {
        const MAX: usize = 25;
        let Some(raw_calls) = args.get("tool_calls").and_then(|v| v.as_array()) else {
            return crate::ToolOutcome::err_fmt("Missing required parameter: tool_calls");
        };
        if raw_calls.is_empty() {
            return crate::ToolOutcome::err_fmt("Invalid tool_calls: expected at least one call");
        }

        let mut results: Vec<lash_sansio::BatchResultRow> = Vec::new();
        let mut parallel_specs = Vec::new();
        for (index, item) in raw_calls.iter().enumerate().take(MAX) {
            let Some(obj) = item.as_object() else {
                return crate::ToolOutcome::err_fmt(format_args!(
                    "Invalid tool_calls[{index}]: expected object with tool and parameters"
                ));
            };
            let Some(tool) = obj
                .get("tool")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                return crate::ToolOutcome::err_fmt(format_args!(
                    "Invalid tool_calls[{index}].tool: expected non-empty string"
                ));
            };
            if tool == "batch" {
                results.push(lash_sansio::BatchResultRow::failure(
                    index,
                    tool,
                    0,
                    serde_json::json!("Tool 'batch' is not allowed inside batch"),
                ));
                continue;
            }
            let parameters = obj
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let Some(manifest) = context.callable_tool_manifest(tool) else {
                results.push(lash_sansio::BatchResultRow::failure(
                    index,
                    tool,
                    0,
                    serde_json::json!(format!("Tool '{tool}' is unavailable in this session")),
                ));
                continue;
            };
            parallel_specs.push((
                index,
                crate::ToolInvocation::new(format!("test-batch:{index}"), manifest.id, parameters),
            ));
        }

        let outcomes = context
            .call_tool_batch(
                parallel_specs
                    .iter()
                    .map(|(_, invocation)| invocation.clone())
                    .collect(),
            )
            .await;
        for ((index, invocation), outcome) in parallel_specs.into_iter().zip(outcomes) {
            let tool_label = invocation.tool_id.to_string();
            let tool_record = outcome.record.unwrap_or(crate::ToolCallRecord {
                call_id: Some(invocation.id),
                tool: tool_label,
                args: invocation.args,
                output: outcome.output,
                duration_ms: 0,
            });
            let value = tool_record.output.value_for_projection();
            results.push(if tool_record.output.is_success() {
                lash_sansio::BatchResultRow::success(
                    index,
                    tool_record.tool,
                    tool_record.duration_ms,
                    value,
                )
            } else {
                lash_sansio::BatchResultRow::failure(
                    index,
                    tool_record.tool,
                    tool_record.duration_ms,
                    value,
                )
            });
        }

        for overflow_index in MAX..raw_calls.len() {
            results.push(lash_sansio::BatchResultRow::failure(
                overflow_index,
                raw_calls
                    .get(overflow_index)
                    .and_then(|item| item.get("tool"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown"),
                0,
                serde_json::json!("Maximum of 25 tool calls allowed in batch"),
            ));
        }

        results.sort_by_key(|row| row.index);
        crate::ToolOutcome::ok(serde_json::json!({ "results": results }))
    }

    #[test]
    fn test_batch_result_row_decode_names_missing_required_field() {
        let error = serde_json::from_value::<lash_sansio::BatchResultRow>(serde_json::json!({
            "index": 0,
            "tool": "probe",
            "success": true,
            "result": "ok"
        }))
        .expect_err("row without duration_ms must fail");

        assert!(
            error.to_string().contains("missing field `duration_ms`"),
            "{error}"
        );
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
            parts: crate::shared_parts(vec![crate::Part::error(
                format!("{message_id}.p0"),
                format!("Turn limit reached ({max_turns}) before a final test response."),
            )]),
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
            use crate::{CheckpointKind, Message, MessageRole, Part, SessionStreamEvent};
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
                let parts_out = vec![Part::prose(format!("{asst_id}.p0"), assistant_text, None)];
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
                assistant_parts.push(Part::prose(
                    format!("{}.p{}", asst_id, assistant_parts.len()),
                    assistant_text,
                    None,
                ));
            }
            let mut calls = Vec::new();
            for (call_id, tool_name, input_json, replay) in tool_calls {
                assistant_parts.push(Part::tool_call(
                    format!("{}.p{}", asst_id, assistant_parts.len()),
                    input_json.clone(),
                    call_id.clone(),
                    tool_name.clone(),
                    replay.clone(),
                ));
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
            use crate::{CheckpointKind, Message, MessageRole, Part, SessionStreamEvent};
            use lash_sansio::session_model::reassign_part_ids;
            let mut actions = Vec::new();
            let mut result_parts = Vec::new();
            let mut terminal_outcome = None;
            for outcome in completed {
                if terminal_outcome.is_none() && outcome.output.is_success() {
                    terminal_outcome = match outcome.output.control.as_ref() {
                        Some(crate::ToolControl::SwitchAgentFrame {
                            frame_key,
                            initial_nodes,
                            task: Some(task),
                        }) if !task.trim().is_empty() => Some(TurnOutcome::AgentFrameSwitch {
                            frame_key: frame_key.clone(),
                            task: task.clone(),
                            initial_nodes: initial_nodes.clone(),
                        }),
                        Some(crate::ToolControl::Finish { value }) => {
                            Some(TurnOutcome::Finished(TurnFinish::ToolValue {
                                tool_name: outcome.tool_name.clone(),
                                value: crate::tool_value_for_projection(value),
                            }))
                        }
                        Some(crate::ToolControl::Fail { failure }) => {
                            Some(TurnOutcome::Stopped(TurnStop::ToolError {
                                tool_name: outcome.tool_name.clone(),
                                value: crate::tool_failure_for_projection(failure),
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
                            result_parts.push(Part::tool_result(
                                String::new(),
                                text.clone(),
                                outcome.call_id.clone(),
                                outcome.tool_name.clone(),
                            ));
                        }
                        lash_sansio::ModelToolReturnPart::Attachment(source) => {
                            result_parts.push(Part::tool_result_attachment(
                                String::new(),
                                String::new(),
                                lash_sansio::PartAttachment {
                                    source: source.clone(),
                                },
                                outcome.call_id.clone(),
                                outcome.tool_name.clone(),
                            ));
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
            if let Some(max_turns) = ctx.turn_budget().max_turns()
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
