//! The invariants every cell of the matrix checks, whatever its case
//! (design-opus §7.2, ADR 0132 §2):
//!
//! - **F1, fencing.** Once a zombie's or a stale owner's actors moved, no
//!   owner commit it attempts lands until it claims again: each is refused
//!   or never enters the store, so nothing it wrote is visible.
//! - **F2 and NR-2, no `Once` body twice.** No admitted `Once` execution's
//!   body is entered twice, by the tripwire's count or by the body ledger's
//!   (owner, call) entries; a `Once` started without an outcome settles
//!   `Interrupted` and is never entered again.
//! - **NR-1.** An execution whose outcome its body produced (completed or
//!   known failure) ran its body: exactly once for `Once`.
//! - **NR-3.** A `Repeatable` started without an outcome re-runs at its own
//!   ordinal, so it is entered at most once more for the cell's one cut.
//! - **NR-4.** Resume performs no outcome lookup on behalf of re-running
//!   code and emits no committed ordinal again, and a turn restores from
//!   its checkpoint at most once per interruption.
//! - **Admission first (S4).** No body starts before its `x_start` commit.
//! - **Lease first (S4).** No body starts on a node past its lease: under a
//!   stale-epoch cut, a `Once` the moved owner settles `Interrupted` was
//!   never entered after its node paused.
//! - **The fold accepts the persisted records** of every owner that ran a
//!   body.
//! - **Every actor reaches a terminal or a durable wait.** At the end no
//!   actor of the run is `ready` with no claimer, owned by a node that no
//!   longer serves, parked, or `waiting` with mail it has not read.
//! - **Deadlines are honoured** within the case's bound plus a failover and
//!   the stop grace.

use std::collections::BTreeSet;

use lash_core_execution::runtime::actor::round::SettledOutput;
use lash_core_execution::runtime::actor::round::{PinnedWaits, PolicyView, Recovery, fold};
use lash_durable::domain::{AdmittedId, OwnerKey};
use lash_durable::{ActorState, CommitLabel};
use lash_durable_test::{Cut, Fault, SimNodes, Stored, Write, WriteKind};
use lash_sansio::ExecutionPolicy;

use super::world::World;

/// Every invariant of the cell cut at `cut`, which `nodes` ran over
/// `world`; `bound_ms` is the virtual time by which the case must be done.
pub async fn check(
    world: &World,
    nodes: &SimNodes,
    cut: Option<&Cut>,
    bound_ms: u64,
    max_restores: usize,
) -> Vec<String> {
    let trace = nodes.script().trace();
    let mut violations = fencing(cut, &trace);
    violations.extend(once(world, nodes, &trace, stale_pause(cut, &trace)).await);
    violations.extend(no_replay(world, cut, max_restores));
    violations.extend(admission_first(world));
    violations.extend(settled(world, nodes).await);
    let now = nodes.clock().logical_ms();
    if now > bound_ms {
        violations.push(format!(
            "deadline: done at {now} ms of virtual time, past the bound {bound_ms} ms"
        ));
    }
    violations
}

/// F1: from a zombie's cut write, or after a stale owner's, no owner commit
/// the paused node makes lands, until it registers or claims again. A write
/// the node never sent again (it stopped first) left nothing either.
pub fn fencing(cut: Option<&Cut>, trace: &[Write]) -> Vec<String> {
    let Some(cut) = cut else {
        return Vec::new();
    };
    if !matches!(cut.fault, Fault::Zombie | Fault::StaleEpoch) || cut.kind != WriteKind::Actor {
        return Vec::new();
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec![format!(
            "F1: the cut write {} is not in the trace",
            cut.point
        )];
    };
    // The node pauses at the cut and resumes only once its actors moved, a
    // failover later: every write it sends after it resumes is stamped
    // later than the cut. Writes stamped at the cut's instant were already
    // in flight beside it, under a live lease.
    let paused_at = trace[at].at_ms;
    let mut violations = Vec::new();
    for (index, write) in trace.iter().enumerate().skip(at) {
        let zombie_held = index == at && cut.fault == Fault::Zombie;
        if write.node != cut.node || !(zombie_held || write.at_ms > paused_at) {
            continue;
        }
        let claimed_again = write.kind == WriteKind::Lease
            && (write.point.label == CommitLabel::NODE_REGISTER
                || (write.point.label == CommitLabel::CLAIM
                    && write.stored == Stored::Committed { effective: true }));
        if claimed_again {
            break;
        }
        if write.kind != WriteKind::Actor {
            continue;
        }
        if write.committed() {
            violations.push(format!("F1: zombie write {write} committed"));
        }
    }
    violations
}

