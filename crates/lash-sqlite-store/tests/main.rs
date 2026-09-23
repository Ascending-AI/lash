#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

#[path = "graph_sequence_cutover.rs"]
mod graph_sequence_cutover;
#[path = "layered_effect_host.rs"]
mod layered_effect_host;
#[path = "parent_end_payload.rs"]
mod parent_end_payload;
#[path = "parent_end_registration_race.rs"]
mod parent_end_registration_race;
#[path = "process_definitions_registry.rs"]
mod process_definitions_registry;
#[path = "process_event_time_cutover.rs"]
mod process_event_time_cutover;
#[path = "release_stamp.rs"]
mod release_stamp;
#[path = "required_constraints.rs"]
mod required_constraints;
#[path = "session_read_view.rs"]
mod session_read_view;
#[path = "storage_fixes.rs"]
mod storage_fixes;
#[path = "store_gc.rs"]
mod store_gc;
#[path = "tool_attempt_store_fault.rs"]
mod tool_attempt_store_fault;
#[path = "turn_control_binding.rs"]
mod turn_control_binding;

mod attachment_owner_proof;
mod boundary_retry;
mod deployment_binding_compat;
