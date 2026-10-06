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

mod session_services;

use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use crate::plugin::PluginSessionRequest;
use crate::session::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
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
#[cfg(feature = "testing")]
pub mod kernel_internals;
#[cfg(any(test, feature = "testing"))]
pub mod observation_sink;
pub mod sansio_transcript;
pub mod tool_fixtures;
mod trigger_context;

/// A recording or fault layer over any effect host (FIG-3580).
pub use crate::runtime::effect::{EffectLayer, LayeredEffectHost};
pub use crate::runtime::process::{
    NonTerminalPagePause, NonTerminalPageRead, ProcessRegistryFaults, RegistrationHoldPoint,
    TriggerDeliveryPinReleaseLoss,
};
pub use execution_context_builder::*;
#[cfg(any(test, feature = "testing"))]
pub use observation_sink::ChannelObservationSink;
pub use tool_fixtures::{FIXTURE_ECHO_TOOL, FixtureTools, fixture_echo_definition};
pub use trigger_context::*;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

#[cfg(any(test, feature = "testing"))]
#[cfg(any(test, feature = "testing"))]
pub fn process_work_wiring_for_registry(
    registry: Arc<dyn crate::ProcessRegistry>,
) -> crate::ProcessWorkWiring {
    let watched = crate::facade_support::watch_process_registry(registry);
    let port = Arc::new(crate::NoProcessWork::for_registry(Arc::clone(
        watched.registry(),
    )));
    crate::ProcessWorkWiring::new(watched, port)
}

#[cfg(any(test, feature = "testing"))]
fn process_execution_env_fixture_spec() -> crate::ProcessExecutionEnvSpec {
    crate::ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
    )
}

/// Publishes the fixed fixture execution environment into `env_store`, held
/// by a fresh host pin the fixture never releases, and returns its reference.
#[cfg(any(test, feature = "testing"))]
pub async fn process_execution_env_fixture(
    env_store: &dyn crate::ProcessExecutionEnvStore,
) -> crate::ProcessExecutionEnvRef {
    crate::publish_process_execution_env(
        env_store,
        &host_pin_claim_for_testing(),
        &process_execution_env_fixture_spec(),
    )
    .await
    .expect("fixed process execution environment fixture publishes")
}

/// The reference [`process_execution_env_fixture`] publishes under. An
/// environment reference is content-addressed, so it names the fixture
/// environment in every store: a subscription draft that records the
/// reference, and never loads it, takes this instead of publishing.
#[cfg(any(test, feature = "testing"))]
pub fn process_execution_env_fixture_ref() -> crate::ProcessExecutionEnvRef {
    process_execution_env_fixture_spec()
        .stable_ref()
        .expect("fixed process execution environment fixture encodes")
}

/// FIG-2999: a recorded start declares its own execution env, so a fixture
/// whose `ProcessService` realizes a recorded intent publishes that env the way
/// the runtime's own start command does instead of leaving the registration
/// without a reference.
#[cfg(any(test, feature = "testing"))]
pub async fn publish_process_execution_env_for_testing(
    env_store: &dyn crate::ProcessExecutionEnvStore,
    claim: &crate::ReferrerClaim,
    spec: &crate::ProcessExecutionEnvSpec,
) -> Result<crate::ProcessExecutionEnvRef, crate::PluginError> {
    crate::publish_process_execution_env(env_store, claim, spec).await
}

/// A claim under a freshly minted host pin: an unguarded referrer a fixture
/// publishes under and never releases.
#[cfg(any(test, feature = "testing"))]
pub fn host_pin_claim_for_testing() -> crate::ReferrerClaim {
    crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
        crate::HostArtifactPin::mint(),
    ))
    .expect("a host pin is an unguarded referrer")
}

/// The kind [`HeldProcessEngine`] runs.
#[cfg(any(test, feature = "testing"))]
pub const HELD_PROCESS_ENGINE_KIND: &str = "testing-held";

/// An engine input of [`HeldProcessEngine`]'s kind: the trivial process a law
/// registers when it is about the row rather than about what runs it.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_input(payload: serde_json::Value) -> crate::ProcessInput {
    crate::ProcessInput::Engine {
        kind: HELD_PROCESS_ENGINE_KIND.to_string(),
        payload,
    }
}

/// A registration of [`held_engine_input`] under the fixture execution
/// environment reference, which every registration carries.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_registration(
    payload: serde_json::Value,
    provenance: crate::ProcessProvenance,
    lifetime: impl Into<crate::LifetimeDecision>,
) -> crate::ProcessRegistration {
    crate::ProcessRegistration::new(held_engine_input(payload), provenance, lifetime)
        .with_execution_env_ref(Some(process_execution_env_fixture_ref()))
}

/// Engine fixture whose process runs until it is cancelled, and then answers
/// its cancellation: a process that stays open until the law ends it.
#[cfg(any(test, feature = "testing"))]
pub struct HeldProcessEngine;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl crate::ProcessEngine for HeldProcessEngine {
    fn kind(&self) -> &'static str {
        HELD_PROCESS_ENGINE_KIND
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        context.cancellation_token().cancelled().await;
        Ok(
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("the held process was cancelled"),
            ))
            .into(),
        )
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the held engine stores no artifact `{artifact_ref}`"
        )))
    }
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

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the fixture engine stores no artifact `{artifact_ref}`"
        )))
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn process_engine_fixture() -> crate::ProcessEngineRegistry {
    crate::ProcessEngineRegistry::new()
        .with_registration(crate::ProcessEngineRegistration::accepting(Arc::new(
            FixtureProcessEngine,
        )))
        .with_registration(crate::ProcessEngineRegistration::accepting(Arc::new(
            HeldProcessEngine,
        )))
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
        Ok(vec![
            crate::ProcessEngineRegistration::accepting(Arc::new(FixtureProcessEngine)),
            crate::ProcessEngineRegistration::accepting(Arc::new(HeldProcessEngine)),
        ])
    }

    fn build(
        &self,
        _context: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(FixtureProcessEnginePlugin))
    }
}

