//! The cell seam: one turn, behind the facade on the production driver,
//! whose model writes a TypeScript cell that calls the `Once` tool
//! `ext_write`, raced against a sleep longer than the matrix horizon; the
//! cell snapshots with the model response that asked for it and both
//! admissions (`model.done+cell.snapshot+admit`,
//! FIG-6000), the body runs, its outcome
//! commits (`round.outcome`), the cell finishes from its snapshot with the
//! sleep still pending, and its end commits on its own (`cell.snapshot`).
//!
//! The killed variant's cell computes past its first slice first, so the
//! model response commits on its own (`model.done`) and the cell snapshots
//! with the operation's admission (`cell.snapshot+admit`); it holds the
//! operation's first body forever and the host
//! kills the node running it, then restarts it: another owner reaps the
//! dead one, restores the turn and finds the operation started without an
//! outcome, so it injects `Interrupted` into the cell (`cell.inject`), never
//! entering the body again. It races no sleep, so its end commits with
//! `turn.commit` even with a host plugin's checkpoint callback (FIG-6162).
//!
//! Laws: the turn answered; the cell's program is entered fresh only by an
//! activation that found no snapshot of it; unless its call was settled
//! `Interrupted`, every model call after the cell binds what the cell
//! bound; the killed variant's operation settled `Interrupted` with its
//! body entered at most once.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::sync::MutexExt as _;
use lash_core_store::store::RunTerminalKind;
use lash_durable::CommitLabel;
use lash_durable_test::{Cut, SimNodes};
use lash_vocabulary::identity::SessionId;

use super::{admit_turn, turn_end, turn_settled};
use crate::crash_matrix::cells::BOUND_AFTER_CELL;
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::services::{EXT_WRITE, TurnScript};
use crate::crash_matrix::world::{SOAK, World};

pub struct CellCase {
    killed: bool,
    tag: String,
    session: Mutex<Option<SessionId>>,
}

impl CellCase {
    /// The cell, or with `killed` its variant killed inside the body.
    #[must_use]
    pub fn new(killed: bool) -> Self {
        Self::tagged(killed, "")
    }

    /// [`Self::new`] with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(killed: bool, tag: &str) -> Self {
        Self {
            killed,
            tag: tag.to_owned(),
            session: Mutex::default(),
        }
    }

    fn session(&self) -> Option<SessionId> {
        self.session.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for CellCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let script = if self.killed {
            TurnScript::CellKilled
        } else {
            TurnScript::Cell
        };
        let session = admit_turn(world, script, &format!("cell{}", self.tag)).await?;
        *self.session.lock_recover() = Some(session);
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        match self.session() {
            Some(session) => turn_settled(nodes, &session).await,
            None => false,
        }
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let Some(session) = self.session() else {
            return vec!["the turn was never seeded".to_owned()];
        };
        let mut violations = Vec::new();
        match turn_end(nodes, &session).await {
            Some(end) if end.kind() == RunTerminalKind::Answered => {}
            other => violations.push(format!("the cell's turn ended {other:?}")),
        }
        // NR, no program re-entry: a fresh entry of the program happens only
        // while no snapshot of the cell is committed, so a second one only
        // when the cut fell before the first snapshot committed.
        let trace = nodes.script().trace();
        // The cell's first snapshot commits the model response that asked
        // for it (FIG-6000).
        let first_snapshot = trace.iter().position(|write| {
            (write.point.label == CommitLabel::CELL_SNAPSHOT_ADMIT
                || write.point.label == CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT)
                && write.committed()
        });
        let cut_at = cut.and_then(|cut| {
            trace
                .iter()
                .position(|write| write.point == cut.point && write.cut == Some(cut.fault))
        });
        let before_snapshot = match (cut_at, first_snapshot) {
            (Some(at), Some(snapshot)) => at <= snapshot,
            (Some(_), None) => true,
            (None, _) => false,
        };
        // A soak's faults are many, not one cut: its kills before a first
        // snapshot each enter the program once more. The matrix holds the
        // one-cut bound.
        let soak = world.noted(SOAK);
        for (exec, entered) in world.tripwire().counts().vm_programs {
            if soak {
                break;
            }
            let bound = if before_snapshot { 2 } else { 1 };
            if entered > bound {
                violations.push(format!(
                    "no program re-entry: {exec:?} was entered fresh {entered} times"
                ));
            }
        }
        violations.extend(carried_laws(world, nodes, &session));
        if self.killed {
            violations.extend(killed_laws(world, nodes, &session).await);
        }
        violations
    }

    /// A cell runs on the code mode worker subprocess, whose compute and respawn
    /// after a node's death the virtual clock does not see: every step the
    /// nodes take while the worker works moves time on. The cell is bounded
    /// by the matrix horizon alone, which its run must still end within.
    fn bound(&self) -> Duration {
        crate::crash_matrix::HORIZON
    }

    fn max_restores(&self) -> usize {
        // The kill's takeover restores the turn once more.
        if self.killed { 2 } else { 1 }
    }
}

/// The cell's binding reaches the turn's next prompt (FIG-6315): unless its
/// call was settled `Interrupted`, which ends the cell with an error and
/// binds nothing, every model call after the cell binds `written`, whether
/// its owner ran the cell to its end or read back the end a dead owner
/// recorded.
fn carried_laws(world: &World, nodes: &SimNodes, session: &SessionId) -> Vec<String> {
    let actor = lash_durable::ActorKey::session(session.as_str()).ok();
    let interrupted = nodes.script().trace().iter().any(|write| {
        write.point.label == CommitLabel::CELL_INJECT && write.committed() && write.actor == actor
    });
    if interrupted {
        return Vec::new();
    }
    let prefix = format!("{BOUND_AFTER_CELL}{session} ");
    let calls: Vec<String> = world
        .notes()
        .into_iter()
        .filter_map(|note| note.strip_prefix(&prefix).map(str::to_owned))
        .collect();
    if calls.is_empty() {
        return vec!["no model call followed the cell".to_owned()];
    }
    calls
        .into_iter()
        .filter(|bound| !bound.split(',').any(|name| name == "written"))
        .map(|bound| {
            format!("a model call after the cell bound [{bound}], not the cell's `written`")
        })
        .collect()
}

/// The killed variant: its operation settled `Interrupted` and its body ran
/// at most once. A cut that ended the owner after the operation's start
/// committed but before its body ran leaves the body unentered; a stale
/// owner may enter it after another owner settled it, as a zombie does.
///
/// The cell's run records do not outlive it (its last snapshot prunes every
/// run no snapshot can feed back), so the store's acknowledged commits are
/// the evidence: the operation settled once, as `cell.inject`, and never as
/// `round.outcome`.
async fn killed_laws(world: &World, nodes: &SimNodes, session: &SessionId) -> Vec<String> {
    let mut violations = Vec::new();
    let entered: usize = world
        .ledger()
        .of_tool(EXT_WRITE)
        .iter()
        .map(|(_, entries)| entries.len())
        .sum();
    if entered > 1 {
        violations.push(format!("the killed body was entered {entered} times"));
    }
    let actor = lash_durable::ActorKey::session(session.as_str()).ok();
    let committed = |label: CommitLabel| {
        nodes
            .script()
            .trace()
            .iter()
            .filter(|write| write.point.label == label && write.committed() && write.actor == actor)
            .count()
    };
    let injected = committed(CommitLabel::CELL_INJECT);
    let completed = committed(CommitLabel::ROUND_OUTCOME);
    if injected != 1 || completed != 0 {
        violations.push(format!(
            "the killed operation settled Interrupted {injected} times and completed {completed} times, not Interrupted once"
        ));
    }
    violations
}
