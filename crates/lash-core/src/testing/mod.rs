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

// The execution kernel's fixtures downstream test crates reach through
// `lash_core::testing`, named one by one: the kernel's `kernel_internals` seam
// serves its own store-backed tests and is not re-exported here.
pub use lash_core_execution::testing::{
    EffectLayer, EmptyToolProvider, FIXTURE_ECHO_TOOL, FixtureProcessEngine, FixtureTools,
    LayeredEffectHost, MockSessionManager, RuntimeCommitBudgetMeasurement, TestClock,
    TestExecutionContextBuilder, TestExecutionPorts, TestProvider, TestProviderBuilder,
    UnavailableEffectController, UnavailableProcessExecutionEnvStore, attempt_sentinel,
    behavior_transcript, cancelled_code_execution_context, code_execution_context,
    code_execution_context_cancelling_after_yield, code_execution_context_for_process,
    code_execution_context_with_invocation, code_execution_context_with_process_dependencies,
    code_execution_context_with_tool_catalog,
    code_execution_context_with_tool_provider_and_catalog,
    code_execution_context_with_tool_provider_catalog_and_invocation,
    code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation,
    code_execution_context_with_trigger_store,
    code_execution_context_with_trigger_store_and_invocation,
    coordinate_tool_provider_with_services, effect_backed_process_service, exec_code_invocation,
    execute_tool_intents_with_services, execute_tool_intents_with_services_and_hook,
    execute_tool_intents_with_services_and_trigger_router, fixture_echo_definition,
    graph_integrity, in_memory_lineage_handles, lineage, measure_runtime_commit_budget,
    mock_assembled_turn, mock_attempt_context, mock_attempt_context_from,
    mock_attempt_context_with_completion_key, mock_attempt_context_with_execution_binding,
    mock_session_policy, mock_tool_context, mock_tool_context_with_execution_binding,
    mock_tool_context_with_host_and_direct_completions, process_engine_fixture,
    process_engine_plugin_fixture, process_engine_run_context_for_validation,
    process_execution_env_fixture, process_execution_env_fixture_ref,
    process_work_wiring_for_registry, publish_process_execution_env_for_testing,
    queued_lane_holder_for_testing, queued_work_claim_policy, run_tool, run_tool_granted,
    runbook_evidence, runtime_lease_owner, runtime_services_without_ports, sansio_transcript,
    stage_execution_state_components, standard_test_policy, store_fixtures,
    test_code_protocol_factories, test_plugin_host, test_standard_protocol_factories,
    test_standard_protocol_factory_with_runtime_state, test_trigger_router,
    tool_registry_with_live_provider, trace_capture, with_engine_child_max_attempts,
};

// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and a
// reader or editor following the merged doc comment — including the
// submodule's own intra-doc links — would resolve it against *this* module's
// scope, where none of the linked items exist.
pub mod adversarial_text;
pub mod checkpoint_observer;
pub mod conformance_support;
mod live_replay;
pub mod runtime_helpers;
#[cfg(feature = "testing")]
pub mod runtime_internals;

/// Marks resident state stale for downstream reload-race tests.
#[cfg(any(test, feature = "testing"))]
pub fn invalidate_resident_session_state_for_testing(runtime: &mut crate::LashRuntime) {
    runtime.invalidate_resident_session_state();
}

/// Synthesize the response produced when a plugin aborts an in-flight LLM
/// stream after `events` have reached core's stream accumulator.
///
/// This test seam deliberately uses the production accumulator and its
/// empty-response branch: on the abort path, those accumulated parts are the
/// whole response rather than gap-fill input for a provider completion.
pub fn response_synthesized_from_aborted_stream(
    events: &[crate::llm::types::LlmStreamEvent],
) -> crate::llm::types::LlmResponse {
    crate::runtime::response_synthesized_from_aborted_stream(events)
}
