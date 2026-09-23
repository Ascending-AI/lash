// Test-support module: these helpers run inside a test and a broken fixture
// assumption must abort it loudly rather than be reshaped into a runtime error
// the test under way would then report as a runtime defect. Clippy's
// `allow-expect-in-tests` reaches `#[test]` functions only, not the fixtures
// they call.
#![expect(
    clippy::expect_used,
    reason = "test-support fixtures: a broken setup assumption aborts the test"
)]

pub use lash_core::TestLocalProcessRegistry;
/// Derives a durable frame-node identity through the runtime's canonical
/// producer for integration fixtures that need to enqueue frame-scoped work.
pub use lash_core::facade_support::frame_node_id;
pub use lash_core::testing::run_tool;
/// Runs one granted tool call with mock contexts, so a provider's granted
/// branch is exercisable outside a live turn.
pub use lash_core::testing::run_tool_granted;
/// A standalone [`ToolRegistry`](crate::tools::ToolRegistry) plus the
/// [`ToolSourceHandle`](crate::tools::ToolSourceHandle) `provider` registered
/// under — the same live-source route `session.admin().tools().add_provider`
/// takes, for host tests that exercise source routing without a live session.
pub use lash_core::testing::tool_registry_with_live_provider;
/// A recording or fault layer over any effect host: the host lends its inner
/// host's scoped controllers with the layer in front of their seam operations,
/// and every group and journal operation stays the inner host's.
pub use lash_core::testing::{EffectLayer, LayeredEffectHost};
pub use lash_core::testing::{
    MockSessionManager, TestClock, TestProvider, TestProviderBuilder, mock_attempt_context,
    mock_tool_context, mock_tool_context_with_execution_binding, test_code_protocol_factories,
};

/// [`RuntimeExecutionContext`](crate::tools::RuntimeExecutionContext)
/// constructors for host tests that drive context-bound execution —
/// e.g. [`tools::link_with_deferred_resolution`](crate::tools::link_with_deferred_resolution)
/// — without a production runtime.
///
/// Each takes the [`TestExecutionPorts`] it runs over: a backend's ports
/// (`&backend` converts), or a host's with
/// [`TestExecutionPorts::over_host`]. There is no in-memory default.
pub use lash_core::testing::{
    TestExecutionPorts, cancelled_code_execution_context, code_execution_context,
    code_execution_context_cancelling_after_yield, code_execution_context_for_process,
    code_execution_context_with_invocation, code_execution_context_with_process_dependencies,
    code_execution_context_with_tool_catalog,
    code_execution_context_with_tool_provider_and_catalog,
    code_execution_context_with_tool_provider_catalog_and_invocation,
    code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation,
    exec_code_invocation,
};

/// The [`DeferredResolutionLinkKey`](crate::tools::DeferredResolutionLinkKey) a
/// deferred link admits for an [`exec_code_invocation`]-built invocation —
/// the infallible counterpart of
/// [`DeferredResolutionLinkKey::from_exec_code_invocation`](crate::tools::DeferredResolutionLinkKey::from_exec_code_invocation)
/// for a fixture known to carry an admitted `ExecCode` effect address. Seed a
/// [`DeferredResolutionRecord`](crate::tools::DeferredResolutionRecord) with it
/// when a host test drives
/// [`link_with_deferred_resolution`](crate::tools::link_with_deferred_resolution).
#[cfg(feature = "rlm")]
pub fn deferred_resolution_link_key(
    invocation: &lash_core::RuntimeInvocation,
) -> crate::tools::DeferredResolutionLinkKey {
    crate::tools::DeferredResolutionLinkKey::from_exec_code_invocation(invocation)
        .expect("an exec_code_invocation carries an admitted ExecCode effect address")
}

#[cfg(test)]
pub(crate) fn runtime_lease_owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("lash-runtime-test-worker", "lash-runtime-test-boot")
}

/// The normalized behavior-transcript vocabulary. Render a scenario's real facts
/// into it and pin the result with an inline `insta` snapshot; see
/// `docs/adr/0050-behavior-transcripts-are-one-normalized-vocabulary.md`.
pub use lash_core::testing::behavior_transcript;

/// Store-factory decorator that observes accepted runtime-checkpoint commits, so
/// a scenario can render durable-write transcript lines from real facts.
pub use lash_core::testing::checkpoint_observer;

/// Store-construction fixtures shared by kernel tests and certification
/// scenarios: session-store requests, lease claims, commit helpers, and the
/// completion-deferral authorization seam.
pub use lash_core::testing::store_fixtures;

/// Runtime rebuild certification: the cold-rebuild and worker-recovery suite a
/// host runs against its durable backend. The durable-store laws are not
/// re-exported here; a host certifies its stores through
/// `lash-internal-conformance` directly.
#[cfg(feature = "rlm")]
mod rebuild;
#[cfg(feature = "rlm")]
pub use rebuild::{RuntimeRebuildBackend, runtime_rebuild_and_worker_recovery};