#[cfg(any(test, feature = "testing"))]
impl crate::plugin::PluginDefinition for FixtureProcessEngineFactory {
    fn declaration() -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial("testing-fixture-process-engine")
    }
}

/// Plugin factory that contributes [`FixtureProcessEngine`] and
/// [`HeldProcessEngine`] to a facade host.
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
    AssembledTurn, AssistantOutput, OutputState, ProviderOptions, RuntimeSessionState,
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
/// default on the successor.
pub use lash_core_store::testing::queued_work_admission_policy;

/// Scripted faults and pauses at the store and deployment seams, and the gate
/// a law holds any other trait seam at (ADR 0044 §Simulation).
pub use crate::runtime::DeploymentOp;
pub use lash_core_store::store::StoreOp;
pub use lash_core_store::testing::{
    Call, FaultKind, GATE_DEADLINE, Gate, Op, Outcome, Phase, Script, Scripted, ScriptedError,
};

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

/// Production-equivalent logical payload accounting for one runtime commit.
pub use lash_core_store::store::commit_budget::RuntimeCommitBudgetMeasurement;

/// Measure a commit with the exact accounting used by
/// [`crate::RuntimeCommit::validate_budget`].
pub fn measure_runtime_commit_budget(
    commit: &crate::RuntimeCommit,
) -> Result<RuntimeCommitBudgetMeasurement, crate::StoreError> {
    commit.measure_budget()
}

/// Stage a protocol-owned execution-state root and its complete keyed leaf set
/// into resident session state, exactly as the runtime's turn boundary does.
///
/// The staging mutator itself is runtime-owned (ADR 0051's "neither" class), so
/// protocol crates reach it through this fixture rather than through
/// `RuntimeSessionState`'s public surface.
pub fn stage_execution_state_components(
    state: &mut RuntimeSessionState,
    snapshot: crate::plugin::ExecutionStateCapture,
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
        let mut response = (self.complete)(request).await?;
        // A scripted answer that carries counters is one its provider
        // reported, and a real provider reports them beside its own raw usage
        // record: without one the attempt is unreported by the provider
        // (ADR 0031).
        if response.provider_usage.is_none()
            && response.usage != crate::llm::types::LlmUsage::default()
        {
            response.provider_usage = serde_json::to_value(&response.usage).ok();
        }
        Ok(response)
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

/// Metadata for a test model: `wire_model` with a 200k prompt budget and no
/// other facts.
pub fn test_llm_profile_metadata(wire_model: &str) -> crate::LlmProfileMetadata {
    crate::LlmProfileMetadata::builder(wire_model)
        .context_window_tokens(200_000)
        .build()
        .expect("valid test model metadata")
}

/// A recorded test model selection: `metadata` as a registry would mint it
/// under `key`, run with the provider's default reasoning.
pub fn test_llm_profile_config(
    key: impl Into<crate::LlmProfileKey>,
    metadata: crate::LlmProfileMetadata,
) -> crate::LlmProfileConfig {
    crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(key.into(), metadata))
}

/// A registry serving one model: `provider` executes `metadata` under `key`.
pub fn single_llm_profile_registry(
    key: impl Into<crate::LlmProfileKey>,
    metadata: crate::LlmProfileMetadata,
    provider: ProviderHandle,
) -> std::sync::Arc<crate::LlmProfileRegistry> {
    std::sync::Arc::new(
        crate::LlmProfileRegistry::new()
            .register(key, crate::RegisteredLlmProfile::new(metadata, provider))
            .expect("a one-model registry registers its model"),
    )
}

/// A registry serving exactly the model `policy` records, executed by
/// `provider`: the recorded key bound to the recorded metadata.
pub fn llm_profiles_serving(
    policy: &crate::SessionPolicy,
    provider: ProviderHandle,
) -> std::sync::Arc<crate::LlmProfileRegistry> {
    let recorded = &policy
        .model
        .as_ref()
        .expect("a policy served by a registry records a model")
        .model;
    single_llm_profile_registry(
        recorded.key().clone(),
        recorded.metadata().clone(),
        provider,
    )
}

/// A registry serving [`standard_test_policy`]'s model with `provider`.
pub fn standard_test_llm_profiles(
    provider: ProviderHandle,
) -> std::sync::Arc<crate::LlmProfileRegistry> {
    single_llm_profile_registry(
        "mock-model",
        test_llm_profile_metadata("mock-model"),
        provider,
    )
}

pub fn standard_test_policy() -> crate::SessionPolicy {
    crate::SessionPolicy {
        model: Some(test_llm_profile_config(
            "mock-model",
            test_llm_profile_metadata("mock-model"),
        )),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024))
    }
}

/// Build a `SessionPolicy` populated with the canonical test model used by
/// lash's in-tree tests.
pub fn mock_session_policy() -> SessionPolicy {
    standard_test_policy()
}

/// The context a tool hook of a session's call to `tool_name` reads, built
/// for a test that calls the hook directly.
pub fn tool_hook_context(
    tool_name: &str,
    argument_projection: crate::ToolArgumentProjectionPolicy,
) -> crate::plugin::ToolHookContext {
    crate::plugin::ToolHookContext {
        owner: crate::RuntimeOwner::Session(crate::SessionId::from("session")),
        call_id: crate::ToolCallId::fixture("hook"),
        tool_id: crate::ToolId::from(format!("tool:{tool_name}")),
        tool_name: tool_name.to_string(),
        plugin_config: Default::default(),
        argument_projection,
        turn_context: crate::TurnContext::default(),
        sessions: std::sync::Arc::new(MockSessionManager::default()),
    }
}

