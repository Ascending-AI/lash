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
/// Opt in to implementing runtime service substitutes in tests.
pub use crate::core_internal::RuntimeService;
#[cfg(test)]
use lash_execution_contracts::testing::process_execution_env_fixture;

pub use lash_core_execution::testing::DurableCommitHook;
pub use lash_core_execution::testing::EmptyToolProvider;
pub use lash_core_execution::testing::FIXTURE_ECHO_TOOL;
pub use lash_core_execution::testing::FixtureProcessEngine;
pub use lash_core_execution::testing::FixtureTools;
pub use lash_core_execution::testing::extension_trace::{
    BoundExtensionTrace, bound_extension_trace, bound_tool_trace,
};
pub use lash_core_execution::testing::kernel_internals::prompt_catalog_from_recipes;
pub use lash_core_execution::testing::{
    Call, CallbackCounts, DeploymentOp, FaultKind, GATE_DEADLINE, Gate, Op, Outcome, Phase, Script,
    Scripted, ScriptedError, StoreOp,
};

pub use lash_core_execution::testing::HeldProcessEngine;
pub use lash_core_execution::testing::MockSessionManager;
pub use lash_core_execution::testing::NonTerminalPagePause;
pub use lash_core_execution::testing::NonTerminalPageRead;
pub use lash_core_execution::testing::ProcessRegistryFaults;
pub use lash_core_execution::testing::RegistrationHoldPoint;
pub use lash_core_execution::testing::RuntimeCommitBudgetMeasurement;
pub use lash_core_execution::testing::TestClock;
pub use lash_core_execution::testing::TestExecutionContextBuilder;
pub use lash_core_execution::testing::TestExecutionPorts;
pub use lash_core_execution::testing::TestProvider;
pub use lash_core_execution::testing::TestProviderBuilder;
pub use lash_core_execution::testing::ToolCallFixture;
pub use lash_core_execution::testing::UnavailableProcessEnvStore;
pub use lash_core_execution::testing::UnavailableTurnPreludeStore;
pub use lash_core_execution::testing::behavior_transcript;
pub use lash_core_execution::testing::cancelled_code_execution_context;
pub use lash_core_execution::testing::code_execution_context;
pub use lash_core_execution::testing::code_execution_context_with_deferred_provider;
pub use lash_core_execution::testing::code_execution_context_with_invocation;
pub use lash_core_execution::testing::code_execution_context_with_process_dependencies;
pub use lash_core_execution::testing::code_execution_context_with_tool_catalog;
pub use lash_core_execution::testing::code_execution_context_with_tool_provider_and_catalog;
pub use lash_core_execution::testing::code_execution_context_with_tool_provider_catalog_and_invocation;
pub use lash_core_execution::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation;

pub use lash_core_execution::testing::effect_backed_process_service;
pub use lash_core_execution::testing::exec_code_invocation;
pub use lash_core_execution::testing::execute_effect_locally;
pub use lash_core_execution::testing::execute_tool_intents_with_services;
pub use lash_core_execution::testing::execute_tool_intents_with_services_and_hook;
pub use lash_core_execution::testing::fixture_echo_definition;

pub use lash_core_execution::testing::graph_integrity;

pub use lash_core_execution::testing::lineage;
pub use lash_core_execution::testing::llm_profiles_serving;
pub use lash_core_execution::testing::measure_runtime_commit_budget;
pub use lash_core_execution::testing::mock_attempt_context;
pub use lash_core_execution::testing::mock_attempt_context_with_execution_binding;
pub use lash_core_execution::testing::mock_attempt_context_with_host;
pub use lash_core_execution::testing::mock_session_policy;
pub use lash_core_execution::testing::process_engine_fixture;
pub use lash_core_execution::testing::process_engine_plugin_fixture;

pub use lash_core_execution::testing::process_env_of_size;

