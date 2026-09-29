//! Phase A's legs (ADR 0115 §6): the release gate that proves, before the
//! cut, that 1.0 can serve as an N-1.
//!
//! Each leg runs the two `lash-upgrade-node` builds as separate processes
//! against real PostgreSQL, a real multi-node Restate and SQLite reopen
//! cases, through `lash_upgrade_harness::harness`. Each is HARD on the lane
//! that builds what it proves (ADR 0115 §9, lane L8), so each is listed here,
//! ignored with the lane it waits for, rather than missing: a run that asks
//! for the ignored tests fails every leg that is not built yet.
//!
//! A built leg lives in its own module and is ignored only because it needs
//! both node builds, PostgreSQL and a live `restate-server`, as the rolling
//! smoke is: `just phase-a` builds them and runs every built leg. Every leg
//! is built.
//!
//! The ninth row of §6, `operator_json_contract`, is a single-binary test in
//! `crates/lashctl/tests/` (lane L6).

mod expanded_store_rollback;
mod finalize_races_every_writer;
mod generation_handoff_rollback;
mod history_after_finalize;
mod negotiated_wire_both_directions;
mod object_sweep_crash_resume;
mod retention_delivery_rollback;
mod skipped_compatibility_release_refused;
mod support;