/// The runtime's per-call dispatch state for one tool call, built for a test.
///
/// That state is crate-private: a test configures it here, then projects the
/// [`crate::AttemptContext`] a provider body receives ([`Self::attempt`]) or
/// hands it to a dispatch entry point in [`kernel_internals`]. Nothing a
/// fixture yields reaches dispatch, process administration or an effect
/// controller from inside a body.
#[derive(Clone)]
pub struct ToolCallFixture<'run> {
    pub(crate) context: crate::tool_provider::ToolContext<'run>,
}

impl ToolCallFixture<'static> {
    /// A call on a default [`MockSessionManager`], suitable for unit-testing a
    /// `ToolProvider` in isolation.
    pub fn mock() -> Self {
        Self::with_host(Arc::new(MockSessionManager::default()))
    }

    /// Like [`Self::mock`], but lets the caller supply the host. Useful when a
    /// tool reads from the host (snapshots, tool state, lifecycle hooks) and
    /// the test wants to assert against captured interactions.
    pub fn with_host<T>(host: Arc<T>) -> Self
    where
        T: crate::plugin::SessionStateService
            + crate::plugin::SessionLifecycleService
            + crate::plugin::SessionGraphService
            + 'static,
    {
        Self::with_host_and_direct_completions(
            host,
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
    }

    /// Like [`Self::with_host`], with the direct-completion client a tool's
    /// own model call goes through.
    pub fn with_host_and_direct_completions<T>(
        host: Arc<T>,
        direct_completions: crate::DirectCompletionClient<'static>,
    ) -> Self
    where
        T: crate::plugin::SessionStateService
            + crate::plugin::SessionLifecycleService
            + crate::plugin::SessionGraphService
            + 'static,
    {
        let sessions: Arc<dyn crate::plugin::SessionStateService> = host.clone();
        let session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService> = host.clone();
        let session_graph: Arc<dyn crate::plugin::SessionGraphService> = host;
        Self {
            context: crate::tool_provider::ToolContext::builder(
                SessionId::from("test-session"),
                sessions,
                session_lifecycle,
                session_graph,
                Arc::new(crate::UnavailableProcessService),
                crate::runtime::ScopedEffectController::shared(
                    Arc::new(UnavailableEffectController),
                    crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
                )
                .expect("valid test runtime scope"),
                Arc::new(crate::RuntimeAttachmentStore::unavailable()),
                direct_completions,
            )
            .build(),
        }
    }
}

impl<'run> ToolCallFixture<'run> {
    /// The call a runtime dispatch context would run: its sessions, processes,
    /// controller, catalog and parent invocation are the dispatch's.
    /// The call is named `ToolCallId::fixture("tool-call-fixture")` until
    /// [`Self::call_id`] or [`Self::prepared_call`] names it.
    pub fn from_dispatch(dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>) -> Self {
        let call = crate::PreparedToolCall {
            call_id: crate::ToolCallId::fixture("tool-call-fixture"),
            provider_call_id: None,
            tool_id: crate::ToolId::new("fixture"),
            tool_name: "fixture".to_string(),
            args: serde_json::Value::Null,
            replay: None,
            prepared_payload: serde_json::Value::Null,
        };
        Self {
            context: crate::tool_provider::ToolContext::from_dispatch(dispatch, &call).build(),
        }
    }

    /// Who the call runs for.
    pub fn owner(&self) -> &crate::ExecutionOwner {
        self.context.owner()
    }

    /// The process the call runs inside, when it runs inside one.
    pub fn enclosing_process(&self) -> Option<&ProcessId> {
        self.context.enclosing_process()
    }

    /// Names the call; a declaring body derives its intent identity from it.
    pub fn call_id(mut self, call_id: crate::ToolCallId) -> Self {
        self.context.call_id = call_id;
        self
    }

    /// Binds the call id and prepared payload of `call`.
    pub fn prepared_call(mut self, call: &crate::PreparedToolCall) -> Self {
        self.context.call_id = call.call_id.clone();
        self.context.prepared_payload = call.prepared_payload.clone();
        self
    }

    /// Sets the grant execution binding the call runs with.
    pub fn execution_binding(mut self, binding: serde_json::Value) -> Self {
        self.context.tool_execution_binding = binding;
        self
    }

    pub fn cancellation_token(
        mut self,
        cancellation_token: Option<tokio_util::sync::CancellationToken>,
    ) -> Self {
        self.context.cancellation_token = cancellation_token;
        self
    }

    /// Names the process the call runs inside.
    pub fn enclosing_process_id(mut self, process_id: Option<ProcessId>) -> Self {
        self.context.enclosing_process = process_id;
        self
    }

    /// Overrides who the call runs for.
    pub fn owner_as(mut self, owner: crate::ExecutionOwner) -> Self {
        self.context.owner = owner;
        self
    }

    /// Supplies the process service the call's process reads go through.
    pub fn processes(mut self, processes: Arc<dyn crate::ProcessService>) -> Self {
        self.context.processes = processes;
        self
    }

    pub fn parent_invocation(mut self, invocation: Option<crate::RuntimeInvocation>) -> Self {
        self.context.parent_invocation = invocation;
        self
    }

    pub fn child_execution_trace_hook(
        mut self,
        hook: Option<crate::ToolChildExecutionTraceHook>,
    ) -> Self {
        self.context.child_execution_trace_hook = hook;
        self
    }

