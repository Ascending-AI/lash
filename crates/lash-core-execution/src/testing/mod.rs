//! In-tree test fixtures shared across the lash crate's test modules.
//!
//! Cuts down on per-test-module `MockSessionManager` boilerplate by
//! providing a configurable mock implementation plus a couple of small
//! builders for common policy / turn fixtures.

// Test-support module: these fixtures run inside a test, and a broken setup
// assumption must abort it loudly rather than be reshaped into a runtime error
// the test under way would then report as a runtime defect. Clippy's
// `allow-expect-in-tests` reaches `#[test]` functions only, not the fixtures
// they call.
#![expect(
    clippy::expect_used,
    reason = "test-support fixtures: a broken setup assumption aborts the test"
)]

use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use crate::{
    ProcessEventLog as _, ProcessLifecycle as _, ProcessObserverRegistry as _,
    ProcessRegistrar as _,
};
use lash_sansio::sync::MutexExt;
// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and a
// reader or editor following the merged doc comment — including the
// submodule's own intra-doc links — would resolve it against *this* module's
// scope, where none of the linked items exist.
pub mod attempt_sentinel;
pub mod behavior_transcript;
pub mod runbook_evidence;
#[cfg(feature = "testing")]
pub mod trace_capture;

pub mod execution_context_builder;
pub mod sansio_transcript;
pub mod tool_fixtures;
mod trigger_context;

pub use execution_context_builder::*;
pub use tool_fixtures::{FIXTURE_ECHO_TOOL, FixtureTools, fixture_echo_definition};
pub use trigger_context::*;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

#[cfg(any(test, feature = "testing"))]
pub fn queued_lane_holder_for_testing(expires_at_epoch_ms: u64) -> crate::QueuedLaneHolder {
    crate::QueuedLaneHolder::new(crate::store::SessionExecutionLease {
        session_id: SessionId::from("queued-lane-test"),
        owner: crate::LeaseOwnerIdentity::opaque("holder", "holder:incarnation"),
        executor_id: "holder-executor".to_string(),
        lease_token: "holder-token".to_string(),
        fencing_token: 7,
        claimed_at_epoch_ms: 1_000,
        lease_term_ms: 6_400,
        expires_at_epoch_ms,
    })
}

#[cfg(any(test, feature = "testing"))]
pub fn process_work_wiring_for_registry(
    registry: Arc<dyn crate::ProcessRegistry>,
) -> crate::ProcessWorkWiring {
    let watched = crate::facade_support::watch_process_registry(registry);
    let port = Arc::new(crate::NativeProcessWork::for_registry(Arc::clone(
        watched.registry(),
    )));
    crate::ProcessWorkWiring::new(watched, port)
}

#[cfg(any(test, feature = "testing"))]
pub fn process_execution_env_fixture() -> (
    Arc<dyn crate::ProcessExecutionEnvStore>,
    crate::ProcessExecutionEnvRef,
) {
    let spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let (store, env_ref) = crate::InMemoryProcessExecutionEnvStore::from_spec_for_testing(
        crate::ArtifactOwner::host("process-execution-env-fixture"),
        &spec,
    )
    .expect("fixed process execution environment fixture is valid");
    (Arc::new(store), env_ref)
}

/// FIG-2999: a recorded start declares its own execution env, so a fixture
/// whose `ProcessService` realizes a recorded intent publishes that env the way
/// the runtime's own start command does instead of leaving the registration
/// without a reference.
#[cfg(any(test, feature = "testing"))]
pub async fn publish_process_execution_env_for_testing(
    env_store: &dyn crate::ProcessExecutionEnvStore,
    owner: &crate::ArtifactOwner,
    spec: &crate::ProcessExecutionEnvSpec,
) -> Result<crate::ProcessExecutionEnvRef, crate::PluginError> {
    crate::publish_process_execution_env(env_store, owner, spec).await
}

/// Engine fixture for trigger-delivery tests that need to exercise the real
/// engine-only start contract without publishing unrelated language artifacts.
#[cfg(any(test, feature = "testing"))]
pub struct FixtureProcessEngine;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl crate::ProcessEngine for FixtureProcessEngine {
    fn kind(&self) -> &'static str {
        "testing-fixture"
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        Ok(
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({ "fixture": "complete" }),
            ))
            .into(),
        )
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn process_engine_fixture() -> crate::ProcessEngineRegistry {
    crate::ProcessEngineRegistry::new().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(FixtureProcessEngine)),
    )
}

#[cfg(any(test, feature = "testing"))]
struct FixtureProcessEnginePlugin;

