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
//! smoke is: `just phase-a` builds them and runs every built leg.
//!
//! The ninth row of §6, `operator_json_contract`, is a single-binary test in
//! `crates/lashctl/tests/` (lane L6).

use anyhow::{Result, bail};

mod expanded_store_rollback;
mod generation_handoff_rollback;
mod history_after_finalize;
mod negotiated_wire_both_directions;
mod object_sweep_crash_resume;
mod retention_delivery_rollback;
mod skipped_compatibility_release_refused;
mod support;

/// A leg whose lane has not landed: it fails, never passes empty.
fn waits_for(lane: &str) -> Result<()> {
    bail!("this Phase A leg waits for {lane}")
}

/// For every mutation class of §2.2, N pauses a transaction after its fence
/// (the `AfterFence` fault seam). N+1's finalize waits, and the paused
/// writer commits. A writer that begins after finalize fails
/// `WriterFenced` with zero rows written. A pre-encoded commit that
/// straddles finalize is encoded again under N+1's `F`. Runs on PostgreSQL
/// and on SQLite (each database).
///
/// Waits for lane L2 (FIG-4083): the PostgreSQL writer fence. Lane L3's
/// SQLite half is built: the fence in `SqliteConnection::write`, the
/// `AfterFence` seam of `lash_sqlite_store::testing::SqliteFaultPoint`, and
/// `lash_sqlite_store::testing::finalize_fleet_format` as N+1's finalize.
#[test]
#[ignore = "waits for lane L2 (FIG-4083): the PostgreSQL writer fence"]
fn finalize_races_every_writer() -> Result<()> {
    waits_for("lane L2 (FIG-4083)")
}