    /// The mock call runs under a `RuntimeOperation` scope, which names no
    /// opener; production attempts always run under a turn, drain or process
    /// scope. A fixture that exercises an owner-derived answer (a declared
    /// child's parent scope) binds the real scope here.
    pub fn scoped_effect_controller(mut self, scoped: crate::ScopedEffectController<'run>) -> Self {
        self.context.effect_controller = scoped;
        self
    }

    /// Makes the call run inside the durable process `process_id` of
    /// `registry`, writing under `execution_write_authority`. The append
    /// target and the enclosing process are one fact: a process the fixture
    /// already names must be this one.
    pub fn inside_process(
        mut self,
        process_id: impl Into<ProcessId>,
        registry: Arc<dyn crate::ProcessRegistry>,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
    ) -> Self {
        self.context = self.context.with_process_events_for_testing(
            process_id,
            registry,
            execution_write_authority,
        );
        self
    }

    /// The attempt context a body executing this call under
    /// `execution_scope_id` receives. No completion key is reserved: a test
    /// harness is not the attempt coordinator.
    pub fn attempt(&self, execution_scope_id: impl Into<String>) -> crate::AttemptContext<'run> {
        crate::AttemptContext::from_tool_context(
            &self.context,
            execution_scope_id.into(),
            crate::tool_provider::AttemptCompletionSupport::NotDeclared,
        )
    }
}

/// Project an in-crate test's dispatch state into the leaf-attempt context.
#[cfg(test)]
pub(crate) fn mock_attempt_context_from<'run>(
    context: &crate::tool_provider::ToolContext<'run>,
) -> crate::AttemptContext<'run> {
    ToolCallFixture {
        context: context.clone(),
    }
    .attempt("test-turn")
}

/// Tool bodies take [`crate::AttemptContext`], so this is the context a unit
/// test hands a provider's `execute`.
pub fn mock_attempt_context() -> crate::AttemptContext<'static> {
    ToolCallFixture::mock().attempt("test-turn")
}

/// Like [`mock_attempt_context`], but with the grant execution binding
/// populated, for provider tests that assert grant-only routing behavior.
pub fn mock_attempt_context_with_execution_binding(
    binding: serde_json::Value,
) -> crate::AttemptContext<'static> {
    ToolCallFixture::mock()
        .execution_binding(binding)
        .attempt("test-turn")
}

/// Like [`mock_attempt_context`], but lets the caller supply the host.
pub fn mock_attempt_context_with_host<T>(host: Arc<T>) -> crate::AttemptContext<'static>
where
    T: crate::plugin::SessionStateService
        + crate::plugin::SessionLifecycleService
        + crate::plugin::SessionGraphService
        + 'static,
{
    ToolCallFixture::with_host(host).attempt("test-turn")
}

impl<'run> crate::AttemptContext<'run> {
    /// Crate-internal fixture constructor for the granted route: the context
    /// a call admitted by `grant` executes under. Dispatch applies the
    /// grant's execution binding and source id together
    /// (`AttemptAuthority::apply_execution_binding`), so this hook does the
    /// same rather than leaving the pair to drift in a hand-built fixture.
    /// Like [`ToolCallFixture::attempt`], no completion key is reserved: a
    /// test harness is not the attempt coordinator.
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
            crate::tool_provider::AttemptCompletionSupport::NotDeclared,
        )
    }
}

/// Run one effect through its local executor with no journal: the execution
/// half of a controller-owned test double, which keeps its own journal and
/// answers its own await events.
///
/// Await-event commands are refused, because answering them needs a promise
/// authority this function does not have. A process command runs on a task of
/// its own, as a host runs it, so a panicking process is contained as a typed
/// failure rather than unwinding the double.
pub async fn execute_effect_locally(
    envelope: crate::RuntimeEffectEnvelope,
    local_executor: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
    match envelope.command {
        crate::RuntimeEffectCommand::PeekAwaitEvent { .. }
        | crate::RuntimeEffectCommand::AwaitEvent { .. } => {
            Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::AwaitEventUnsupported,
                "a locally executed effect has no await-event authority",
            ))
        }
        crate::RuntimeEffectCommand::Process { command } => {
            if matches!(
                command.as_ref(),
                crate::ProcessCommand::PublishDefinition { .. }
                    | crate::ProcessCommand::GetDefinition { .. }
            ) {
                let result = local_executor
                    .into_definition_execution()?
                    .execute(*command)
                    .await?;
                return Ok(crate::RuntimeEffectOutcome::Process { result });
            }
            let execution = local_executor.into_process()?;
            let receiver = envelope.invocation.execution_scope().clone();
            if matches!(command.as_ref(), crate::ProcessCommand::Await { .. }) {
                // Boxed: the await's state machine is large, and inlining it
                // would grow every caller's future by the same amount.
                let result = Box::pin(execution.execute(&receiver, *command)).await?;
                return Ok(crate::RuntimeEffectOutcome::Process { result });
            }
            let joined = crate::task::spawn(async move {
                Box::pin(execution.execute(&receiver, *command)).await
            })
            .await;
            let result = match joined {
                Ok(result) => result?,
                Err(error) => {
                    return Err(crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectProcessTaskJoin,
                        format!("locally executed process effect task failed: {error}"),
                    ));
                }
            };
            Ok(crate::RuntimeEffectOutcome::Process { result })
        }
        crate::RuntimeEffectCommand::Trigger { command } => {
            local_executor
                .execute_trigger(envelope.invocation, *command)
                .await
        }
        _ => local_executor.execute(envelope).await,
    }
}

/// The effect controller of a context with no effect host: every effect and
/// group is refused, and await events answer the resolver's refusing
/// defaults.
///
/// It journals nothing, so it is not an effect host. A mock tool context
/// carries it, as it carries no process port and no attachment port: a tool
/// test that never runs an effect needs no backend, and one that does
/// builds its context over one.
#[derive(Debug, Default)]
pub struct UnavailableEffectController;

