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
//! The ninth row of §6, `operator_json_contract`, is a single-binary test in
//! `crates/lashctl/tests/` (lane L6).

use anyhow::{Result, bail};

/// A leg whose lane has not landed: it fails, never passes empty.
fn waits_for(lane: &str) -> Result<()> {
    bail!("this Phase A leg waits for {lane}")
}

/// N+1's migrate expands PostgreSQL and every SQLite database. N restarts,
/// opens `Expanded`, writes, and N+1 reads N's rows. Raising `min_reader`
/// makes N refuse `ReaderFloorAbove` on both backends. Each unsafe addition
/// of §1.4 makes N refuse `ShapeRefused`. A populated store with its stamp
/// deleted refuses `Unstamped`.
///
/// Waits for lane L1 (FIG-4043): the stamps with `min_reader`, admission and
/// the tolerant shape check, plus the `synthetic-next` expand step in
/// `crates/lash-postgres-store/src/postgres/migrate.rs` and the SQLite
/// component bumps in `crates/lash-sqlite-store/src/schema.rs`.
#[test]
#[ignore = "waits for lane L1 (FIG-4043): store stamps, admission and the tolerant shape check"]
fn expanded_store_rollback() -> Result<()> {
    waits_for("lane L1 (FIG-4043)")
}

/// A build whose writable range starts above the recorded `F` refuses at
/// open with `FleetOutsideWritable`, before it takes traffic. So does one
/// whose component range starts above the stamp.
///
/// Waits for lane L1 (FIG-4043): the opens move onto `FleetFormat::admit`
/// and the component descriptor.
#[test]
#[ignore = "waits for lane L1 (FIG-4043): opens admit F and the component stamp"]
fn skipped_compatibility_release_refused() -> Result<()> {
    waits_for("lane L1 (FIG-4043)")
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

/// Remote protocol: N+1 to N and N to N+1 select 1, including requests,
/// replies, errors and streams. A synthetic `[2,2]` peer against `[1,1]`
/// gets `Unsupported`, with zero effects. Restate: an N caller reaches
/// N+1's handlers and gets version-1 replies. After a rollback an N+1
/// caller reaches N's handlers. A disjoint call changes nothing.
///
/// Waits for lane L4 (the Restate call envelope) and lane L5 (FIG-3804,
/// remote negotiation), plus the `synthetic-next` ranges in
/// `crates/lash-remote-protocol/src/negotiation.rs` and
/// `crates/lash-restate/src/compat.rs`.
#[test]
#[ignore = "waits for lanes L4 and L5 (FIG-3804): the Restate call envelope and remote negotiation"]
fn negotiated_wire_both_directions() -> Result<()> {
    waits_for("lanes L4 and L5 (FIG-3804)")
}

/// Objects and `LashTurn` outcomes written by N and by N+1 before finalize
/// are all in N's format, and N reads them all. After finalize the
/// synthetic sweep converts objects and survives a crash mid-sweep.
/// Preflight lists the objects still at format 1. A kept N handler is
/// refused typed by `_compat`.
///
/// Waits for lane L4: `_compat`, the selected encoder and the versioned
/// `RootOutcome`, plus the `synthetic-next` effect-group state format in
/// `crates/lash-restate/src/effect_group/protocol.rs`.
#[test]
#[ignore = "waits for lane L4: _compat, the selected encoder and the versioned RootOutcome"]
fn object_sweep_crash_resume() -> Result<()> {
    waits_for("lane L4")
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