/// When a stale-epoch cut paused its node: the instant of its cut owner
/// commit. Whatever the node does after it resumes is stamped later, and
/// runs past its self-stop deadline.
fn stale_pause(cut: Option<&Cut>, trace: &[Write]) -> Option<u64> {
    let cut = cut.filter(|cut| cut.fault == Fault::StaleEpoch && cut.kind == WriteKind::Actor)?;
    trace
        .iter()
        .find(|write| {
            write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
        })
        .map(|write| write.at_ms)
}

/// F2, NR-1, NR-2 and NR-3 over every owner's persisted records, and the
/// fold that reads them; S4 over the `x_start` commits in `trace`; with
/// `paused_at`, a stale-epoch cut's pause, the lease law too.
async fn once(
    world: &World,
    nodes: &SimNodes,
    trace: &[Write],
    paused_at: Option<u64>,
) -> Vec<String> {
    let mut violations = Vec::new();
    let counts = world.tripwire().counts();
    let ledger = world.ledger().entries();
    let mut owners: BTreeSet<OwnerKey> = ledger.keys().map(|(owner, _)| owner.clone()).collect();
    owners.extend(counts.bodies.keys().map(|id| id.owner.clone()));
    // What the trace saw commit, not what the records hold at the end: a
    // cell's snapshot prunes the runs no snapshot reaches again.
    let started: BTreeSet<&AdmittedId> = trace
        .iter()
        .filter(|write| write.committed())
        .flat_map(|write| &write.starts)
        .collect();
    for owner in owners {
        let rows = match nodes.database().run_records(&owner).await {
            Ok(rows) => rows,
            Err(error) => {
                violations.push(format!("fold: {owner:?}'s records do not read: {error}"));
                continue;
            }
        };
        let waits = match PinnedWaits::read(nodes.database().as_ref(), &rows).await {
            Ok(waits) => waits,
            Err(error) => {
                violations.push(format!(
                    "fold: {owner:?}'s pinned waits do not read: {error}"
                ));
                continue;
            }
        };
        let folded = match fold(&rows, &PolicyView::new([]), &waits) {
            Ok(folded) => folded,
            Err(refusal) => {
                violations.push(format!(
                    "fold: {owner:?}'s records are refused: {refusal:?}"
                ));
                continue;
            }
        };
        for (id, recovery) in folded.recoveries() {
            let Some(execution) = folded.admitted(id) else {
                violations.push(format!("fold: {id:?} recovers with no admission"));
                continue;
            };
            let entered = counts.bodies.get(id).copied().unwrap_or(0);
            let once = execution.policy() == ExecutionPolicy::Once;
            if once && entered > 1 {
                violations.push(format!("F2: Once body {id:?} was entered {entered} times"));
            }
            if !once && entered > 2 {
                violations.push(format!(
                    "NR-3: Repeatable body {id:?} was entered {entered} times for one cut"
                ));
            }
            match recovery {
                Recovery::Settled(SettledOutput::Completed(_) | SettledOutput::Failed(_))
                    if entered == 0 =>
                {
                    violations.push(format!(
                        "NR-1: {id:?} settled with a body's outcome but no body ran"
                    ));
                }
                Recovery::Settled(SettledOutput::Interrupted) if entered > 1 => {
                    violations.push(format!(
                        "NR-2: interrupted {id:?} was entered {entered} times"
                    ));
                }
                Recovery::Interrupt if entered > 1 => violations.push(format!(
                    "NR-2: {id:?}, started without an outcome, was entered {entered} times"
                )),
                _ => {}
            }
            // The moved owner interrupts a `Once` its paused node admitted;
            // a body entered after the pause ran on that node past its lease.
            if let Some(paused_at) = paused_at
                && once
                && matches!(
                    recovery,
                    Recovery::Settled(SettledOutput::Interrupted) | Recovery::Interrupt
                )
                && let Some(entries) = ledger.get(&(id.owner.clone(), execution.call().clone()))
                && let Some(late) = entries.iter().find(|entry| entry.at_ms > paused_at)
            {
                violations.push(format!(
                    "S4: interrupted {id:?} ({}) was entered at {} ms, after its node paused \
                     at {paused_at} ms past its lease",
                    late.tool, late.at_ms
                ));
            }
        }
    }
    for (id, entered) in &counts.bodies {
        if !started.contains(id) {
            violations.push(format!(
                "S4: body {id:?} was entered {entered} times with no x_start in the records"
            ));
        }
    }
    for ((owner, call), entries) in &ledger {
        let once = entries
            .iter()
            .any(|entry| entry.policy == ExecutionPolicy::Once);
        if once && entries.len() > 1 {
            violations.push(format!(
                "F2: Once call {call} of {owner:?} reached the outside world {} times",
                entries.len()
            ));
        }
    }
    violations
}

