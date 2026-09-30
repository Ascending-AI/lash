//! Backend-agnostic conformance suites for durable-backend traits.
//!
//! Each suite is parameterized over a factory that produces a *fresh* backend
//! instance and asserts the trait's contract invariants. Run the same suite
//! against every implementation so the contract has one executable source of
//! truth.
//!
//! Reopen and recovery laws use distinct outer handles over one substrate.
//! The runtime-persistence recovery laws certify store behavior only across
//! admission, checkpoint, commit, and settlement boundaries. The distinct
//! [`turn_crash_matrix_level_1`] suite executes a real scripted turn through
//! conformance-owned store, provider, and effect-controller decorators; its
//! golden trace generates the crash points and its outcome table supplies the
//! recovery oracle. Backend helper processes run the selected level-2 points
//! under `SIGKILL`, including provider streaming, external-effect outcome loss,
//! and final turn control.
//!
//! Each generated test constructs its own backend fixture and reports the violated law
//! independently.
//! Other entry points can be called from backend-specific `#[tokio::test]` functions.

pub use lash_core::testing::coordinate_tool_provider_with_services;

mod attachment_adoption;
pub use attachment_adoption::{
    AttachmentBytesFactory, abandoned_attachment_write_recovery_after_cold_reopen,
    attachment_condemnation_enumeration_conformance,
    attachment_owner_identity_round_trips_conformance, cross_owner_attachment_adoption_conformance,
};
mod attachment_condemnation_recovery;
pub use attachment_condemnation_recovery::{
    cold_reopen_adopts_old_generation_before_new_deletes, concurrent_adoption_deletes_once,
    persistently_failing_delete_stalls_typed,
};

#[cfg(feature = "lashlang")]
mod artifact_referrers;
mod artifact_store;
mod attachment_owner;
mod attachment_store;
mod await_event_cold;
mod law_backend;
pub(crate) use law_backend::{LawBackend, StoreLawBackend, law_session_store};
pub use law_backend::{backend_over, recording_backend_over};
mod admission_support;
mod admitted_head_redrive;
mod cancelled_turn_withheld_input;
mod cell_binding_drift;
mod completion_routing;
mod declared_start;
mod deployment_view;
mod direct_turn_acceptance;
use deployment_view::DeploymentViewExt;
mod drive_admission;
mod effect_group_host;
mod effect_host;
mod fence_integrity;
mod fleet_format;
mod frame_open_redrive;
mod frame_switch_redrive;
mod generation_drain;
mod helpers;
mod hostile_input;
mod lineage;
mod live_replay;
mod migrated_tools_redrive;
mod model_call_drift_park;
mod obligation_relay;
mod observer_intent;
mod plugin_state;
mod presentation_divergence_park;
mod process_change_feed;
mod process_change_horizon;
mod process_continuation_store;
mod process_event_append_arms;
mod process_event_batch;
mod process_filters;
mod process_park_feed;
mod process_prune_reclaim;
mod process_prune_start_staging;
mod process_references;
mod process_registry;
mod process_trigger_retention;
mod queued_after_commit_redrive;
pub mod registration_macro_support;
mod release_stamp;
mod retention;
mod root_answers_its_rows;
mod root_control;
mod root_start_marker;
mod root_supersession;
mod root_terminal;
mod run_shape;
mod run_spec_drive;
mod runtime_persistence;
mod runtime_persistence_state_machine;
mod segment_budget;
mod segment_redrive;
pub use segment_budget::{
    SegmentBudgetHarness, SegmentBudgetObservation,
    segment_budget_and_continuation_preserve_results_across_waits,
};
mod served_process_start;
mod session_close;
mod session_delete;
mod session_delete_blob_reclaim;
mod session_graph_append;
mod session_graph_state_machine;
mod session_history;
mod session_ingress;
mod session_store_factory;
mod session_store_factory_enumeration;
mod session_store_factory_failure_evidence;
mod turn_commit_outcome;
pub use turn_commit_outcome::{
    cancelled as turn_commit_cancelled, completed as turn_commit_completed,
    failed as turn_commit_failed, frame_switch as turn_commit_frame_switch,
};
mod batch_sugar;
mod session_store_factory_vacuum;
mod store_contract_state_machine;
mod store_maintenance_outcome;
mod store_recovery;
mod support_prelude;
mod tool_access_persistence;
mod tool_batch_crash_redrive;
mod tool_batch_parallelism;
mod tool_batch_scaling;
mod tool_call_identity;
mod tool_child_drift;
mod tool_child_invocation;
mod tool_child_turn_cancel;
mod tool_intent_runtime;
mod trigger_store;
mod turn_config;
mod turn_control;
mod turn_crash_matrix;
mod turn_park_feed;
mod turn_runner;
mod wake_delivery;

