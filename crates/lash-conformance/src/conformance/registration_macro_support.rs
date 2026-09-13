//! Implementation paths used only by exported conformance registration macros.

pub use super::artifact_store::*;
pub use super::attachment_adoption::*;
pub use super::attachment_owner::*;
pub use super::attachment_store::*;
pub use super::await_event_cold::*;
pub use super::direct_turn_acceptance::*;
pub use super::durable_queued_drain_wait::*;
pub use super::effect_group_drain::*;
pub use super::effect_group_host::*;
pub use super::effect_host::*;
pub use super::fence_integrity::*;
pub use super::graph_integrity::*;
pub use super::hostile_input::*;
pub use super::lineage::*;
pub use super::live_replay::*;
pub use super::observer_intent::*;
pub use super::process_change_feed::*;
pub use super::process_change_horizon::*;
pub use super::process_continuation_store::*;
pub use super::process_event_append_arms::*;
pub use super::process_filters::*;
pub use super::process_prune_reclaim::*;
pub use super::process_references::*;
pub use super::process_registry::status_filters::*;
pub use super::process_registry::*;
pub use super::process_trigger_retention::*;
pub use super::retention::*;
pub use super::runtime_persistence::*;
pub use super::runtime_persistence_state_machine::*;
pub use super::session_delete_blob_reclaim::*;
pub use super::session_execution_lease_renewal::*;
pub use super::session_graph_append::*;
pub use super::session_graph_state_machine::*;
pub use super::session_store_factory::*;
pub use super::session_store_factory_failure_evidence::*;
pub use super::store_contract_state_machine::*;
pub use super::store_maintenance_outcome::*;
pub use super::store_recovery::*;
pub use super::tool_access_persistence::*;
pub use super::tool_intent_runtime::*;
pub use super::trigger_store::*;
pub use super::turn_control::*;
pub use super::turn_crash_matrix::*;
pub use super::wake_delivery::*;
pub use lash_core::ProcessRegistry;

pub fn effect_group_suite_executors() -> std::sync::Arc<dyn crate::GroupExecutors> {
    super::effect_group_host::suite_executors()
}

pub fn effect_group_test_prefix(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::new_v4().simple())
}
