//! Phase A's legs (ADR 0115 §6): the release gate that proves, before the
//! cut, that 1.0 can serve as an N-1.
//!
//! Each leg runs the two `lash-upgrade-node` builds as separate processes
//! through `lash_upgrade_harness::harness`. The live-service legs need
//! PostgreSQL and `restate-server`; SQLite legs cover reopen and migration.
//! `just phase-a` builds both nodes and runs the service-dependent tests,
//! which are ignored by ordinary test runs.
//!
//! `operator_json_contract` is a single-binary test in `crates/lashctl/tests/`
//! (ADR 0115 §6).

mod expanded_store_rollback;
mod finalize_races_every_writer;
mod generation_handoff_rollback;
mod history_after_finalize;
mod negotiated_wire_both_directions;
mod object_sweep_crash_resume;
mod retention_delivery_rollback;
mod skipped_compatibility_release_refused;
mod support;

mod workflow_graph_range;