#[cfg(any(test, feature = "testing"))]
impl crate::SessionPlugin for FixtureProcessEnginePlugin {
    fn id(&self) -> &'static str {
        "testing-fixture-process-engine"
    }

    fn register(&self, _registrar: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
struct FixtureProcessEngineFactory;

#[cfg(any(test, feature = "testing"))]
impl crate::PluginFactory for FixtureProcessEngineFactory {
    fn id(&self) -> &'static str {
        "testing-fixture-process-engine"
    }

    fn process_engine_contributions(
        &self,
        _context: &crate::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<crate::ProcessEngineRegistration>, crate::PluginError> {
        Ok(vec![crate::ProcessEngineRegistration::accepting(Arc::new(
            FixtureProcessEngine,
        ))])
    }

    fn build(
        &self,
        _context: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(FixtureProcessEnginePlugin))
    }
}

/// Plugin factory that contributes [`FixtureProcessEngine`] to a facade host.
#[cfg(any(test, feature = "testing"))]
pub fn process_engine_plugin_fixture() -> Arc<dyn crate::PluginFactory> {
    Arc::new(FixtureProcessEngineFactory)
}

use crate::llm::transport::LlmTransportError;
use crate::llm::types::{LlmRequest, LlmResponse};
use crate::plugin::{PluginError, SessionCreateRequest, SessionHandle, SessionSnapshot};
use crate::provider::{Provider, ProviderComponents, ProviderHandle};
use crate::session_model::{ConversationRecord, SessionHistoryRecord};
use crate::{
    AssembledTurn, AssistantOutput, ModelSpec, OutputState, ProviderOptions, RuntimeSessionState,
    SessionPolicy, TokenUsage, TurnExecutionMetrics, TurnFinish, TurnOutcome, TurnStop,
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
pub use lash_core_store::testing::queued_work_claim_policy;

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

pub use lash_core_ids::test_clock::TestClock;

#[test]
fn test_clock_wall_clock_faces_agree() {
    let clock = TestClock::new(1_700_000_000_123);
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
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
    pub graph_delta_bytes: usize,
    /// Named-MessagePack size of the hydrated checkpoint.
    pub checkpoint_bytes: usize,
    /// Raw UTF-8 byte length of the committed attachment ids.
    pub attachment_manifest_bytes: usize,
    pub queue_batch_bytes: usize,
    /// Persisted JSON encoding of the selected Agent Frame identity.
    pub agent_frame_bytes: usize,
    pub usage_delta_bytes: usize,
    /// Persisted JSON encoding of the durable turn result stamp.
    pub turn_result_bytes: usize,
    /// Saturating sum of the budgeted components.
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
        queue_batch_bytes: measurement.queue_batch_bytes,
        agent_frame_bytes: measurement.agent_frame_bytes,
        usage_delta_bytes: measurement.usage_delta_bytes,
        turn_result_bytes: measurement.turn_result_bytes,
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
type ReconciliationFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<crate::provider::ReconciledUsage>, LlmTransportError>>
            + Send,
    >,
>;
type ReconcileFn = dyn Fn(String) -> ReconciliationFuture + Send + Sync;
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
    /// Host-invoked usage reconciliation (FIG-2765). Defaults to "this provider
    /// keeps no generation records", the trait default.
    reconcile: Arc<ReconcileFn>,
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
                reconcile: Arc::new(|_generation_id| Box::pin(async { Ok(None) })),
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

    #[cfg(any(test, feature = "testing"))]
    pub fn generation_retry_guarantee(
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

    /// Answer host-invoked usage reconciliation for a generation id.
    pub fn reconcile_usage<F, Fut>(mut self, reconcile: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<crate::provider::ReconciledUsage>, LlmTransportError>>
            + Send
            + 'static,
    {
        self.provider.reconcile = Arc::new(move |generation_id| Box::pin(reconcile(generation_id)));
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

    async fn reconcile_usage(
        &mut self,
        generation_id: &str,
    ) -> Result<Option<crate::provider::ReconciledUsage>, LlmTransportError> {
        (self.reconcile)(generation_id.to_string()).await
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

pub fn standard_test_policy() -> crate::SessionPolicy {
    crate::SessionPolicy {
        provider_id: "mock".to_string(),
        model: crate::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
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

/// Tool bodies take [`crate::AttemptContext`], never [`crate::ToolContext`], so
/// this is the context a unit test hands a provider's `execute`.
pub fn mock_attempt_context() -> crate::AttemptContext<'static> {
    mock_attempt_context_from(&mock_tool_context())
}

impl<'run> crate::AttemptContext<'run> {
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

    /// Crate-internal fixture constructor for the granted route: the context
    /// a call admitted by `grant` executes under. Dispatch applies the
    /// grant's execution binding and source id together
    /// (`AttemptAuthority::apply_execution_binding`), so this hook does the
    /// same rather than leaving the pair to drift in a hand-built fixture.
    /// Like [`__for_testing`](Self::__for_testing), no completion key is
    /// reserved: a test harness is not the attempt coordinator.
    ///
    /// Deliberately crate-private and double-underscored: a granted context
    /// minted for one grant could otherwise be paired with a different
    /// manifest through the public [`crate::ToolCall::new`], which the
    /// registry would accept when the same source resolves it — the
    /// prepared-identity refusal only exists inside dispatch.
    /// [`run_tool_granted`] stays the sole downstream entry, so a granted
    /// context can never outlive the grant that produced it.
    pub(crate) fn __for_granted_source(
        context: &crate::ToolContext<'run>,
        execution_scope_id: impl Into<String>,
        grant: &crate::ToolExecutionGrant,
    ) -> Self {
        let context = context
            .clone()
            .with_tool_execution_binding(grant.execution_binding.clone())
            .with_granted_source_id(grant.source_id.clone());
        Self::from_tool_context(
            &context,
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

/// Like [`mock_attempt_context_from`], but with a completion key already
/// reserved, as the coordinator does for a tool that declared it defers.
///
/// A body that parks reads the key before returning `Pending`, so a test of a
/// parking tool that used the keyless projection would only ever observe the
/// "did not declare deferred completion" refusal.
pub fn mock_attempt_context_with_completion_key<'run>(
    context: &crate::ToolContext<'run>,
    key: crate::AwaitEventKey,
) -> crate::AttemptContext<'run> {
    crate::AttemptContext::from_tool_context(
        context,
        "test-turn".to_string(),
        Some(key),
        crate::tool_provider::AttemptCompletionSupport::Available,
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
        SessionId::from("test-session".to_string()),
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

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::err(serde_json::json!(format!(
            "test tool provider has no tool `{}`",
            call.name()
        )))
        .into()
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

/// Restate a context's engine child attempt bound, the way a runtime wires it
/// from [`RuntimeControlConfig::engine_child_max_attempts`](crate::RuntimeControlConfig).
///
/// Tests that drive a child to its attempt ceiling need a smaller bound than
/// the host default; production has no reason to set it per execution, so the
/// setter itself stays crate-private.
#[cfg(any(test, feature = "testing"))]
pub fn with_engine_child_max_attempts(
    context: crate::RuntimeExecutionContext<'_>,
    max_attempts: std::num::NonZeroU32,
) -> crate::RuntimeExecutionContext<'_> {
    context.with_engine_child_max_attempts(max_attempts)
}

/// Build an empty code-execution context for a specific durable process.
#[cfg(any(test, feature = "testing"))]
pub fn code_execution_context_for_process(
    registration: &crate::ProcessRegistration,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .build()
        .into_runtime()
        .with_process_execution(registration, None)
}

/// Build an empty code-execution context whose cancellation is already visible.
pub fn cancelled_code_execution_context() -> crate::RuntimeExecutionContext<'static> {
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    TestExecutionContextBuilder::new()
        .build()
        .into_runtime()
        .with_cancellation_token(cancellation)
}

/// Build an empty code-execution context cancelled after its runtime yields.
pub fn code_execution_context_cancelling_after_yield() -> crate::RuntimeExecutionContext<'static> {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancellation_after_yield = cancellation.clone();
    crate::task::spawn(async move {
        tokio::task::yield_now().await;
        cancellation_after_yield.cancel();
    });
    TestExecutionContextBuilder::new()
        .build()
        .into_runtime()
        .with_cancellation_token(cancellation)
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

/// Build an empty code-execution context with a caller-supplied effect
/// controller and stable parent invocation.
pub fn code_execution_context_with_effect_controller_and_invocation(
    effect_controller: Arc<dyn crate::RuntimeEffectController>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .shared_effect_controller(effect_controller)
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

/// Build a concrete code-execution context with caller-supplied tool and
/// effect hosts plus the stable parent invocation.
pub fn code_execution_context_with_tool_provider_catalog_effect_controller_and_invocation(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    effect_controller: Arc<dyn crate::RuntimeEffectController>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .provider(provider)
        .tool_catalog(tool_catalog)
        .shared_effect_controller(effect_controller)
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

/// Build a concrete code-execution context with an already admitted effect
/// scope. Durable-controller tests use this instead of the shared-controller
/// shortcut, whose intentionally synthetic runtime-operation scope is suitable
/// only for scope-agnostic fakes.
pub fn code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    effect_controller: crate::ScopedEffectController<'static>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .provider(provider)
        .tool_catalog(tool_catalog)
        .borrowed_effect_controller(effect_controller)
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

pub fn exec_code_invocation(
    session_id: impl Into<SessionId>,
    turn_id: impl Into<TurnId>,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: impl Into<String>,
    replay_key: impl Into<String>,
) -> crate::RuntimeInvocation {
    let session_id = session_id.into();
    let turn_id = turn_id.into();
    crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(
            crate::ExecutionScope::turn(session_id.clone(), turn_id.clone()),
            replay_key,
        )
        .expect("valid test effect address"),
        crate::RuntimeAttribution::for_turn(session_id, turn_id, turn_index, protocol_iteration),
        effect_id,
    )
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

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = 1_700_000_000_000;
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, _duration: std::time::Duration) {}

    async fn sleep_until(&self, _deadline: std::time::Instant) {}
}

#[test]
fn frozen_tool_coordinator_clock_wall_clock_faces_agree() {
    let clock = FrozenToolCoordinatorClock(std::time::Instant::now());
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

/// Execute one opted-in provider through the production attempt coordinator,
/// intent drain, and model-return projection against supplied durable services.
///
/// This is public for **conformance-suite embedders** that compare the same
/// provider and intent contract across durable backends.
pub async fn coordinate_tool_provider_with_services(
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    processes: Arc<dyn crate::ProcessService>,
    session_id: &SessionId,
    definition: crate::ToolDefinition,
    provider: Arc<dyn crate::ToolProvider>,
    call: crate::PreparedToolCall,
) -> Result<crate::sansio::CompletedToolCall, String> {
    let parent_invocation = crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(
            scoped_effect_controller.execution_scope().clone(),
            format!("tool-batch:{}", call.call_id),
        )
        .expect("valid test effect address"),
        crate::RuntimeAttribution::for_turn(session_id, scoped_effect_controller.scope_id(), 1, 0),
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
    let turn_cancel_wait = dispatch.effect_controller.scoped().turn_cancel_wait(
        tool_context
            .cancellation_token()
            .cloned()
            .unwrap_or_default(),
    );
    let coordinated = crate::tool_dispatch::coordinate_tool_invocation(
        dispatch.as_ref(),
        call.clone(),
        None,
        crate::ToolRetryPolicy::Never,
        None,
        crate::tool_dispatch::ToolAttemptEffectIdentity::Batch {
            parent: parent_invocation,
            replay_suffix: call.call_id.clone(),
        },
        &turn_cancel_wait,
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
    let baseline = crate::ModelToolReturn::from_output(
        call.call_id.clone(),
        outcome.record.tool.clone(),
        &outcome.record.output,
    );
    let settlement = crate::runtime::effect::ToolSettlement::from_dispatch(&outcome, baseline);
    let mut model_return = dispatch
        .plugins
        .present_tool_result(
            crate::plugin::ToolResultProjectionContext {
                session_id: SessionId::from(session_id.to_string()),
                call_id: call.call_id.clone(),
                tool_name: outcome.record.tool.clone(),
                args: outcome.record.args.clone(),
                output: outcome.record.output.clone(),
                duration_ms: outcome.record.duration_ms,
                artifacts: std::sync::Arc::new(
                    crate::runtime::effect::SessionPresentationArtifacts::new(
                        std::sync::Arc::clone(&dispatch.attachment_store),
                    ),
                ),
            },
            std::sync::Arc::new(settlement),
            &dispatch
                .execution_env_spec
                .policy
                .model
                .capability
                .attachment_acceptance,
        )
        .await
        .model_return;
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
    session_id: &SessionId,
    tool_call_id: &str,
    intents: &crate::ToolIntents,
) -> Result<Vec<crate::ToolIntentExecutionOutcome>, crate::RuntimeEffectControllerError> {
    execute_tool_intents_with_services_and_hook(
        scoped_effect_controller,
        processes,
        session_id,
        tool_call_id,
        intents,
        None,
    )
    .await
}

/// Execute a recorded tool-intent drain with the production trigger router.
///
/// Durable-adapter tests use this narrow seam to prove replay refusal before
/// trigger-store ingestion.
pub async fn execute_tool_intents_with_services_and_trigger_router(
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    processes: Arc<dyn crate::ProcessService>,
    trigger_router: crate::TriggerRouter,
    session_id: &SessionId,
    tool_call_id: &str,
    intents: &crate::ToolIntents,
) -> Result<Vec<crate::ToolIntentExecutionOutcome>, crate::RuntimeEffectControllerError> {
    execute_tool_intents_with_services_and_hook_and_trigger_router(
        scoped_effect_controller,
        processes,
        Some(trigger_router),
        session_id,
        tool_call_id,
        intents,
        None,
    )
    .await
}

/// Execute a recorded tool-intent drain through the production process-command
/// route and notify a test hook after a child Start has committed.
pub async fn execute_tool_intents_with_services_and_hook(
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    processes: Arc<dyn crate::ProcessService>,
    session_id: &SessionId,
    tool_call_id: &str,
    intents: &crate::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<Vec<crate::ToolIntentExecutionOutcome>, crate::RuntimeEffectControllerError> {
    execute_tool_intents_with_services_and_hook_and_trigger_router(
        scoped_effect_controller,
        processes,
        None,
        session_id,
        tool_call_id,
        intents,
        child_trace_hook,
    )
    .await
}

async fn execute_tool_intents_with_services_and_hook_and_trigger_router(
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    processes: Arc<dyn crate::ProcessService>,
    trigger_router: Option<crate::TriggerRouter>,
    session_id: &SessionId,
    tool_call_id: &str,
    intents: &crate::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<Vec<crate::ToolIntentExecutionOutcome>, crate::RuntimeEffectControllerError> {
    let parent_invocation = crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(
            scoped_effect_controller.execution_scope().clone(),
            format!("tool-intent-drain:{tool_call_id}"),
        )
        .expect("valid test effect address"),
        crate::RuntimeAttribution::for_session(session_id),
        format!("tool-intent-drain:{tool_call_id}"),
    );
    let dispatch = build_atomic_tool_dispatch(
        TestExecutionContextBuilder::new()
            .session_id(session_id)
            .session_lifecycle(Arc::new(MockSessionManager::default()))
            .processes(processes)
            .trigger_router(trigger_router)
            .borrowed_effect_controller(scoped_effect_controller)
            .dispatch_parent_invocation(parent_invocation),
    );
    crate::tool_dispatch::execute_final_tool_intents(
        dispatch.as_ref(),
        Some(tool_call_id),
        intents,
        child_trace_hook,
    )
    .await
}

/// Build the real engine run context used by validation-path tests that are
/// expected to settle before constructing a nested runtime context.
pub fn process_engine_run_context_for_validation(
    registration: crate::ProcessRegistration,
    tool_catalog: Arc<crate::ToolCatalog>,
    process_registry_available: bool,
) -> crate::ProcessEngineRunContext<'static> {
    let process_id = registration.id.clone();
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    let process_work = process_work_wiring_for_registry(registry);
    let plugins = crate::PluginHost::new(test_standard_protocol_factories())
        .build_session("engine-validation-test")
        .expect("test protocol session builds");
    let effect_host = crate::facade_support::NativeEffectHost::default();
    let scoped_effect_controller = crate::EffectHost::scoped_static(
        &effect_host,
        crate::AdmittedScope::process(crate::ProcessRef::new(
            process_id.clone(),
            crate::ProcessIncarnation::from_registration_sequence(1),
        )),
    )
    .expect("valid process scope")
    .expect("native effect host owns a static controller");
    let execution_context = crate::ProcessExecutionContext::default()
        .with_execution_write_authority(crate::ProcessExecutionWriteAuthority::invocation(
            process_id,
            "engine-validation-test-execution",
        ));
    crate::ProcessEngineRunContext::new(
        registration,
        crate::ProcessIncarnation::from_registration_sequence(1),
        execution_context,
        process_work,
        SessionId::from("engine-validation-test"),
        plugins,
        tool_catalog,
        None,
        None,
        Arc::new(crate::NoQueuedWork::new()),
        crate::DeliveryPolicy::EarliestSafeBoundary,
        Arc::new(crate::SystemClock),
        process_registry_available,
        tokio_util::sync::CancellationToken::new(),
        None,
        scoped_effect_controller,
        None,
        Box::new(|_| {
            Err(crate::PluginError::Session(
                "validation test unexpectedly entered the nested runtime".to_string(),
            ))
        }),
    )
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
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
}

impl EffectBackedProcessService {
    async fn cancel_command(
        &self,
        process_id: &ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<crate::ProcessCommand, crate::PluginError> {
        match self.registry.resolve_process_ref(process_id).await {
            Ok(process_ref) => Ok(crate::ProcessCommand::Cancel {
                process_ref,
                origin,
                requester,
                attribution,
            }),
            Err(refusal @ crate::PluginError::ProcessUnknown { .. })
            | Err(refusal @ crate::PluginError::ProcessNoLongerRetained { .. }) => {
                Ok(crate::ProcessCommand::CancelRefused {
                    process_id: ProcessId::from(process_id.to_string()),
                    origin,
                    requester,
                    refusal,
                })
            }
            Err(error) => Err(error),
        }
    }

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
        let scoped = scope.effect_controller.scoped();
        let attribution = scope
            .parent_invocation
            .as_ref()
            .map(|parent| parent.attribution.clone())
            .unwrap_or_else(|| crate::RuntimeAttribution::for_session("atomic-tool-test-session"));
        let invocation = crate::runtime::causal::process_effect_invocation(
            scoped.execution_scope(),
            attribution,
            scope.parent_invocation.clone(),
            &effect_id,
        );
        let controller = scope.controller();
        let (proxy, requests) = crate::runtime::effect::EffectTaskController::scoped(
            controller,
            scope.effect_controller.scoped().admitted_scope().clone(),
        )
        .map_err(crate::RuntimeEffectControllerError::from)?;
        let local_executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&self.registry),
            Arc::new(crate::NativeProcessWork::for_registry(Arc::clone(
                &self.registry,
            ))),
        )
        .with_process_env_store(Arc::clone(&self.process_env_store))
        .with_process_effect_controller(
            proxy
                .owned_controller()
                .expect("effect-task proxy owns its controller"),
        );
        let outcome = crate::runtime::effect::drive_effect_controller_task(
            controller,
            scoped.execution_scope().clone(),
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
        session_id: &SessionId,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        match mode {
            crate::ProcessListMode::Live => self.registry.list_live_observed_by(session_id).await,
            crate::ProcessListMode::All => {
                self.registry
                    .list_observed_by(
                        session_id,
                        &crate::ProcessListFilter {
                            status: crate::ProcessStatusFilter::Any,
                            ..Default::default()
                        },
                    )
                    .await
            }
        }
    }

    async fn start_from_request(
        &self,
        _session_id: &SessionId,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        let observers = request.observers.clone();
        let env_spec = request.env_spec.clone();
        let registration = request.into_registration(None);
        let command = crate::ProcessCommand::Start {
            registration,
            observers: observers.into_iter().collect(),
            env_spec,
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
        session_id: &SessionId,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        self.start_from_request(session_id, request, scope).await
    }

    async fn recorded_max_attempts(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<Option<u32>, crate::PluginError> {
        Ok(self
            .registry
            .get_process(process_id)
            .await?
            .and_then(|record| record.max_attempts))
    }

    async fn start(
        &self,
        _session_id: &SessionId,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = crate::ProcessCommand::Start {
            registration,
            observers: options.initial_observers.into_iter().collect(),
            env_spec: options.env_spec,
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
        _session_id: &SessionId,
        process_id: &ProcessId,
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
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        let command = crate::ProcessCommand::Await {
            process_ref: crate::ProcessRef::new(
                process_id,
                crate::ProcessIncarnation::from_registration_sequence(1),
            ),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Await { output } => Ok(*output),
            _ => unreachable!("await command returns await outcome"),
        }
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
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
        session_id: &SessionId,
        process_ids: &[ProcessId],
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
        _session_id: &SessionId,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = self
            .cancel_command(
                process_id,
                crate::CancelOrigin::OperatorRequested,
                serde_json::to_string(scope.effect_controller.scoped().execution_scope())
                    .expect("serializable effect scope"),
                None,
            )
            .await?;
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            crate::ProcessEffectOutcome::CancelRefused { refusal } => Err(refusal),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn cancel_recorded_intent(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = self
            .cancel_command(
                process_id,
                crate::CancelOrigin::ModelRequested,
                identity.replay_key.clone(),
                Some(crate::RuntimeReplayAttribution::ToolIntent(identity)),
            )
            .await?;
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            crate::ProcessEffectOutcome::CancelRefused { refusal } => Err(refusal),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn signal_possessed(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let event_type = crate::process_signal_event_type(&signal_name)?;
        let request = crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
            crate::process_signal_wait_key(process_id, &signal_name, &signal_id),
        );
        let process_ref = self.registry.resolve_process_ref(process_id).await?;
        let command = crate::ProcessCommand::Signal {
            process_ref,
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
        session_id: &SessionId,
        process_id: &ProcessId,
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
        _session_id: &SessionId,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let command = crate::ProcessCommand::EmitEvent {
            process_id: ProcessId::from(process_id.to_string()),
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
        session_id: &SessionId,
        process_id: &ProcessId,
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
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
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
    Arc::new(EffectBackedProcessService {
        registry,
        process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
    })
}

/// Convenience helper for the common tool-test shape: build a
/// [`mock_tool_context`], wrap `name` + `args` in a `ToolCall`, and `await`
/// the provider's `execute`. Use this for unit tests that don't need to
/// inspect host interactions; call `mock_tool_context()` directly and
/// construct `ToolCall` manually for more involved scenarios.
///
/// The full [`crate::ToolAttemptOutcome`] is returned so tests can assert on
/// declared intents rather than losing them to a projection.
pub async fn run_tool<P>(
    tool: &P,
    name: &str,
    args: &serde_json::Value,
) -> crate::ToolAttemptOutcome
where
    P: crate::ToolProvider + ?Sized,
{
    let host = mock_tool_context();
    let context = mock_attempt_context_from(&host);
    let Some(manifest) = tool.resolve_manifest(name) else {
        return crate::ToolOutcome::err_fmt(format!("unknown tool: {name}")).into();
    };
    tool.execute(crate::ToolCall::new(&manifest, args, &context))
        .await
}

/// Like [`run_tool`], but the attempt context rides the granted route: the
/// context a call admitted by `grant` executes under in a live turn.
///
/// The manifest comes from the grant rather than a catalog lookup —
/// executing outside Tool Catalog membership is what a
/// [`crate::ToolExecutionGrant`] is for — and the grant's execution binding
/// and source id are applied exactly as dispatch applies them, so a
/// provider's granted branch is exercisable outside a live turn.
/// [`run_tool`] remains the ungranted, catalog-route variant.
pub async fn run_tool_granted<P>(
    tool: &P,
    grant: &crate::ToolExecutionGrant,
    args: &serde_json::Value,
) -> crate::ToolAttemptOutcome
where
    P: crate::ToolProvider + ?Sized,
{
    let context =
        crate::AttemptContext::__for_granted_source(&mock_tool_context(), "test-turn", grant);
    tool.execute(crate::ToolCall::new(grant.manifest(), args, &context))
        .await
}

/// The standalone-registry counterpart of `session.admin().tools().add_provider`:
/// a fresh [`crate::ToolRegistry`] with `provider` registered through the same
/// live-source route a session admin call takes.
///
/// Host tests that exercise the registry's source routing — e.g. the route a
/// resolved deferred grant follows in production — build the registry here
/// rather than through the facade-internal registry ops. The returned
/// [`crate::tool_registry::ToolSourceHandle`] is the source the provider's
/// calls route to; [`crate::ToolRegistry::from_tool_provider`] is the
/// plugin-source lane instead.
pub fn tool_registry_with_live_provider(
    provider: Arc<dyn crate::ToolProvider>,
) -> (crate::ToolRegistry, crate::tool_registry::ToolSourceHandle) {
    use crate::tool_registry::facade_ops::ToolRegistryFacadeOps as _;

    let registry = crate::ToolRegistry::empty();
    let handle = registry
        .add_tool_provider(provider)
        .expect("registering a provider on a fresh tool registry cannot fail");
    (registry, handle)
}

pub fn mock_assembled_turn(session_id: &SessionId, summary: &str) -> AssembledTurn {
    AssembledTurn {
        state: SessionSnapshot {
            session_id: SessionId::from(session_id.to_string()),
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
        llm_calls: Vec::new(),
        tool_calls: Vec::new(),
        omitted: None,
        failure_evidence: Vec::new(),
        errors: Vec::new(),
        turn_input_acceptance: None,
        turn_cancel_input_outcome: Default::default(),
    }
}

/// Configurable mock for host capability traits. Tests override
/// the snapshot, tool catalog, and turn outcome via the builder
/// methods; `create_session` mutations are recorded so tests can
/// assert against them.
pub struct MockSessionManager {
    pub snapshot: SessionSnapshot,
    pub tool_catalog: Vec<serde_json::Value>,
    pub turn: AssembledTurn,
    pub tool_registry: Option<crate::ToolRegistry>,
    pub process_registry: Arc<crate::TestLocalProcessRegistry>,
    pub created: Mutex<Vec<SessionCreateRequest>>,
    /// Process terminals armed through
    /// [`ProcessService::attach_process_terminal`](crate::ProcessService::attach_process_terminal),
    /// in arming order.
    ///
    /// This mock cannot observe a real terminal, so it records the arming and
    /// leaves the resolution to the test standing in for the process: a test
    /// that resolves the recorded key is the terminal, and one that never does
    /// is a process that never ended.
    pub terminal_attachments: Mutex<Vec<(crate::ProcessRef, crate::AwaitEventKey)>>,
}

impl Default for MockSessionManager {
    fn default() -> Self {
        Self {
            snapshot: RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
            .to_snapshot(),
            tool_catalog: Vec::new(),
            turn: mock_assembled_turn(&SessionId::from("root"), ""),
            tool_registry: None,
            process_registry: Arc::new(crate::TestLocalProcessRegistry::default()),
            created: Mutex::new(Vec::new()),
            terminal_attachments: Mutex::new(Vec::new()),
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
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError> {
        Ok(crate::ExecutionScope::turn(session_id, turn_id))
    }

    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }

    async fn snapshot_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }
    async fn tool_catalog(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError> {
        Ok(self.tool_catalog.clone())
    }
    async fn tool_state(&self, _session_id: &SessionId) -> Result<crate::ToolState, PluginError> {
        self.tool_registry
            .as_ref()
            .map(crate::ToolRegistry::export_state)
            .ok_or_else(|| {
                PluginError::Session("tool state is unavailable in this session".to_string())
            })
    }

    async fn apply_tool_state(
        &self,
        _session_id: &SessionId,
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
                .unwrap_or_else(|| SessionId::from("child".to_string())),
            parent_session_id: request
                .relation
                .parent_session_id()
                .map(ToOwned::to_owned)
                .map(Into::into),
            policy: request.policy.unwrap_or_else(mock_session_policy),
            observed_processes: Vec::new(),
        })
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionGraphService for MockSessionManager {}

#[async_trait::async_trait]
impl crate::ProcessService for MockSessionManager {
    async fn attach_process_terminal(
        &self,
        process_ref: &crate::ProcessRef,
        key: &crate::AwaitEventKey,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        self.terminal_attachments
            .lock_recover()
            .push((process_ref.clone(), key.clone()));
        Ok(())
    }

    async fn start_from_recorded_intent(
        &self,
        session_id: &SessionId,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, PluginError> {
        let observers = request.observers.clone();
        let env_spec = request.env_spec.clone();
        let record = self
            .start(
                session_id,
                request.into_registration(None),
                crate::ProcessStartOptions::new()
                    .with_initial_observers(observers)
                    .with_env_spec(env_spec),
                scope,
            )
            .await?;
        Ok(crate::ProcessHandleView::from_record(record))
    }

    async fn start(
        &self,
        _session_id: &SessionId,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        let id = registration.id.clone();
        // The mock stands in as the journaled start effect: a spec-carrying
        // start is stamped with the content-addressed reference the executor's
        // publish would produce, since registration validation requires it.
        let registration = match (registration.env_ref.is_none(), options.env_spec.as_ref()) {
            (true, Some(env_spec)) => registration.with_execution_env_ref(Some(
                env_spec.stable_ref().map_err(|error| {
                    PluginError::Session(format!(
                        "failed to encode process execution environment: {error}"
                    ))
                })?,
            )),
            _ => registration,
        };
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
        process_id: &ProcessId,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, PluginError> {
        let registry: Arc<dyn crate::ProcessRegistry> = self.process_registry.clone();
        crate::NativeProcessWork::for_registry(registry)
            .await_terminal(process_id)
            .await
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
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
            crate::ProcessListMode::All => {
                self.process_registry
                    .list_observed_by(
                        session_id,
                        &crate::ProcessListFilter {
                            status: crate::ProcessStatusFilter::Any,
                            ..Default::default()
                        },
                    )
                    .await
            }
        }
    }

    async fn validate_visible(
        &self,
        session_id: &SessionId,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let _ = scope;
        for handle_id in handle_ids {
            match self
                .process_registry
                .is_observer(session_id, &ProcessId::from(handle_id))
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
        _session_id: &SessionId,
        process_id: &ProcessId,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        crate::NativeRuntimeEffectController::request_process_cancel(
            self.process_registry.clone(),
            process_id,
            crate::CancelOrigin::OperatorRequested,
            serde_json::to_string(_scope.effect_controller.scoped().execution_scope())
                .expect("serializable effect scope"),
            None,
        )
        .await
    }

    async fn cancel_recorded_intent(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        crate::NativeRuntimeEffectController::request_process_cancel(
            self.process_registry.clone(),
            process_id,
            crate::CancelOrigin::ModelRequested,
            identity.replay_key.clone(),
            Some(crate::RuntimeReplayAttribution::ToolIntent(identity)),
        )
        .await
    }

    async fn signal_possessed(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
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
                    crate::process_signal_wait_key(process_id, &signal_name, &signal_id),
                ),
            )
            .await
            .map(|result| result.event)
    }

    async fn signal_recorded_intent(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
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
        _session_id: &SessionId,
        process_id: &ProcessId,
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
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
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
#[cfg(any(test, feature = "testing"))]
pub use test_protocol_fakes::{
    test_code_protocol_factories, test_plugin_host, test_standard_protocol_factories,
    test_standard_protocol_factory_with_runtime_state,
};

mod test_protocol_fakes;

pub mod conformance_support;
pub mod graph_integrity;
pub mod lineage;
pub mod store_fixtures;
pub use crate::runtime::in_memory_lineage_handles;
