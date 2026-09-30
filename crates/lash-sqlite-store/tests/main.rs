#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

#[path = "fleet_format.rs"]
mod fleet_format;
#[path = "graph_sequence_cutover.rs"]
mod graph_sequence_cutover;
#[path = "parent_end_payload.rs"]
mod parent_end_payload;
#[path = "parent_end_registration_race.rs"]
mod parent_end_registration_race;
#[path = "process_definitions_registry.rs"]
mod process_definitions_registry;
#[path = "release_stamp.rs"]
mod release_stamp;
#[path = "storage_fixes.rs"]
mod storage_fixes;
#[path = "store_gc.rs"]
mod store_gc;

mod boundary_retry;
