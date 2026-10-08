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
mod attachment_provider_files;
mod attachment_read_budgets;
mod attachment_store;
pub mod material_retention;
pub use attachment_provider_files::*;
pub use attachment_read_budgets::*;
mod law_backend;
pub use law_backend::backend_over;
pub(crate) use law_backend::{StoreLawBackend, law_session_store};
mod admission_support;
mod definitions;
mod deployment_view;
use deployment_view::DeploymentViewExt;
mod fence_integrity;
mod fleet_format;
mod helpers;
mod hostile_input;
mod lineage;
mod live_replay;
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
mod queue_observation;
pub mod registration_macro_support;
mod release_stamp;
mod retention;
mod revision_pins;
mod run_shape;
mod runtime_persistence;
mod session_delete_blob_reclaim;
mod session_graph_append;
mod session_graph_state_machine;
mod session_history;
mod session_ingress;
mod session_mail;
mod trace_provenance;
pub use session_mail::{
    WakeCut, a_producer_commits_its_row_and_its_wake_together,
    a_producer_wakes_no_absent_or_deleted_session, two_claimers_racing_admission_admit_one_run,
};
mod session_store_factory;
mod session_store_factory_enumeration;
mod session_store_factory_vacuum;
mod store_contract_state_machine;
mod store_maintenance_outcome;
mod store_recovery;
mod support_prelude;
mod tool_access_persistence;
mod tool_intent_retention;

#[cfg(feature = "lashlang")]
pub use artifact_referrers::*;
pub use artifact_store::*;

pub use attachment_store::*;
pub use definitions::*;
pub use fence_integrity::*;
pub use fleet_format::{FleetFormatDeployment, fleet_format_conformance};
pub use helpers::*;
pub use lineage::*;
pub use live_replay::*;
pub use obligation_relay::*;
pub use observer_intent::*;
pub use plugin_state::{
    ingress_plugin_callbacks_publish_state_that_survives_a_checkpoint, plugin_state_boundary_trace,
};
pub use process_change_horizon::*;
pub use process_prune_reclaim::*;
pub use process_prune_start_staging::*;
pub use process_registry::*;
pub use release_stamp::{ReleaseStampDeployment, release_stamp_conformance};
pub use retention::*;
pub use revision_pins::*;
pub use runtime_persistence::*;
pub use session_delete_blob_reclaim::*;
pub use session_graph_append::*;
pub use session_graph_state_machine::*;
pub use session_history::*;
pub use session_ingress::{
    IngressAdmissionProbe, IngressAdmissionSnapshot, SESSION_INGRESS_SESSION_ID,
    SessionIngressHandles, session_ingress_session_request,
};
pub use session_store_factory::*;
pub use store_contract_state_machine::*;
pub use store_maintenance_outcome::*;
pub use store_recovery::*;
pub(crate) use support_prelude::*;
pub use tool_access_persistence::*;
pub use tool_intent_retention::*;

mod attachment_delivery;
pub use attachment_delivery::*;