pub use lash_core_execution::testing::process_roster_records_for_fixture;
pub use lash_core_execution::testing::process_work_wiring_for_registry;
pub use lash_core_execution::testing::publish_process_env_for_testing;
pub use lash_core_execution::testing::queued_work_admission_policy;
pub use lash_core_execution::testing::run_policy_naming;
pub use lash_core_execution::testing::run_tool;
pub use lash_core_execution::testing::run_tool_granted;
pub use lash_core_execution::testing::runtime_lease_owner;
pub use lash_core_execution::testing::runtime_services_without_ports;
pub use lash_core_execution::testing::sansio_transcript;
pub use lash_core_execution::testing::single_llm_profile_registry;
pub use lash_core_execution::testing::stage_execution_graph;
pub use lash_core_execution::testing::staged_start_record;
pub use lash_core_execution::testing::standard_test_llm_profiles;
pub use lash_core_execution::testing::standard_test_policy;
pub use lash_core_execution::testing::store_fixtures;
pub use lash_core_execution::testing::test_code_protocol_factories;
pub use lash_core_execution::testing::test_llm_profile_config;
pub use lash_core_execution::testing::test_llm_profile_metadata;
pub use lash_core_execution::testing::test_plugin_host;
pub use lash_core_execution::testing::test_protocol_factories_ending_without_done;
pub use lash_core_execution::testing::test_standard_protocol_factories;
pub use lash_core_execution::testing::test_standard_protocol_factory_with_runtime_state;
pub use lash_core_execution::testing::test_tools_provider;
pub use lash_core_execution::testing::tool_component;
pub use lash_core_execution::testing::tool_hook_context;
pub use lash_core_execution::testing::tool_plugin;
pub use lash_core_execution::testing::trace_capture;

/// Prompt composition over a cut a test states (ADR 0133).
pub use lash_core_execution::testing::prompt;

// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and a
// reader or editor following the merged doc comment — including the
// submodule's own intra-doc links — would resolve it against *this* module's
// scope, where none of the linked items exist.
pub mod adversarial_text;
pub mod checkpoint_observer;
pub mod conformance_support;
mod layered_backend;
mod live_replay;
pub use live_replay::{
    process_committed_event, process_language_observation, process_observation_label,
    process_step_body_started, session_language_observation,
};
mod poll;
mod recording_store;
pub mod runtime_helpers;
#[cfg(feature = "testing")]
pub mod runtime_internals;
#[cfg(target_os = "linux")]
mod thread_census;

#[cfg(target_os = "linux")]
pub use thread_census::ThreadCensus;

pub use poll::{poll_until, wait_until};

/// The in-memory trace sink laws read typed records from.
pub use lash_trace::RecordingTraceSink;

#[cfg(test)]
std::thread_local! {
    /// The SQLite backends the running unit test opened. A memory backend's
    /// databases live while any handle does, and its stores reach sibling
    /// databases by name, so a fixture that hands out only a store would
    /// otherwise let those vanish under it. Each test runs on its own thread,
    /// so this holds every backend exactly as long as the test that opened it.
    static TEST_BACKENDS: std::cell::RefCell<Vec<lash_sqlite_store::SqliteStoreSet>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set for this crate's unit tests,
/// held for the rest of the running test. The `testing` feature itself never
/// links SQLite; only the crate's own test build does, through its
/// dev-dependency.
#[cfg(test)]
pub(crate) async fn sqlite_memory_backend() -> lash_sqlite_store::SqliteStoreSet {
    let backend = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory backend");
    process_execution_env_fixture(backend.process_env_store().as_ref()).await;
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    backend
}

/// [`sqlite_memory_backend`] behind a recording test effect host.
#[cfg(test)]
pub(crate) async fn sqlite_recording_backend() -> lash_execution_contracts::backend::Backend {
    lash_conformance::backend_over(std::sync::Arc::new(sqlite_memory_backend().await))
}

#[cfg(test)]
std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`sqlite_recording_backend`] for a test that reaches
/// only store ports.
#[cfg(test)]
pub(crate) async fn sqlite_memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    TEST_STORE_SETS.with(|held| held.borrow_mut().push(std::sync::Arc::clone(&stores)));
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    stores
}

/// [`sqlite_memory_store_set`] as a backend whose effect host is the recording
/// double: for a test that needs a `Backend` value but runs no effect.
#[cfg(test)]
pub(crate) async fn sqlite_memory_store_backend() -> lash_execution_contracts::backend::Backend {
    lash_conformance::backend_over(sqlite_memory_store_set().await)
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
    backend: &lash_sqlite_store::SqliteStoreSet,
) -> runtime_helpers::RecordingStore {
    runtime_helpers::RecordingStore::over(
        lash_execution_contracts::backend::StoreSet::session_store_factory(backend),
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

/// Laws exercised by optional plugins over their own durable codecs.
pub use lash_core_store::testing::guarded_surfaces;

pub use lash_core_execution::runtime::actor::round::graph::{
    AdmitMember, CallOutcome, MaterialNode, PinnedMember, RoundFact, SettledFact,
};

mod cell_ledger;
pub use cell_ledger::{cell_ledger_bytes, turn_transcript};
