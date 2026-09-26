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
    LayeredEffectHost, MockSessionManager, ProcessRegistryFaults, RegistrationHoldPoint,
    RuntimeCommitBudgetMeasurement, TestClock, TestExecutionContextBuilder, TestExecutionPorts,
    TestProvider, TestProviderBuilder, UnavailableEffectController,
    UnavailableProcessExecutionEnvStore, WorklistPagePause, WorklistPageRead, attempt_sentinel,
    behavior_transcript, cancelled_code_execution_context, code_execution_context,
    code_execution_context_cancelling_after_yield, code_execution_context_for_process,
    code_execution_context_stopped_on, code_execution_context_with_invocation,
    code_execution_context_with_process_dependencies, code_execution_context_with_tool_catalog,
    code_execution_context_with_tool_provider_and_catalog,
    code_execution_context_with_tool_provider_catalog_and_invocation,
    code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation,
    code_execution_context_with_trigger_store,
    code_execution_context_with_trigger_store_and_invocation,
    coordinate_tool_provider_with_services, effect_backed_process_service, exec_code_invocation,
    execute_effect_locally, execute_tool_intents_with_services,
    execute_tool_intents_with_services_and_hook,
    execute_tool_intents_with_services_and_trigger_router, fixture_echo_definition,
    graph_integrity, lineage, measure_runtime_commit_budget, mock_assembled_turn,
    mock_attempt_context, mock_attempt_context_from, mock_attempt_context_with_completion_key,
    mock_attempt_context_with_execution_binding, mock_session_policy, mock_tool_context,
    mock_tool_context_with_execution_binding, mock_tool_context_with_host_and_direct_completions,
    process_engine_fixture, process_engine_plugin_fixture,
    process_engine_run_context_for_validation, process_execution_env_fixture,
    process_execution_env_fixture_ref, process_work_wiring_for_registry,
    publish_process_execution_env_for_testing, queued_lane_holder_for_testing,
    queued_work_claim_policy, run_tool, run_tool_granted, runbook_evidence, runtime_lease_owner,
    runtime_services_without_ports, sansio_transcript, stage_execution_state_components,
    standard_test_policy, store_fixtures, test_code_protocol_factories, test_plugin_host,
    test_standard_protocol_factories, test_standard_protocol_factory_with_runtime_state,
    test_trigger_router, tool_registry_with_live_provider, trace_capture,
    with_engine_child_max_attempts,
};

// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and a
// reader or editor following the merged doc comment — including the
// submodule's own intra-doc links — would resolve it against *this* module's
// scope, where none of the linked items exist.
pub mod adversarial_text;
pub mod checkpoint_observer;
pub mod conformance_support;
#[cfg(test)]
mod kernel_door_tests;
mod layered_backend;
mod live_replay;
mod recording_store;
pub mod runtime_helpers;
#[cfg(feature = "testing")]
pub mod runtime_internals;

#[cfg(test)]
std::thread_local! {
    /// The SQLite backends the running unit test opened. A memory backend's
    /// databases live while any handle does, and its stores reach sibling
    /// databases by name, so a fixture that hands out only a store would
    /// otherwise let those vanish under it. Each test runs on its own thread,
    /// so this holds every backend exactly as long as the test that opened it.
    static TEST_BACKENDS: std::cell::RefCell<Vec<lash_sqlite_store::SqliteBackend>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory backend for this crate's unit tests (ADR 0102),
/// held for the rest of the running test. The `testing` feature itself never
/// links SQLite; only the crate's own test build does, through its
/// dev-dependency.
#[cfg(test)]
pub(crate) async fn sqlite_memory_backend() -> lash_sqlite_store::SqliteBackend {
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    backend
}

/// [`sqlite_memory_backend`] as the handle a host config takes.
#[cfg(test)]
pub(crate) async fn memory_backend() -> crate::Backend {
    std::sync::Arc::new(sqlite_memory_backend().await).into()
}

/// A fresh Restate server double under `seed` with `config`: lash-restate's
/// engine over a SQLite memory store set, the twin of [`memory_backend`] for a
/// kernel test whose effects run on an engine. Hold the double to the end of
/// the test and never build a core over the handle itself (FIG-3723); a turn
/// runs on `double.open_handler(scope)`'s scoped controller. Under
/// `Scheduling::Serial`, a scripted provider that waits on the test holds
/// `double.server().outside_gates().enter()` across the wait.
#[cfg(test)]
pub(crate) async fn kernel_double(
    seed: u64,
    config: lash_restate_test::ServerConfig,
) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, config)
        .await
        .expect("build the Restate server double")
}

#[cfg(test)]
std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`memory_backend`] for a test that reaches
/// only store ports.
#[cfg(test)]
pub(crate) async fn memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    TEST_STORE_SETS.with(|held| held.borrow_mut().push(std::sync::Arc::clone(&stores)));
    stores
}

/// [`memory_store_set`] as a backend whose effect host is the recording
/// double: for a test that needs a `Backend` value but runs no effect.
#[cfg(test)]
pub(crate) async fn memory_store_backend() -> crate::Backend {
    lash_conformance::recording_backend_over(memory_store_set().await)
}

/// A [`runtime_helpers::RecordingStore`] over a fresh, unbound store of a
/// fresh memory backend: the first session admitted binds it.
#[cfg(test)]
pub(crate) async fn unbound_recording_store() -> runtime_helpers::RecordingStore {
    unbound_recording_store_on(&sqlite_memory_backend().await).await
}

/// A [`runtime_helpers::RecordingStore`] over a fresh, unbound store of
/// `backend`'s catalog.
#[cfg(test)]
pub(crate) async fn unbound_recording_store_on(
    backend: &lash_sqlite_store::SqliteBackend,
) -> runtime_helpers::RecordingStore {
    runtime_helpers::RecordingStore::over(std::sync::Arc::new(
        backend.open_store().await.expect("open an unbound store"),
    ))
}

/// The twin of [`unbound_recording_store`] on the Restate server double: a
/// [`runtime_helpers::RecordingStore`] over a fresh, unbound store of the
/// double's engine store set, storage only. The open reads through
/// [`RestateTestBackend::engine_stores`] — the decorated set — so a
/// `backend_with` layer on its session-store factory applies here too.
#[cfg(test)]
pub(crate) async fn double_unbound_recording_store(
    double: &lash_restate_test::RestateTestBackend,
) -> runtime_helpers::RecordingStore {
    runtime_helpers::RecordingStore::over(
        crate::SessionStoreFactory::open_unbound_store(
            crate::StoreSet::session_store_factory(double.engine_stores().as_ref()).as_ref(),
        )
        .await
        .expect("open an unbound store on the double's engine store set"),
    )
}

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
