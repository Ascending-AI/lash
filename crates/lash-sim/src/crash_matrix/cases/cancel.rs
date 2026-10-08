//! The turn-cancel seam: one turn whose only tool runs until the turn's
//! cancel, which the host requests once the tool runs (`mail.session`); the
//! owner ends the turn in one `turn.cancel` commit. A detached idle process
//! also receives an operator cancellation and must end with that origin.
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
    process: Mutex<Option<lash_sansio::ProcessId>>,
}

impl CancelCase {
    /// The case with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            session: Mutex::default(),
            process: Mutex::default(),
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
        let process = super::register(
            world,
            crate::crash_matrix::engine::hold("cancelled-process"),
            None,
        )
        .await?;
        *self.process.lock_recover() = Some(process.clone());
        let host = Arc::clone(world);
        world.spawn(async move {
            use lash_core_execution::ProcessWorkSubstrate as _;
            let requested_at = host.now_ms();
            let _ = crate::crash_matrix::world::retry(&host, |backend| {
                let process = process.clone();
                async move {
                    backend.wake_process(&process).await?;
                    lash_core_execution::DurableProcessWork::new(backend)
                        .deliver_cancel(
                            &process,
                            &lash_core_execution::CancelRequest::new(
                                lash_core_execution::CancelOrigin::OperatorRequested,
                                "lash-sim",
                                requested_at,
                            ),
                            "",
                        )
                        .await
                        .map_err(|error| {
                            lash_durable::DurableError::Store(lash_durable::StoreFailure {
                                kind: lash_durable::StoreFailureKind::Unavailable,
                                message: error.to_string(),
                            })
                        })
                }
            })
            .await;
        });
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        match self.session() {
            Some(session) => {
                let process = self.process.lock_recover().clone();
                match process {
                    Some(process) => {
                        turn_settled(nodes, &session).await && super::ended(nodes, &[process]).await
                    }
                    None => false,
                }
            }
            None => false,
        }
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let Some(session) = self.session() else {
            return vec!["the turn was never seeded".to_owned()];
        };
        let final_call = world.noted(&format!("model.call {session} final"));
        let mut violations = match turn_end(nodes, &session).await {
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
        };
        let process = self.process.lock_recover().clone();
        match process {
            Some(process) => match super::outcome(world, &process).await {
                Some(end)
                    if super::find(&end, "origin")
                        == Some(&serde_json::json!("operator_requested")) => {}
                other => violations.push(format!(
                    "process cancel: operator request did not end the idle process: {other:?}"
                )),
            },
            None => violations.push("process cancel: no process was seeded".to_owned()),
        }
        violations
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
