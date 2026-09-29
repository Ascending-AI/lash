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
mod negotiated_wire_both_directions;
mod object_sweep_crash_resume;
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
/// Waits for lanes L2 and L3 (FIG-3800 part A): the PostgreSQL and SQLite
/// writer fences.
#[test]
#[ignore = "waits for lanes L2 and L3 (FIG-3800 A): the PostgreSQL and SQLite writer fences"]
fn finalize_races_every_writer() -> Result<()> {
    waits_for("lanes L2 and L3 (FIG-3800 A)")
}

/// A foreign-`G` journal dispatches zero effects and parks. Signals that
/// race a hand-off are delivered exactly once. A root admitted by a drive
/// pinned to N runs on N+1, with no refusal and no `SubstrateLost`. A
/// continuation N cannot decode keeps N+1's deployment and routes there.
/// Rollback registers N at a fresh URI. Registering at a URI that serves
/// another generation is refused.
///
/// Waits for lane L4: the `drive_version` gate's removal and the
/// registration guard, plus the `synthetic-next` continuation format in
/// `crates/lashlang/src/runtime/vm/continuation.rs`.
#[test]
#[ignore = "waits for lane L4: the drive_version gate's removal and the registration guard"]
fn generation_handoff_rollback() -> Result<()> {
    waits_for("lane L4")
}

/// Checkpoints, attachments, referrer edges and fences, and obligations
/// written by N+1 before finalize survive N's rollback, N's retention and
/// GC, and a return to N+1: nothing is lost or delivered twice. A row with
/// an unknown obligation kind stays outstanding and typed under N.
///
/// Waits for lanes L7a (surfaces now) and L7b (surfaces after ADR 0113).
#[test]
#[ignore = "waits for lanes L7a and L7b: the per-surface obligations of §5"]
fn retention_delivery_rollback() -> Result<()> {
    waits_for("lanes L7a and L7b")
}

/// After finalize, N+1 still reads the history N wrote, through the
/// permanent floor and not through `{F, newest}`.
///
/// Waits for lane L1 (FIG-4043), plus the `synthetic-next` session node
/// body version and its history upcaster in
/// `crates/lash-core-store/src/session_graph.rs`.
#[test]
#[ignore = "waits for lane L1 (FIG-4043): the history floor behind admitted stores"]
fn history_after_finalize() -> Result<()> {
    waits_for("lane L1 (FIG-4043)")
}
