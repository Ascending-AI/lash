//! Backend-agnostic conformance suites for durable-backend traits.
//!
//! Each suite is parameterized over a factory that produces a *fresh* backend
//! instance and asserts the trait's contract invariants. Run the same suite
//! against every implementation (the production backend and any in-memory test
//! double) so the contract has one executable source of truth and the doubles
//! can't drift from production behavior.
//!
//! Reopen and recovery laws use distinct outer handles over one substrate.
//! The runtime-persistence recovery laws certify store behavior only across
//! claim, checkpoint, commit, and settlement boundaries. The distinct
//! [`turn_crash_matrix_level_1`] suite executes a real scripted turn through
//! conformance-owned store, provider, and effect-controller decorators; its
//! golden trace generates the crash points and its outcome table supplies the
//! recovery oracle. Backend helper processes run the selected level-2 points
//! under `SIGKILL`, including provider streaming, external-effect outcome loss,
//! and final turn control.
//!
//! Register runtime persistence vectors with `runtime_persistence_tests!` or
//! `runtime_persistence_reopenable_tests!`. Each generated test constructs its
//! own backend fixture and reports the violated law independently. Other
//! entry points can be called from backend-specific `#[tokio::test]` functions.

pub use lash_core::testing::coordinate_tool_provider_with_services;

mod attachment_adoption;
pub use attachment_adoption::{
    abandoned_attachment_write_recovery_after_cold_reopen,
    attachment_condemnation_delete_crash_survives_cold_reopen,
    attachment_condemnation_enumeration_conformance, cross_owner_attachment_adoption_conformance,
};

mod artifact_store;
mod attachment_owner;
mod attachment_store;
mod await_event_cold;
pub mod cold_process_turn_parent;
mod direct_turn_acceptance;
mod durable_queued_drain_wait;
mod effect_group_drain;
mod effect_group_host;
mod effect_host;
mod fence_integrity;
mod graph_integrity;
mod helpers;
mod hostile_input;
mod lineage;
mod live_replay;
mod observer_intent;
mod plugin_state;
mod process_change_feed;
mod process_change_horizon;
mod process_continuation_store;
mod process_event_append_arms;
mod process_filters;
mod process_prune_reclaim;
mod process_references;
mod process_registry;
mod process_trigger_retention;
#[doc(hidden)]
pub mod registration_macro_support;
mod retention;
mod runtime_persistence;
mod runtime_persistence_state_machine;
mod session_delete_blob_reclaim;
mod session_execution_lease_renewal;
mod session_graph_append;
mod session_graph_state_machine;
mod session_store_factory;
mod session_store_factory_enumeration;
mod session_store_factory_failure_evidence;
mod session_store_factory_vacuum;
mod store_contract_state_machine;
mod store_maintenance_outcome;
mod store_recovery;
mod support_prelude;
mod tool_access_persistence;
mod tool_intent_runtime;
mod trigger_store;
mod turn_control;
mod turn_crash_matrix;
mod wake_delivery;

pub use artifact_store::*;
pub use attachment_owner::*;
pub use attachment_store::*;
pub use await_event_cold::*;
pub use direct_turn_acceptance::*;
pub use durable_queued_drain_wait::*;
pub use effect_group_drain::*;
pub use effect_group_host::*;
pub use effect_host::*;
pub use fence_integrity::*;
pub use graph_integrity::*;
pub use helpers::*;
pub use lineage::*;
pub use live_replay::*;
pub use observer_intent::*;
pub use plugin_state::plugin_state_boundary_trace;
pub use process_change_horizon::*;
pub use process_continuation_store::*;
pub use process_prune_reclaim::*;
pub use process_registry::*;
pub use process_trigger_retention::*;
pub use retention::*;
pub use runtime_persistence::*;
pub use runtime_persistence_state_machine::*;
pub use session_delete_blob_reclaim::*;
pub use session_execution_lease_renewal::*;
pub use session_graph_append::*;
pub use session_graph_state_machine::*;
pub use session_store_factory::*;
pub use session_store_factory_failure_evidence::*;
pub use store_contract_state_machine::*;
pub use store_maintenance_outcome::*;
pub use store_recovery::*;
pub(crate) use support_prelude::*;
pub use tool_access_persistence::*;
pub use tool_intent_runtime::*;
pub use trigger_store::*;
pub use turn_control::*;
pub use turn_crash_matrix::*;
pub use wake_delivery::*;