impl crate::AwaitEventResolver for UnavailableEffectController {
    /// No effect host stands behind it, so no authority mints its keys.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for UnavailableEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        Err(crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::Plugin,
            format!(
                "this context has no effect host; `{}` was not executed",
                envelope.invocation.effect_replay_key()
            ),
        ))
    }
}

/// The process-exec-env port of a context with no store: publication and
/// acquisition are refused, a cleanup has nothing to sever, and reads find
/// nothing.
///
/// It keeps nothing, so it is not a persistence store: it stands where a test
/// needs *a* port it never publishes through.
#[derive(Debug, Default)]
pub struct UnavailableProcessExecutionEnvStore;

impl UnavailableProcessExecutionEnvStore {
    fn refused() -> crate::ArtifactStoreError {
        crate::ArtifactStoreError::Backend(
            "this context has no process execution environment store".into(),
        )
    }
}

#[async_trait::async_trait]
impl crate::ProcessExecutionEnvStore for UnavailableProcessExecutionEnvStore {
    async fn publish_process_execution_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &crate::ProcessExecutionEnvRef,
        _bytes: &[u8],
    ) -> Result<(), crate::ArtifactStoreError> {
        Err(Self::refused())
    }

    async fn acquire_process_execution_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &crate::ProcessExecutionEnvRef,
    ) -> Result<(), crate::ArtifactStoreError> {
        Err(Self::refused())
    }

    async fn end_process_env_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn get_process_execution_env(
        &self,
        _env_ref: &crate::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError> {
        Ok(None)
    }
}

/// The turn-prelude port of a context with no store: a publication is
/// refused, a cleanup has nothing to sever, and reads find nothing.
///
/// It keeps nothing, so it is not a persistence store: it stands where a test
/// needs *a* port its runtime never runs a turn through.
#[derive(Debug, Default)]
pub struct UnavailableTurnPreludeStore;

#[async_trait::async_trait]
impl crate::TurnPreludeStore for UnavailableTurnPreludeStore {
    async fn publish_turn_prelude(
        &self,
        _claim: &crate::ReferrerClaim,
        _prelude_ref: &crate::TurnPreludeRef,
        _bytes: &[u8],
    ) -> Result<(), crate::ArtifactStoreError> {
        Err(crate::ArtifactStoreError::Backend(
            "this context has no turn prelude store".into(),
        ))
    }

    async fn end_turn_prelude_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn get_turn_prelude(
        &self,
        _prelude_ref: &crate::TurnPreludeRef,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError> {
        Ok(None)
    }
}

/// Runtime services with no attachment port and no process-exec-env store,
/// for a test of plugin or catalog wiring that never writes through either.
pub fn runtime_services_without_ports(
    plugins: Arc<crate::PluginSession>,
) -> crate::RuntimeServices {
    crate::RuntimeServices::new(
        plugins,
        Arc::new(crate::RuntimeAttachmentStore::unavailable()),
        Arc::new(UnavailableProcessExecutionEnvStore),
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

pub fn code_execution_context_with_tool_catalog<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    tool_catalog: crate::ToolCatalog,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .tool_catalog(tool_catalog)
        .build()
        .into_runtime()
}

pub fn code_execution_context_with_tool_provider_and_catalog<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .provider(provider)
        .tool_catalog(tool_catalog)
        .build()
        .into_runtime()
}

pub fn code_execution_context_with_process_dependencies<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    trigger_router: Option<crate::TriggerRouter>,
    processes: Arc<dyn crate::ProcessService>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .provider(provider)
        .tool_catalog(tool_catalog)
        .trigger_router(trigger_router)
        .processes(processes)
        .execution_env_spec(execution_env_spec)
        .build()
        .into_runtime()
}

pub fn code_execution_context<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .build()
        .into_runtime()
}

/// Build an empty code-execution context for a specific durable process.
#[cfg(any(test, feature = "testing"))]
pub fn code_execution_context_for_process<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    process_id: crate::ProcessId,
    registration: &crate::ProcessRegistration,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .build()
        .into_runtime()
        .with_process_execution(process_id, registration, None)
}

/// Build an empty code-execution context whose cancellation is already visible.
pub fn cancelled_code_execution_context<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
) -> crate::RuntimeExecutionContext<'run> {
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    TestExecutionContextBuilder::new(ports.into())
        .build()
        .into_runtime()
        .with_cancellation_token(cancellation)
}

/// Build an empty code-execution context whose turn's cancellation gate
/// already carries an `Immediate` local stop. Delivering the stop here, before
/// the cell runs, is what makes the observation deterministic: a code cell
/// running on the context meets the request at its first journaled cancel
/// checkpoint, the same boundary a request landing mid-run would be met at,
/// instead of racing a spawned delivery against its checkpoint schedule.
pub async fn code_execution_context_stopped<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
) -> crate::RuntimeExecutionContext<'run> {
    let ports = ports.into();
    let host = Arc::clone(&ports.effect_host);
    let context = TestExecutionContextBuilder::new(ports)
        .build()
        .into_runtime();
    let control = Arc::new(
        crate::runtime::turn_control::ActiveTurnControl::new(
            host.await_event_resolver(),
            crate::TurnAddress::new(
                context
                    .session_id()
                    .expect("the test execution context runs in a session")
                    .clone(),
                "cancelled-cell-turn",
            ),
        )
        .await
        .expect("the test host keys a turn's cancellation gate"),
    );
    control
        .request_local_stop(
            host.await_event_resolver(),
            crate::TurnCancelMode::Immediate,
            None,
        )
        .await
        .expect("the test host resolves the cancellation gate");
    context.with_recorded_turn_cancel(
        false,
        control,
        host,
        tokio_util::sync::CancellationToken::new(),
    )
}

