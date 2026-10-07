//! The cell seam: one turn, behind the facade on the production driver,
//! whose model writes a TypeScript cell that calls the `Once` tool
//! `ext_write`; the cell snapshots with the
//! operation's admission (`cell.snapshot+admit`), the body runs, its outcome
//! commits (`round.outcome`), the cell finishes from its snapshot and the
//! model answers with its result.
//!
//! The killed variant holds the operation's first body forever and the host
//! kills the node running it, then restarts it: another owner reaps the
//! dead one, restores the turn and finds the operation started without an
//! outcome, so it injects `Interrupted` into the cell (`cell.inject`), never
//! entering the body again.
//!
//! Laws: the turn answered; the cell's program is entered fresh only by an
//! activation that found no snapshot of it; the killed variant's operation
//! settled `Interrupted` with its body entered at most once.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::sync::MutexExt as _;
use lash_durable::CommitLabel;
use lash_durable::domain::TurnTerminal;
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::SessionId;

use super::{admit_turn, turn_end, turn_settled};
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
    async fn seed(&self, world: &Arc<World>, nodes: &Arc<SimNodes>) -> Result<(), String> {
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
            Some(end) if end.terminal == TurnTerminal::Answered => {}
            other => violations.push(format!("the cell's turn ended {other:?}")),
        }
        // NR, no program re-entry: a fresh entry of the program happens only
        // while no snapshot of the cell is committed, so a second one only
        // when the cut fell before the first snapshot committed.
        let trace = nodes.script().trace();
        let first_snapshot = trace.iter().position(|write| {
            write.point.label == CommitLabel::CELL_SNAPSHOT_ADMIT && write.committed()
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
        if self.killed {
            violations.extend(killed_laws(world, nodes, &session).await);
        }
        violations
    }

    fn bound(&self) -> Duration {
        if self.killed {
            // The kill's own failover and restart.
            Duration::from_secs(60)
        } else {
            Duration::from_secs(30)
        }
    }

    fn max_restores(&self) -> usize {
        // The kill's takeover restores the turn once more.
        if self.killed { 2 } else { 1 }
    }
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