/// NR-4: no outcome looked up for re-running code, no committed ordinal
/// emitted again, and at most `max_restores` restores per turn, one more
/// for a cut.
fn no_replay(world: &World, cut: Option<&Cut>, max_restores: usize) -> Vec<String> {
    let counts = world.tripwire().counts();
    let mut violations = Vec::new();
    let lookups: usize = counts.outcome_lookups.values().sum();
    if lookups > 0 {
        violations.push(format!(
            "NR-4: {lookups} outcome lookups for re-running code: {:?}",
            counts.outcome_lookups
        ));
    }
    let emitted: usize = counts.committed_ordinals.values().sum();
    if emitted > 0 {
        violations.push(format!(
            "NR-4: {emitted} committed ordinals emitted again: {:?}",
            counts.committed_ordinals
        ));
    }
    let bound = max_restores + usize::from(cut.is_some());
    for ((session, run), restores) in &counts.restores {
        if *restores > bound {
            violations.push(format!(
                "NR-4: turn {run} of {session} restored {restores} times, above {bound}"
            ));
        }
    }
    violations
}

/// S4: every body found its `x_start` committed when it was entered.
fn admission_first(world: &World) -> Vec<String> {
    world
        .ledger()
        .entries()
        .into_iter()
        .flat_map(|((owner, call), entries)| {
            entries
                .into_iter()
                .filter(|entry| !entry.admitted)
                .map(move |entry| {
                    format!(
                        "S4: {} body for {call} of {owner:?} ran at {} ms before its x_start committed",
                        entry.tool, entry.at_ms
                    )
                })
        })
        .collect()
}

/// Every actor of the run ends in a terminal or a durable wait.
async fn settled(world: &World, nodes: &SimNodes) -> Vec<String> {
    let mut violations = Vec::new();
    let database = nodes.database();
    for actor in world.actors() {
        let snapshot = match database.actor(&actor).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                violations.push(format!("actors: {actor} has no row"));
                continue;
            }
            Err(error) => {
                violations.push(format!("actors: {actor} does not read: {error}"));
                continue;
            }
        };
        match snapshot.state {
            ActorState::Idle | ActorState::Terminal => {}
            ActorState::Ready => {
                violations.push(format!("actors: {actor} is stuck ready with no claimer"));
            }
            ActorState::Parked => violations.push(format!("actors: {actor} is parked")),
            ActorState::Owned => {
                let owner = snapshot
                    .owner
                    .as_ref()
                    .map(|owner| owner.node.as_str().to_owned());
                if !owner.as_deref().is_some_and(|node| nodes.serving(node)) {
                    violations.push(format!(
                        "actors: {actor} is owned by {owner:?}, which no longer serves"
                    ));
                }
            }
            // A wait on mail or a due time is durable; mail it has not read
            // should have readied it.
            ActorState::Waiting if snapshot.has_mail => {
                violations.push(format!("actors: {actor} waits with unread mail"));
            }
            ActorState::Waiting => {}
        }
    }
    violations
}
