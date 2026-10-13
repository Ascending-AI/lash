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
use crate::session::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
use lash_sansio::sync::MutexExt;
// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and a
// reader or editor following the merged doc comment — including the
// submodule's own intra-doc links — would resolve it against *this* module's
// scope, where none of the linked items exist.
pub mod behavior_transcript;
#[cfg(feature = "testing")]
pub mod trace_capture;

mod component_fixtures;
mod durable_hooks;
pub mod execution_context_builder;
#[cfg(feature = "testing")]
pub mod kernel_internals;
#[cfg(any(test, feature = "testing"))]
pub mod observation_sink;
#[cfg(feature = "testing")]
pub mod prompt;
pub mod sansio_transcript;
pub mod sql_work;
pub mod tool_fixtures;
pub use component_fixtures::{component_env, native_format_two_component};
pub use durable_hooks::DurableCommitHook;

/// A recording or fault layer over any effect host (FIG-3580).
pub use crate::runtime::process::{
    NonTerminalPagePause, NonTerminalPageRead, ProcessRegistryFaults, RegistrationHoldPoint,
};
pub use execution_context_builder::*;
#[cfg(any(test, feature = "testing"))]
pub use observation_sink::ChannelObservationSink;
pub use tool_fixtures::{FIXTURE_ECHO_TOOL, FixtureTools, fixture_echo_definition};

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

/// The fixed fixture process environment: no tools, no components, a
/// tool-call limit of 1024.
#[cfg(any(test, feature = "testing"))]
pub fn process_env_fixture() -> crate::ProcessEnv {
    process_env_with_limit(1024)
}

/// An environment whose one component, `fixture-payload`, carries a config
/// of about `bytes` bytes: the size of a large stated environment. No
/// registry installs the component, so it is written to a store directly,
/// never admitted.
#[cfg(any(test, feature = "testing"))]
#[expect(clippy::expect_used, reason = "fixture environment")]
pub fn process_env_of_size(bytes: usize) -> crate::ProcessEnv {
    const PAYLOAD: crate::ProcessConfigKey<serde_json::Value> =
        crate::ProcessConfigKey::new("fixture-payload", crate::FormatVersion::ONE);
    crate::ProcessEnv::builder(crate::MaxToolCalls::new(1024))
        .component(
            PAYLOAD,
            &serde_json::json!({ "payload": "x".repeat(bytes) }),
        )
        .build()
        .expect("the fixture process environment encodes")
}

/// The tool a test's own process steps record for `step` of an `engine`
/// process, which links no env: the effect's path stands in for the tool a
/// link names, and an engine step records its body's id.
#[cfg(any(test, feature = "testing"))]
#[must_use]
pub fn step_tool(step: &crate::StepRequest, engine: &str) -> crate::ToolId {
    match step {
        crate::StepRequest::Tool { effect, .. } => crate::ToolId::new(effect.as_str()),
        crate::StepRequest::Engine { kind, .. } => kind.admitted_tool(engine),
    }
}

/// The effect name `name` spells, for a test's own engine.
#[cfg(any(test, feature = "testing"))]
#[expect(clippy::expect_used, reason = "fixture effect names")]
#[must_use]
pub fn effect(name: &str) -> crate::EffectName {
    crate::EffectName::new(name).expect("a fixture effect name")
}

/// The effect `definition`'s binding names (`module.operation`): the path
/// an env links it under and an engine's step performs.
#[cfg(any(test, feature = "testing"))]
#[expect(clippy::expect_used, reason = "fixture tools are bound")]
#[must_use]
pub fn effect_of(definition: &crate::ToolDefinition) -> crate::EffectName {
    effect(&crate::effect_path_of(&definition.manifest).expect("a fixture tool is bound"))
}