/// Build an empty code-execution context whose own turn is stopped
/// `Immediate`, through its durable cancellation gate, once `stop` fires.
///
/// The gate is the one the context's turn-observing waits race: the turn is
/// keyed from the scope those waits observe, so an engine that races a wait
/// against it (a SQL timer, FIG-3672 P9) sees the stop.
pub async fn code_execution_context_stopped_on<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    stop: tokio_util::sync::CancellationToken,
) -> crate::RuntimeExecutionContext<'run> {
    let ports = ports.into();
    let host = Arc::clone(&ports.effect_host);
    let context = TestExecutionContextBuilder::new(ports)
        .build()
        .into_runtime();
    let wait = context.turn_cancel_wait(tokio_util::sync::CancellationToken::new());
    let scope = wait
        .observed_scope()
        .expect("a test context observes its turn's cancellation");
    let (Some(session_id), Some(turn_id)) = (scope.session_id(), scope.turn_id()) else {
        panic!("a test context's observed scope is a turn scope: {scope:?}");
    };
    let control = Arc::new(
        crate::runtime::turn_control::ActiveTurnControl::new(
            host.await_event_resolver(),
            crate::TurnAddress::new(session_id.clone(), turn_id.clone()),
        )
        .await
        .expect("the test host keys a turn's cancellation gate"),
    );
    let stopper = Arc::clone(&control);
    let stop_host = Arc::clone(&host);
    crate::task::spawn(async move {
        stop.cancelled().await;
        stopper
            .request_local_stop(
                host.await_event_resolver(),
                crate::TurnCancelMode::Immediate,
                None,
            )
            .await
            .expect("the test host resolves the cancellation gate");
    });
    context.with_recorded_turn_cancel(
        false,
        control,
        stop_host,
        tokio_util::sync::CancellationToken::new(),
    )
}

/// Build an empty code-execution context carrying the stable parent invocation
/// that production installs around an `ExecCode` effect.
pub fn code_execution_context_with_invocation<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

/// Build a code-execution context with a concrete tool surface and the stable
/// parent invocation production installs around an `ExecCode` effect.
pub fn code_execution_context_with_tool_provider_catalog_and_invocation<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .provider(provider)
        .tool_catalog(tool_catalog)
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

/// Build a concrete code-execution context with an already admitted effect
/// scope. Durable-controller tests use this instead of the shared-controller
/// shortcut, whose intentionally synthetic runtime-operation scope is suitable
/// only for scope-agnostic fakes.
pub fn code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation<
    'run,
>(
    ports: impl Into<TestExecutionPorts<'run>>,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    effect_controller: crate::ScopedEffectController<'run>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
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

fn build_atomic_tool_dispatch<'run>(
    builder: TestExecutionContextBuilder<'run>,
) -> Arc<crate::tool_dispatch::ToolDispatchContext<'run>> {
    builder
        .shared_session_host(Arc::new(MockSessionManager::default()))
        .build()
        .dispatch
}

