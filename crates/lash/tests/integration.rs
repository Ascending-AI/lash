//! Integration contracts compiled together for each runtime feature lane.

// The store witnesses refer to this module through `crate::facade_support`.
#[cfg(feature = "testing")]
mod facade_support {
    pub use lash_core::facade_support::*;
}

#[path = "integration/embed_plugins.rs"]
mod embed_plugins;
#[path = "integration/facade_inventory.rs"]
mod facade_inventory;
#[path = "integration/integrator_facade.rs"]
mod integrator_facade;
#[path = "integration/one_home.rs"]
mod one_home;
#[path = "integration/process_scope_fence.rs"]
mod process_scope_fence;
#[path = "integration/runtime_operation_retirement.rs"]
mod runtime_operation_retirement;
#[path = "integration/stores_evidence.rs"]
mod stores_evidence;
#[path = "integration/support.rs"]
mod support;
#[path = "integration/tool_intent_ingress_observability.rs"]
mod tool_intent_ingress_observability;
