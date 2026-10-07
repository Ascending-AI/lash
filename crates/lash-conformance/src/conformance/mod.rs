//! Backend-agnostic conformance suites for durable-backend traits.
//!
//! Each suite is parameterized over a factory that produces a *fresh* backend
//! instance and asserts the trait's contract invariants. Run the same suite
//! against every implementation so the contract has one executable source of
//! truth.
//!
//! Reopen and recovery laws use distinct outer handles over one substrate.
//! The runtime-persistence recovery laws certify store behavior only across
//! admission, checkpoint, commit, and settlement boundaries.
//!
//! Each generated test constructs its own backend fixture and reports the violated law
//! independently.
//! Other entry points can be called from backend-specific `#[tokio::test]` functions.

mod attachment_adoption;
pub use attachment_adoption::{
    AttachmentBytesFactory, abandoned_attachment_write_recovery_after_cold_reopen,
    attachment_condemnation_enumeration_conformance, cross_session_attachment_adoption_conformance,
};
mod attachment_condemnation_recovery;
pub use attachment_condemnation_recovery::{
    cold_reopen_adopts_old_generation_before_new_deletes, concurrent_adoption_deletes_once,
    persistently_failing_delete_stalls_typed,
};

#[cfg(feature = "lashlang")]
mod artifact_referrers;
mod artifact_store;
mod attachment_referrers;
pub use attachment_referrers::*;
mod attachment_read_budgets;
mod attachment_store;
pub mod material_retention;
pub use attachment_read_budgets::*;
mod bound_trigger_duplicate;
mod law_backend;
pub use law_backend::backend_over;
pub(crate) use law_backend::{
    LawBackend, StoreLawBackend, law_session_store, law_session_store_with_config,
};
mod admission_support;
mod admitted_head_redrive;
mod commit_receipts;
mod declared_start;
mod definitions;
mod deployment_view;
mod direct_turn_acceptance;
use deployment_view::DeploymentViewExt;
mod fence_integrity;
mod fleet_format;
mod helpers;
mod hostile_input;
mod lineage;
mod live_replay;
mod migrated_tools_redrive;
mod obligation_relay;
mod observer_intent;
mod plugin_state;
mod process_change_feed;
mod process_change_horizon;
mod process_event_append_arms;
mod process_event_batch;
mod process_filters;
mod process_prune_reclaim;
mod process_prune_start_staging;
mod process_references;
mod process_registry;
mod process_trigger_retention;
mod queue_observation;
pub mod registration_macro_support;
mod release_stamp;
mod retention;
mod run_shape;
mod runtime_persistence;
mod segment_budget;
mod segment_redrive;
mod trace_provenance;
pub use segment_budget::{
    SegmentBudgetHarness, SegmentBudgetObservation,
    segment_budget_and_continuation_preserve_results_across_waits,
};
mod batch_sugar;
mod revision_pins;
mod served_process_start;
mod session_delete_blob_reclaim;
mod session_graph_append;
mod session_graph_state_machine;
mod session_history;
mod session_ingress;
mod session_mail;
pub use session_mail::{
    WakeCut, a_producer_commits_its_row_and_its_wake_together,
    a_producer_wakes_no_absent_or_deleted_session, two_claimers_racing_admission_admit_one_run,
};
mod session_store_factory;
mod session_store_factory_enumeration;
mod session_store_factory_failure_evidence;
mod session_store_factory_vacuum;
mod store_contract_state_machine;
mod store_maintenance_outcome;
mod store_recovery;
mod support_prelude;
mod tool_access_persistence;
mod tool_batch_parallelism;
mod tool_call_identity;
mod tool_intent_retention;
mod tool_intent_runtime;
mod trigger_store;
mod turn_config;

mod turn_runner;

pub(crate) use admission_support::*;
pub use admitted_head_redrive::*;
#[cfg(feature = "lashlang")]
pub use artifact_referrers::*;
pub use artifact_store::*;

pub use attachment_store::*;
pub use batch_sugar::*;
pub use declared_start::{
    DeclaredStartTier, SubagentPlugin, a_session_lifetime_subagent_survives_its_waiting_turn,
};
pub use definitions::*;
pub use direct_turn_acceptance::*;
pub use fence_integrity::*;
pub use fleet_format::{FleetFormatDeployment, fleet_format_conformance};
pub use helpers::*;
pub use lineage::*;
pub use live_replay::*;
pub use migrated_tools_redrive::*;
pub use obligation_relay::*;
pub use observer_intent::*;
pub use plugin_state::plugin_state_boundary_trace;
pub use process_change_horizon::*;
pub use process_prune_reclaim::*;
pub use process_prune_start_staging::*;
pub use process_registry::*;
pub use process_trigger_retention::*;
pub use release_stamp::{ReleaseStampDeployment, release_stamp_conformance};
pub use retention::*;
pub use revision_pins::*;
pub use runtime_persistence::*;
pub use served_process_start::SubagentFactories;
pub use session_delete_blob_reclaim::*;
pub use session_graph_append::*;
pub use session_graph_state_machine::*;
pub use session_history::*;
pub use session_ingress::{
    IngressAdmissionProbe, IngressAdmissionSnapshot, SESSION_INGRESS_SESSION_ID,
    SessionIngressHandles, session_ingress_session_request,
};
pub use session_store_factory::*;
pub use session_store_factory_failure_evidence::*;
pub use store_contract_state_machine::*;
pub use store_maintenance_outcome::*;
pub use store_recovery::*;
pub(crate) use support_prelude::*;
pub use tool_access_persistence::*;
pub use tool_batch_parallelism::*;
pub use tool_call_identity::ToolCallIdentityTier;
pub use tool_intent_retention::*;
pub use tool_intent_runtime::*;
pub use trigger_store::*;

pub use turn_runner::*;