/// Execute a recorded tool-intent drain through the production process-command
/// route while retaining a small, backend-neutral differential-test surface.
pub async fn execute_tool_intents_with_services(
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    processes: Arc<dyn crate::ProcessService>,
    session_id: &SessionId,
    tool_call_id: &crate::ToolCallId,
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
    process_engines: crate::ProcessEngineRegistry,
    session_id: &SessionId,
    tool_call_id: &crate::ToolCallId,
    intents: &crate::ToolIntents,
) -> Result<Vec<crate::ToolIntentExecutionOutcome>, crate::RuntimeEffectControllerError> {
    execute_tool_intents_with_services_and_hook_and_trigger_router(
        scoped_effect_controller,
        processes,
        Some((trigger_router, process_engines)),
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
    tool_call_id: &crate::ToolCallId,
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
    // The trigger router, with the engines that hold the revisions its
    // registrations commit (ADR 0113 §3.4).
    triggers: Option<(crate::TriggerRouter, crate::ProcessEngineRegistry)>,
    session_id: &SessionId,
    tool_call_id: &crate::ToolCallId,
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
    let builder = TestExecutionContextBuilder::over_controller(scoped_effect_controller)
        .session_id(session_id)
        .session_lifecycle(Arc::new(MockSessionManager::default()))
        .processes(processes)
        .dispatch_parent_invocation(parent_invocation);
    let builder = match triggers {
        Some((router, engines)) => builder
            .trigger_router(Some(router))
            .process_engines(engines),
        None => builder,
    };
    let dispatch = build_atomic_tool_dispatch(builder);
    let context = dispatch.intent_realization_context();
    crate::tool_dispatch::execute_final_tool_intents(
        &context,
        tool_call_id,
        intents,
        child_trace_hook,
    )
    .await
}

/// Build the real engine run context used by validation-path tests that are
/// expected to settle before constructing a nested runtime context.
pub fn process_engine_run_context_for_validation(
    backend: &crate::Backend,
    registration: crate::ProcessRegistration,
    tool_catalog: Arc<crate::ToolCatalog>,
    process_registry_available: bool,
) -> crate::ProcessEngineRunContext<'static> {
    let process_id = crate::mint_process_id();
    let process_work = process_work_wiring_for_registry(backend.process_registry());
    let plugins = crate::PluginHost::new(test_standard_protocol_factories())
        .build_session(PluginSessionRequest::creation(
            "engine-validation-test",
            Default::default(),
        ))
        .expect("test protocol session builds");
    let scoped_effect_controller = backend
        .effect_host()
        .scoped_static(crate::AdmittedScope::process(process_id.clone()))
        .expect("valid process scope")
        .expect("the backend's effect host lends a static controller");
    let execution_context = crate::ProcessExecutionContext::default()
        .with_execution_write_authority(crate::ProcessExecutionWriteAuthority::invocation(
            process_id.clone(),
            "engine-validation-test-execution",
        ));
    crate::ProcessEngineRunContext::new(
        registration,
        process_id,
        execution_context,
        process_work,
        plugins,
        tool_catalog,
        None,
        None,
        Arc::new(crate::NoSessionWork::new()),
        crate::DeliveryPolicy::EarliestSafeBoundary,
        backend.clock(),
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
    /// The cancel command production issues: the retained-process check is
    /// the recorded cancel admission's, never a read ahead of it.
    fn cancel_command(
        process_id: &ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> crate::ProcessCommand {
        crate::ProcessCommand::Cancel {
            process_id: process_id.clone(),
            origin,
            requester,
            attribution,
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
        let scoped = scope.effect_controller.clone();
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
            scope.effect_controller.admitted_scope().clone(),
        )
        .map_err(crate::RuntimeEffectControllerError::from)?;
        let local_executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&self.registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(
                &self.registry,
            ))),
            // The executor admits an engine start against its registry
            // (FIG-1520): the fixture engine is the one these services run.
            process_engine_fixture(),
            crate::runtime::HostStartAdmission::default(),
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
        owner: &crate::RuntimeOwner,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let session_id = crate::plugin::require_session_owner(owner, "list_visible_for_attempt")?;
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
        let registration = admitted_registration(request.into_registration(), &scope)?;
        let command = crate::ProcessCommand::Start {
            registration,
            observers: observers.into_iter().collect(),
            execution_context: Box::new(crate::ProcessExecutionContext::default()),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Start { record, .. } => {
                Ok(crate::ProcessHandleView::from_record(*record))
            }
            _ => unreachable!("start command returns start outcome"),
        }
    }

    async fn start_from_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        let session_id = crate::plugin::require_session_owner(owner, "start_from_recorded_intent")?;
        self.start_from_request(session_id, request, scope).await
    }

    async fn start(
        &self,
        _session_id: &SessionId,
        registration: crate::ProcessStartRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let registration = admitted_registration(registration, &scope)?;
        let command = crate::ProcessCommand::Start {
            registration,
            observers: options.initial_observers.into_iter().collect(),
            execution_context: Box::new(crate::ProcessExecutionContext::default()),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Start { record, .. } => Ok(*record),
            _ => unreachable!("start command returns start outcome"),
        }
    }

    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        let command = crate::ProcessCommand::Await {
            process_id: process_id.clone(),
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
            selection: crate::ProcessListSelection::Observed {
                session_scope: crate::SessionScope::new(session_id),
                mode,
            },
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::List { entries } => Ok(entries),
            _ => unreachable!("list command returns list outcome"),
        }
    }

    async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        if process_ids.is_empty() {
            return Ok(());
        }
        match self
            .execute(
                scope,
                crate::ProcessCommand::ValidateVisible {
                    owner: owner.clone(),
                    process_ids: process_ids.to_vec(),
                },
            )
            .await?
        {
            crate::ProcessEffectOutcome::ValidateVisible { not_visible: None } => Ok(()),
            crate::ProcessEffectOutcome::ValidateVisible {
                not_visible: Some(process_id),
            } => Err(crate::PluginError::ProcessNotVisible { process_id }),
            _ => unreachable!("visibility command returns visibility outcome"),
        }
    }

    async fn cancel(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = Self::cancel_command(
            process_id,
            crate::CancelOrigin::OperatorRequested,
            serde_json::to_string(scope.effect_controller.execution_scope())
                .expect("serializable effect scope"),
            None,
        );
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn cancel_recorded_intent(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = Self::cancel_command(
            process_id,
            crate::CancelOrigin::ModelRequested,
            identity.replay_key.clone(),
            Some(crate::RuntimeReplayAttribution::ToolIntent(identity)),
        );
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn signal_possessed(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let command = crate::ProcessCommand::Signal {
            signal: crate::ProcessSignal::new(
                crate::ProcessSignalIdentity::new(process_id.clone(), signal_name, signal_id)?,
                payload,
            ),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Signal { event } => Ok(*event),
            _ => unreachable!("signal command returns signal outcome"),
        }
    }

    async fn signal_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.signal_possessed(owner, process_id, signal_name, signal_id, payload, scope)
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
            process_id: process_id.clone(),
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
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let session_id = crate::plugin::require_session_owner(owner, "emit_event_recorded_intent")?;
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
/// A runtime start records the start context it was admitted under, as the
/// runtime's realization does (FIG-3607 R1, R2): these test services stand in
/// for that realization.
fn admitted_registration(
    registration: crate::ProcessStartRegistration,
    scope: &crate::ProcessOpScope<'_>,
) -> Result<crate::ProcessStartRegistration, PluginError> {
    match scope
        .start_cx()
        .map_err(|error| PluginError::Session(format!("process start refused: {error}")))?
    {
        Some(cx) => Ok(registration.with_start_cx(&cx)),
        None => Ok(registration),
    }
}

pub fn effect_backed_process_service(
    registry: Arc<dyn crate::ProcessRegistry>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
) -> Arc<dyn crate::ProcessService> {
    Arc::new(EffectBackedProcessService {
        registry,
        process_env_store,
    })
}

/// Convenience helper for the common tool-test shape: build a
/// [`mock_attempt_context`], wrap `name` + `args` in a `ToolCall`, and `await`
/// the provider's `execute`. Use this for unit tests that don't need to
/// inspect host interactions; build a [`ToolCallFixture`] and construct
/// `ToolCall` manually for more involved scenarios.
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
    let context = ToolCallFixture::mock().attempt("test-turn");
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
    let context = crate::AttemptContext::__for_granted_source(
        &ToolCallFixture::mock().context,
        "test-turn",
        grant,
    );
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
            session_id: session_id.clone(),
            policy: SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ),
            ..SessionSnapshot::new(
                session_id.clone(),
                SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
            )
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
        retained_outputs: Vec::new(),
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
    /// The registry the mock's process routes read and write. `None` (the
    /// default) is a session manager with no process port: every process
    /// route refuses, so a tool test that never touches processes needs no
    /// backend, and one that does hands in its backend's registry.
    pub process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
    pub created: Mutex<Vec<SessionCreateRequest>>,
    /// Process terminals armed through
    /// [`ProcessService::attach_process_terminal`](crate::ProcessService::attach_process_terminal),
    /// in arming order.
    ///
    /// This mock cannot observe a real terminal, so it records the arming and
    /// leaves the resolution to the test standing in for the process: a test
    /// that resolves the recorded key is the terminal, and one that never does
    /// is a process that never ended.
    pub terminal_attachments: Mutex<Vec<(crate::ProcessId, crate::AwaitEventKey)>>,
}