/// What an engine that performs each of `tools` declares (P3): each tool's
/// effect, under the signature its schemas project to.
#[cfg(any(test, feature = "testing"))]
#[must_use]
pub fn performs(tools: &[crate::ToolDefinition]) -> crate::PerformedEffects {
    tools
        .iter()
        .map(|tool| {
            (
                effect_of(tool),
                lash_kernel_check::tool_signature(
                    tool.contract.input_schema.canonical(),
                    tool.contract.output_schema.canonical(),
                ),
            )
        })
        .collect()
}

/// A component-free environment that admits no tool and holds at most
/// `max_tool_calls` calls at once: distinct limits are distinct environments.
#[cfg(any(test, feature = "testing"))]
#[expect(clippy::expect_used, reason = "fixture environment")]
pub fn process_env_with_limit(max_tool_calls: usize) -> crate::ProcessEnv {
    crate::ProcessEnv::builder(crate::MaxToolCalls::new(max_tool_calls))
        .build()
        .expect("the fixture process environment encodes")
}

/// Publishes the fixed fixture execution environment into `env_store`, held
/// by a fresh host pin the fixture never releases, and returns its reference.
#[cfg(any(test, feature = "testing"))]
pub async fn process_execution_env_fixture(
    env_store: &dyn crate::ProcessEnvStore,
) -> crate::ProcessEnvRef {
    publish_process_env_for_testing(
        env_store,
        &host_pin_claim_for_testing(),
        &process_env_fixture(),
    )
    .await
    .expect("fixed process execution environment fixture publishes")
}

/// Collect a fixture's roster through single-record pages, exercising continuations.
#[cfg(any(test, feature = "testing"))]
pub async fn process_roster_records_for_fixture(
    registry: &dyn crate::ProcessQuery,
    filter: &crate::ProcessListFilter,
) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
    let mut records = Vec::new();
    let mut continuation = None;
    loop {
        let page = registry
            .list_processes_page(filter, std::num::NonZeroUsize::MIN, continuation)
            .await?;
        records.extend(page.records);
        continuation = page.continuation;
        if continuation.is_none() {
            return Ok(records);
        }
    }
}

/// The reference [`process_execution_env_fixture`] publishes under. An
/// environment reference is content-addressed, so it names the fixture
/// environment in every store: a subscription draft that records the
/// reference, and never loads it, takes this instead of publishing.
#[cfg(any(test, feature = "testing"))]
pub fn process_execution_env_fixture_ref() -> crate::ProcessEnvRef {
    process_env_fixture()
        .stable_ref()
        .expect("fixed process execution environment fixture encodes")
}

/// The launch of an engine start under the fixture environment, with no
/// engine settings.
#[cfg(any(test, feature = "testing"))]
pub fn fixture_engine_launch() -> crate::EngineLaunch {
    crate::EngineLaunch::new(process_execution_env_fixture_ref(), None)
}