pub(crate) use admission_support::*;
pub use admitted_head_redrive::*;
#[cfg(feature = "lashlang")]
pub use artifact_referrers::*;
pub use artifact_store::*;
pub use attachment_owner::*;
pub use attachment_store::*;
pub use await_event_cold::*;
pub use batch_sugar::*;
pub use cancelled_turn_withheld_input::*;
pub use cell_binding_drift::*;
pub use completion_routing::*;
pub use declared_start::{DeclaredStartTier, SubagentPlugin};
pub use direct_turn_acceptance::*;
pub use drive_admission::*;
pub use effect_group_host::*;
pub use effect_host::*;
pub use fence_integrity::*;
pub use fleet_format::{FleetFormatDeployment, fleet_format_conformance};
pub use frame_open_redrive::*;
pub use frame_switch_redrive::*;
pub use generation_drain::*;
pub use helpers::*;
pub use lineage::*;
pub use live_replay::*;
pub use migrated_tools_redrive::*;
pub use model_call_drift_park::*;
pub use obligation_relay::*;
pub use observer_intent::*;
pub use plugin_state::plugin_state_boundary_trace;
pub use presentation_divergence_park::*;
pub use process_change_horizon::*;
pub use process_continuation_store::*;
pub use process_park_feed::*;
pub use process_prune_reclaim::*;
pub use process_prune_start_staging::*;
pub use process_registry::*;
pub use process_trigger_retention::*;
pub use queued_after_commit_redrive::*;
pub use release_stamp::{ReleaseStampDeployment, release_stamp_conformance};
pub use retention::*;
pub use root_start_marker::*;
pub use runtime_persistence::*;
pub use runtime_persistence_state_machine::*;
pub use served_process_start::SubagentFactories;
pub use session_delete_blob_reclaim::*;
pub use session_graph_append::*;
pub use session_graph_state_machine::*;
pub use session_history::*;
pub use session_ingress::{
    SESSION_INGRESS_SESSION_ID, SessionIngressHandles, session_ingress_session_request,
};
pub use session_store_factory::*;
pub use session_store_factory_failure_evidence::*;
pub use store_contract_state_machine::*;
pub use store_maintenance_outcome::*;
pub use store_recovery::*;
pub(crate) use support_prelude::*;
pub use tool_access_persistence::*;
pub use tool_batch_crash_redrive::*;
pub use tool_batch_parallelism::*;
pub use tool_batch_scaling::*;
pub use tool_call_identity::ToolCallIdentityTier;
pub use tool_child_drift::*;
pub use tool_child_invocation::*;
pub use tool_child_turn_cancel::*;
pub use tool_intent_runtime::*;
pub use trigger_store::*;
pub use turn_control::*;
pub use turn_crash_matrix::*;
pub use turn_park_feed::*;
pub use turn_runner::*;
pub use wake_delivery::*;

pub mod usage_ledger;
pub use usage_ledger::{UsageLedgerSnapshot, UsageLedgerStoreFixture};
