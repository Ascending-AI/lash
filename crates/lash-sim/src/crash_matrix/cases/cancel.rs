//! The turn-cancel seam: one turn whose only tool runs until the turn's
//! cancel, which the host requests once the tool runs (`mail.session`); the
//! owner ends the turn in one `turn.cancel` commit.
//!
//! Laws: a cancelled turn calls the model no more and publishes no head;
//! a turn that answered instead did so only because a crash interrupted the
//! tool before the cancel reached it, so the tool settled `Interrupted`.

use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_core_store::store::RunTerminalKind;
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::SessionId;

use super::{admit_turn, run_of, turn_end, turn_settled};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::services::{Tool, TurnScript};
use crate::crash_matrix::world::World;

#[derive(Default)]
pub struct CancelCase {
    tag: String,
    session: Mutex<Option<SessionId>>,
}

impl CancelCase {
    /// The case with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            session: Mutex::default(),
        }
    }

    fn session(&self) -> Option<SessionId> {
        self.session.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for CancelCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = admit_turn(world, TurnScript::Hang, &format!("cancel{}", self.tag)).await?;
        *self.session.lock_recover() = Some(session);
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        match self.session() {
            Some(session) => turn_settled(nodes, &session).await,
            None => false,
        }
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let Some(session) = self.session() else {
            return vec!["the turn was never seeded".to_owned()];
        };
        let final_call = world.noted(&format!("model.call {session} final"));
        match turn_end(nodes, &session).await {
            Some(end) if end.kind() == RunTerminalKind::Cancelled => {
                let mut violations = Vec::new();
                if final_call {
                    violations.push("turn cancel: the model was called again".to_owned());
                }
                if end.head_revision.is_some() {
                    violations.push("turn cancel: the cancelled turn published a head".to_owned());
                }
                violations
            }
            Some(end) if end.kind() == RunTerminalKind::Answered => {
                let run = run_of(&session);
                let interrupted = interrupted_hang(nodes, &session, &run).await;
                if interrupted {
                    Vec::new()
                } else {
                    vec![
                        "turn cancel: the turn answered though its tool was never interrupted"
                            .to_owned(),
                    ]
                }
            }
            other => vec![format!("turn cancel: the turn ended {other:?}")],
        }
    }
}

/// Whether the hanging tool's call settled `Interrupted` in its turn's
/// records.
async fn interrupted_hang(
    nodes: &SimNodes,
    session: &SessionId,
    run: &lash_sansio::TurnId,
) -> bool {
    use lash_core_execution::runtime::actor::round::SettledOutput;
    use lash_core_execution::runtime::actor::round::{PolicyView, Recovery, fold};
    let owner = lash_durable::domain::OwnerKey::Turn(session.clone(), run.clone());
    let Ok(rows) = nodes.database().run_records(&owner).await else {
        return false;
    };
    let Ok(folded) = fold(&rows, &PolicyView::new([])) else {
        return false;
    };
    folded.recoveries().iter().any(|(id, recovery)| {
        folded
            .admitted(id)
            .is_some_and(|execution| execution.draft().tool().as_str() == Tool::Hang.name())
            && matches!(recovery, Recovery::Settled(SettledOutput::Interrupted))
    })
}