/// Publish `env` under `claim` through the one hold path, admitted against
/// no installed component: what a fixture whose environment names none uses.
#[cfg(any(test, feature = "testing"))]
pub async fn publish_process_env_for_testing(
    env_store: &dyn crate::ProcessEnvStore,
    claim: &crate::ReferrerClaim,
    env: &crate::ProcessEnv,
) -> Result<crate::ProcessEnvRef, crate::PluginError> {
    let components = crate::plugin::ProcessComponentRegistry::new();
    crate::hold_process_env(
        &crate::ProcessEnvPorts::admitting(env_store, &components),
        claim,
        &crate::ProcessEnvSelection::New(env.clone()),
    )
    .await
    .map_err(|error| match error {
        crate::HoldEnvError::Refused(refusal) => crate::PluginError::ProcessEnvRefused { refusal },
        crate::HoldEnvError::Fault(error) => error,
    })
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

#[cfg(any(test, feature = "testing"))]
mod process_fixtures;
#[cfg(any(test, feature = "testing"))]
pub use process_fixtures::{
    FixtureProcessEngine, HELD_PROCESS_ENGINE_KIND, HeldProcessEngine, declared_launch_for_testing,
    held_engine_declaration, held_engine_input, held_engine_launch, held_engine_registration,
    held_engine_request, process_engine_fixture, process_engine_plugin_fixture, run_policy_naming,
    tool_component, tool_plugin,
};

use crate::llm::transport::LlmTransportError;
use crate::llm::types::{LlmRequest, LlmResponse};
use crate::plugin::{PluginError, SessionCreateRequest, SessionHandle, SessionSnapshot};
use crate::provider::{Provider, ProviderComponents, ProviderHandle};
use crate::session_model::{ConversationRecord, SessionHistoryRecord};
use crate::{
    AssembledTurn, LlmUsage, ProviderOptions, RuntimeSessionState, SessionPolicy,
    TurnExecutionMetrics, TurnFinish, TurnOutcome, TurnStop,
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
        crate::LeaseOwnerId::new("lash-core-test-worker"),
        crate::LeaseIncarnationId::new(format!("lash-core-test-boot:{}", uuid::Uuid::new_v4())),
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

/// Stage a protocol-owned SessionExecution capture into resident session
/// state, exactly as the runtime's turn boundary does.
///
/// The staging mutator itself is runtime-owned (ADR 0051's "neither" class), so
/// protocol crates reach it through this fixture rather than through
/// `RuntimeSessionState`'s public surface.
pub fn stage_execution_graph(
    state: &mut RuntimeSessionState,
    capture: crate::plugin::ExecutionGraphCapture,
) {
    state.set_execution_graph(capture);
}

type CompletionFuture =
    Pin<Box<dyn Future<Output = Result<LlmResponse, LlmTransportError>> + Send>>;
type CompletionFn = dyn Fn(LlmRequest, String) -> CompletionFuture + Send + Sync;
type AnswerFn =
    dyn Fn(lash_sansio::llm::types::ResponseContext, String) -> CompletionFuture + Send + Sync;
type TemplateFn =
    dyn Fn(&LlmRequest) -> lash_sansio::llm::types::RecordedRequestTemplate + Send + Sync;
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
    /// Optional template builder for admission laws; otherwise canonical refs.
    template: Option<Arc<TemplateFn>>,
    complete: Arc<CompletionFn>,
    /// Set by [`TestProviderBuilder::answer`]: it answers instead of
    /// `complete`, from the wire and no decoded request.
    answer: Option<Arc<AnswerFn>>,
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
                template: None,
                answer: None,
                complete: Arc::new(|_request, _body| {
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

    /// Answer each send from the request its body says: the canonical
    /// body read back ([`crate::provider::canonical_request`]), never a request held beside
    /// it. A provider whose [`template`](Self::template) is not canonical
    /// answers through [`answer`](Self::answer) instead.
    pub fn complete<F, Fut>(mut self, complete: F) -> Self
    where
        F: Fn(LlmRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<LlmResponse, LlmTransportError>> + Send + 'static,
    {
        self.provider.complete = Arc::new(move |request, _body| Box::pin(complete(request)));
        self
    }

    /// Answer each send from the request its body says and the exact wire
    /// it sends.
    pub fn complete_with_wire<F, Fut>(mut self, send: F) -> Self
    where
        F: Fn(LlmRequest, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<LlmResponse, LlmTransportError>> + Send + 'static,
    {
        self.provider.complete = Arc::new(move |request, body| Box::pin(send(request, body)));
        self
    }

    /// Answer each send from its response context and the exact wire it
    /// sends, decoding nothing: for a provider whose
    /// [`template`](Self::template) lowers to a body of its own.
    pub fn answer<F, Fut>(mut self, answer: F) -> Self
    where
        F: Fn(lash_sansio::llm::types::ResponseContext, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<LlmResponse, LlmTransportError>> + Send + 'static,
    {
        self.provider.answer = Some(Arc::new(move |context, body| {
            Box::pin(answer(context, body))
        }));
        self
    }

    /// Build the recorded template for tests of admission and takeover.
    pub fn template<F>(mut self, template: F) -> Self
    where
        F: Fn(&LlmRequest) -> lash_sansio::llm::types::RecordedRequestTemplate
            + Send
            + Sync
            + 'static,
    {
        self.provider.template = Some(Arc::new(template));
        self
    }

    pub fn complete_error(mut self, message: impl Into<String>) -> Self {
        let message = Arc::new(message.into());
        self.provider.complete = Arc::new(move |_request, _body| {
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

    fn attachment_accepts(
        &self,
        _model: &str,
        _mime: &lash_sansio::MediaType,
        _position: lash_sansio::llm::attachment_delivery::AttachmentPosition,
    ) -> lash_sansio::llm::attachment_delivery::ProviderAccepts {
        lash_sansio::llm::attachment_delivery::ProviderAccepts {
            bytes: true,
            url: true,
            provider_file: None,
        }
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

    async fn lower(
        &mut self,
        request: &LlmRequest,
    ) -> Result<lash_sansio::llm::types::RecordedRequestTemplate, LlmTransportError> {
        match &self.template {
            Some(template) => Ok(template(request)),
            None => lash_sansio::llm::types::RecordedRequestTemplate::of_request(
                self.route_identity(request.model.wire_model()),
                request,
            )
            .map_err(crate::provider::attachment_wire::template_error),
        }
    }

    async fn send(
        &mut self,
        body: &lash_sansio::llm::types::LiveRequestBody,
        context: lash_sansio::llm::types::ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        let answered = match &self.answer {
            Some(answer) => answer(context, body.wire()).await,
            None => match crate::provider::canonical_request(body, context) {
                Ok(request) => (self.complete)(request, body.wire()).await,
                Err(error) => Err(error),
            },
        };
        let mut response = answered?;
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
        _context: &lash_sansio::llm::types::ResponseContext,
        _template: &lash_sansio::llm::types::RecordedRequestTemplate,
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
        .cache_retention(crate::provider::CacheRetention::Short)
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
        ..crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )
    }
}

mod hook_context;
pub use hook_context::{mock_session_policy, tool_hook_context};

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
    /// The run the call runs inside, when the test names one.
    pub(crate) resolved_run: Option<Arc<crate::ResolvedRun>>,
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
        let session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService> = host;
        Self {
            context: crate::tool_provider::ToolContext::builder(
                SessionId::from("test-session"),
                sessions,
                session_lifecycle,
                Arc::new(crate::plugin::ServiceBindings::default()),
                crate::ActorContext::unavailable()
                    .scoped(crate::AdmittedScope::runtime_operation(
                        "test-runtime-effect-controller",
                    ))
                    .expect("valid test runtime scope"),
                Arc::new(crate::RuntimeAttachmentStore::unavailable()),
                direct_completions,
            )
            .build(),
            resolved_run: None,
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
            resolved_run: None,
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

    /// Sets the execution binding the call runs with.
    pub fn execution_binding(mut self, binding: Option<crate::ExecutionBinding>) -> Self {
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
    pub fn scoped_effect_controller(mut self, scoped: crate::ActorContext) -> Self {
        self.context.effect_controller = scoped;
        self
    }

    /// The attempt context a body executing this call under
    /// `execution_scope_id` receives. No completion key is reserved: a test
    /// harness is not the attempt coordinator.
    /// Run the call inside a turn whose recorded shape resolved `spec` over
    /// an ambient session config, as `AttemptContext::run_spec` reads it.
    pub fn running(mut self, spec: crate::RunSpec) -> Self {
        let base = crate::PersistedSessionConfig::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        );
        self.resolved_run = Some(Arc::new(
            crate::ResolvedRun::snapshot(base)
                .with_spec(spec)
                .expect("a fixture spec encodes"),
        ));
        self
    }

    pub fn attempt(&self, execution_scope_id: impl Into<String>) -> crate::AttemptContext<'run> {
        let mut attempt = crate::AttemptContext::from_tool_context(
            &self.context,
            execution_scope_id.into(),
            crate::tool_provider::AttemptCompletionSupport::NotDeclared,
        );
        if let Some(run) = &self.resolved_run {
            attempt.resolved_run = Some(Arc::clone(run));
        }
        attempt
    }
}

/// Tool bodies take [`crate::AttemptContext`], so this is the context a unit
/// test hands a provider's `execute`.
pub fn mock_attempt_context() -> crate::AttemptContext<'static> {
    ToolCallFixture::mock().attempt("test-turn")
}

/// Like [`mock_attempt_context`], but with a deferred match's execution
/// binding populated, for provider tests that assert match-only routing.
pub fn mock_attempt_context_with_execution_binding(
    binding: crate::ExecutionBinding,
) -> crate::AttemptContext<'static> {
    ToolCallFixture::mock()
        .execution_binding(Some(binding))
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
            .with_tool_execution_binding(grant.binding.clone())
            .with_granted_provider(grant.provider.clone());
        Self::from_tool_context(
            &context,
            execution_scope_id.into(),
            crate::tool_provider::AttemptCompletionSupport::NotDeclared,
        )
    }
}

/// Run one effect through its local executor with no journal: the execution
/// half of a controller-owned test double, which keeps its own journal.
///
/// A process command runs on a task of
/// its own, as a host runs it, so a panicking process is contained as a typed
/// failure rather than unwinding the double.
pub async fn execute_effect_locally(
    envelope: crate::RuntimeEffectEnvelope,
    local_executor: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
    match envelope.command {
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
        _ => local_executor.execute(envelope).await,
    }
}

/// The process-exec-env port of a context with no store: publication,
/// acquisition and writer provisioning are refused, a cleanup has nothing to
/// sever, and reads find nothing (no fleet record among them).
///
/// It keeps nothing, so it is not a persistence store: it stands where a test
/// needs *a* port it never publishes through.
#[derive(Debug, Default)]
pub struct UnavailableProcessEnvStore;

impl UnavailableProcessEnvStore {
    fn refused() -> crate::ArtifactStoreError {
        crate::ArtifactStoreError::Backend(
            "this context has no process execution environment store".into(),
        )
    }
}

impl crate::store::FleetFormatStore for UnavailableProcessEnvStore {
    fn fleet_format(&self) -> crate::FleetFormat {
        crate::FleetFormat::current()
    }

    fn provision_plugin_writers<'a>(
        &'a self,
        _registrations: &'a [crate::store::plugin_writers::PluginWriterRegistration],
    ) -> crate::store::PluginWriterRangesFuture<'a> {
        Box::pin(async {
            Err(crate::StoreError::Backend(
                "this context has no process execution environment store".into(),
            ))
        })
    }
}

#[async_trait::async_trait]
impl crate::ProcessEnvStore for UnavailableProcessEnvStore {
    async fn publish_process_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &crate::ProcessEnvRef,
        _bytes: &[u8],
    ) -> Result<(), crate::ArtifactStoreError> {
        Err(Self::refused())
    }

    async fn acquire_process_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &crate::ProcessEnvRef,
    ) -> Result<(), crate::ArtifactStoreError> {
        Err(Self::refused())
    }

    async fn end_process_env_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn get_process_env(
        &self,
        _env_ref: &crate::ProcessEnvRef,
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
        Arc::new(UnavailableProcessEnvStore),
    )
}

pub struct EmptyToolProvider;

#[async_trait::async_trait]
impl crate::ToolProvider for EmptyToolProvider {
    fn tool_manifests(
        &self,
        _config: &crate::plugin::OwnerConfigView,
    ) -> std::result::Result<Vec<crate::ToolManifest>, crate::PluginError> {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<Arc<crate::ToolContract>> {
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
    ports: impl Into<TestExecutionPorts>,
    tool_catalog: crate::ToolCatalog,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .tool_catalog(tool_catalog)
        .build()
        .into_runtime()
}

pub fn code_execution_context_with_tool_provider_and_catalog<'run>(
    ports: impl Into<TestExecutionPorts>,
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
    ports: impl Into<TestExecutionPorts>,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    processes: Arc<dyn crate::ProcessService>,
    facts: crate::plugin::ExecutionFacts,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .provider(provider)
        .tool_catalog(tool_catalog)
        .processes(processes)
        .facts(facts)
        .build()
        .into_runtime()
}

pub fn code_execution_context<'run>(
    ports: impl Into<TestExecutionPorts>,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .build()
        .into_runtime()
}

/// Build an empty code-execution context for a specific durable process.
#[cfg(any(test, feature = "testing"))]
pub fn code_execution_context_for_process<'run>(
    ports: impl Into<TestExecutionPorts>,
    process_id: crate::ProcessId,
    registration: &crate::ProcessRegistration,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .build()
        .into_runtime()
        .with_process_execution(
            &crate::ProcessRecord::from_registration(registration.clone(), process_id),
            None,
            None,
        )
}