impl Default for MockSessionManager {
    fn default() -> Self {
        Self {
            snapshot: RuntimeSessionState::new(mock_session_policy()).to_snapshot(),
            tool_catalog: Vec::new(),
            turn: mock_assembled_turn(&SessionId::from("root"), ""),
            tool_registry: None,
            process_registry: None,
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

    /// Routes the mock's process operations through `process_registry`.
    pub fn with_process_registry(
        mut self,
        process_registry: Arc<dyn crate::ProcessRegistry>,
    ) -> Self {
        self.process_registry = Some(process_registry);
        self
    }

    /// The registry the process routes use, or the refusal a mock with no
    /// process port answers.
    pub fn registry(&self) -> Result<&Arc<dyn crate::ProcessRegistry>, PluginError> {
        self.process_registry.as_ref().ok_or_else(|| {
            PluginError::Session(
                "this mock session manager has no process registry; hand one in with \
                 `MockSessionManager::with_process_registry`"
                    .to_string(),
            )
        })
    }

    /// Snapshot of the requests captured by `create_session`. Panics if
    /// the lock is poisoned (a panic from another test thread).
    pub fn created_snapshot(&self) -> Vec<SessionCreateRequest> {
        self.created.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl crate::ProcessService for MockSessionManager {
    async fn attach_process_terminal(
        &self,
        process_id: &crate::ProcessId,
        key: &crate::AwaitEventKey,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<Option<crate::ProcessAwaitOutput>, PluginError> {
        self.terminal_attachments
            .lock_recover()
            .push((process_id.clone(), key.clone()));
        Ok(None)
    }

    async fn start_from_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, PluginError> {
        let session_id = crate::plugin::require_session_owner(owner, "start_from_recorded_intent")?;
        let observers = request.observers.clone();
        let record = self
            .start(
                session_id,
                request.into_registration(),
                crate::ProcessStartOptions::new().with_initial_observers(observers),
                scope,
            )
            .await?;
        Ok(crate::ProcessHandleView::from_record(record))
    }

    async fn start(
        &self,
        _session_id: &SessionId,
        registration: crate::ProcessStartRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        let registration = admitted_registration(registration, &scope)?
            .stating_input()
            .map_err(|registration| {
                PluginError::Session(format!(
                    "{} names a definition, which this mock executor cannot resolve",
                    registration.refusal_name()
                ))
            })?;
        // This mock stands in as the executor, so it completes the row under
        // the workflow-key authority an executing substrate holds.
        let observers = options.initial_observers;
        let id = self
            .registry()?
            .register_process_with_observers(registration, &observers)
            .await?
            .id;
        let authority = crate::ProcessCompletionAuthority::workflow_key(&id);
        self.registry()?
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
        crate::NoProcessWork::for_registry(Arc::clone(self.registry()?))
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
                self.registry()?.list_live_observed_by(session_id).await
            }
            crate::ProcessListMode::All => {
                self.registry()?
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
        owner: &crate::RuntimeOwner,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let _ = scope;
        let session_id = crate::plugin::require_session_owner(owner, "validate_visible")?;
        for handle_id in handle_ids {
            match self
                .registry()?
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
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        let registry = self.registry()?;
        let process_id = registry.require_process_id(process_id).await?;
        registry
            .request_process_cancel(
                &process_id,
                crate::CancelOrigin::OperatorRequested,
                serde_json::to_string(_scope.effect_controller.execution_scope())
                    .expect("serializable effect scope"),
                None,
            )
            .await
    }

    async fn cancel_recorded_intent(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, PluginError> {
        let registry = self.registry()?;
        let process_id = registry.require_process_id(process_id).await?;
        registry
            .request_process_cancel(
                &process_id,
                crate::CancelOrigin::ModelRequested,
                identity.replay_key.clone(),
                Some(crate::RuntimeReplayAttribution::ToolIntent(identity)),
            )
            .await
    }

    async fn signal_possessed(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let signal = crate::ProcessSignal::new(
            crate::ProcessSignalIdentity::new(process_id.clone(), signal_name, signal_id)?,
            payload,
        );
        self.registry()?
            .append_event(process_id, signal.append_request())
            .await
            .map(|result| result.event)
    }

    async fn signal_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.signal_possessed(owner, process_id, signal_name, signal_id, payload, scope)
            .await
    }

    async fn emit_event_recorded_intent(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.registry()?
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
        self.registry()?
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
    test_code_protocol_factories, test_plugin_host, test_protocol_factories_ending_without_done,
    test_standard_protocol_factories, test_standard_protocol_factory_with_runtime_state,
};

mod test_protocol_fakes;

pub mod conformance_support;
pub mod graph_integrity;
pub mod lineage;
pub mod store_fixtures;