/// Build an empty code-execution context whose cancellation is already visible.
pub fn cancelled_code_execution_context<'run>(
    ports: impl Into<TestExecutionPorts>,
) -> crate::RuntimeExecutionContext<'run> {
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    TestExecutionContextBuilder::new(ports.into())
        .build()
        .into_runtime()
        .with_cancellation_token(cancellation)
}

/// Build an empty code-execution context carrying the stable parent invocation
/// that production installs around an `ExecCode` effect.
pub fn code_execution_context_with_invocation<'run>(
    ports: impl Into<TestExecutionPorts>,
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
    ports: impl Into<TestExecutionPorts>,
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
    ports: impl Into<TestExecutionPorts>,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    effect_controller: crate::ActorContext,
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

/// [`code_execution_context_with_tool_provider_catalog_and_invocation`]
/// whose surface records `deferred`, declared by `owner` as `key`, as its
/// deferred provider.
pub fn code_execution_context_with_deferred_provider<'run>(
    ports: impl Into<TestExecutionPorts>,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    invocation: crate::RuntimeInvocation,
    (handle, deferred): (
        crate::plugin::ToolProviderHandle,
        Arc<dyn crate::DeferredToolProvider>,
    ),
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .provider(provider)
        .tool_catalog(tool_catalog)
        .deferred_provider(handle, deferred)
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
/// Its starts come back staged: the outcome commit that records
/// the call, which this drain has none of, is what writes them.
pub async fn execute_tool_intents_with_services(
    scoped_effect_controller: crate::ActorContext,
    processes: Arc<dyn crate::ProcessService>,
    session_id: &SessionId,
    tool_call_id: &crate::ToolCallId,
    intents: &crate::ToolIntents,
) -> Result<crate::tool_dispatch::Realization, crate::RuntimeEffectControllerError> {
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

/// The record a staged process start registers once the outcome commit that
/// carries it lands; `None` for any other effect.
///
/// # Errors
///
/// Rows that do not decode, or a registration its admission refuses.
pub fn staged_start_record(
    effect: &crate::runtime::actor::round::StoreLocalEffect,
) -> Result<Option<crate::ProcessRecord>, PluginError> {
    match effect {
        crate::runtime::actor::round::StoreLocalEffect::ProcessStart(rows) => {
            crate::runtime::StagedRegistration::decode(rows)?
                .record()
                .map(Some)
        }
        _ => Ok(None),
    }
}

/// Execute a recorded tool-intent drain through the production process-command
/// route and notify a test hook after a child Start has committed.
pub async fn execute_tool_intents_with_services_and_hook(
    scoped_effect_controller: crate::ActorContext,
    processes: Arc<dyn crate::ProcessService>,
    session_id: &SessionId,
    tool_call_id: &crate::ToolCallId,
    intents: &crate::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<crate::tool_dispatch::Realization, crate::RuntimeEffectControllerError> {
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

/// A `ProcessService` that applies the production command-runner guard, then
/// routes **every** `ProcessCommand` through the operation scope's effect
/// controller exactly as `ProcessCommandRunner` does.
///
/// The FIG-1127 review found that stubbing the sibling routes here made the
/// harness structurally incapable of reaching `await_process`, `cancel`,
/// `list_visible` and `transfer` — the routes that turned out to be
/// unguarded. The stubs are gone: this service now mirrors the production
/// routing decision per method, so a route that journals in production journals
/// here too.
struct EffectBackedProcessService {
    registry: Arc<dyn crate::ProcessRegistry>,
    process_env_store: Arc<dyn crate::ProcessEnvStore>,
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
        .with_process_actor_parks(Arc::clone(scope.effect_controller.backend().durable()));
        let outcome = scoped
            .process_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::process(command),
                ),
                local_executor,
            )
            .await?;
        outcome.into_process().map_err(crate::PluginError::from)
    }

    /// The process executor production stages a call's starts on, over
    /// this service's stores.
    fn stager(&self) -> Result<crate::runtime::ProcessLocalExecution, crate::PluginError> {
        crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&self.registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(
                &self.registry,
            ))),
            process_engine_fixture(),
            crate::runtime::HostStartAdmission::default(),
        )
        .with_process_env_store(Arc::clone(&self.process_env_store))
        .into_process()
        .map_err(crate::PluginError::from)
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
    process_env_store: Arc<dyn crate::ProcessEnvStore>,
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
    let Some(manifest) = tool.resolve_manifest(name, &crate::plugin::OwnerConfigView::default())
    else {
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

pub fn mock_assembled_turn(session_id: &SessionId, summary: &str) -> AssembledTurn {
    AssembledTurn {
        state: SessionSnapshot {
            session_id: session_id.clone(),
            policy: SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                crate::NoProgressBudget::bounded(12),
            ),
            ..SessionSnapshot::new(
                session_id.clone(),
                SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
                ),
            )
        },
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: summary.to_string(),
        }),
        execution: TurnExecutionMetrics::default(),
        token_usage: LlmUsage::default(),
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
    /// The registry the mock's process routes read and write. `None` (the
    /// default) is a session manager with no process port: every process
    /// route refuses, so a tool test that never touches processes needs no
    /// backend, and one that does hands in its backend's registry.
    pub process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
    pub created: Mutex<Vec<SessionCreateRequest>>,
}

impl Default for MockSessionManager {
    fn default() -> Self {
        Self {
            snapshot: RuntimeSessionState::fixture(mock_session_policy()).to_snapshot(),
            tool_catalog: Vec::new(),
            turn: mock_assembled_turn(&SessionId::from("root"), ""),
            process_registry: None,
            created: Mutex::new(Vec::new()),
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

    /// The process executor production stages a call's starts on, over
    /// the mock's registry.
    fn stager(&self) -> Result<crate::runtime::ProcessLocalExecution, PluginError> {
        crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(self.registry()?),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(
                self.registry()?,
            ))),
            process_engine_fixture(),
            crate::runtime::HostStartAdmission::default(),
        )
        .into_process()
        .map_err(PluginError::from)
    }

    /// Snapshot of the requests captured by `create_session`. Panics if
    /// the lock is poisoned (a panic from another test thread).
    pub fn created_snapshot(&self) -> Vec<SessionCreateRequest> {
        self.created.lock_recover().clone()
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

pub mod committed_turns_law;
pub mod graph_integrity;
pub mod lineage;
pub mod store_fixtures;
pub mod turn_feed_law;

mod process_services;

#[cfg(test)]
pub(crate) use test_protocol_fakes::register_inventory_protocol;

mod callback_counts;
pub use callback_counts::CallbackCounts;
