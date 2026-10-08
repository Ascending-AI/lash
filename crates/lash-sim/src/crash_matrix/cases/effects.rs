//! A store-local start and its call outcome commit together at every cut.

use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_core_execution::runtime::actor::round::SettledOutput;
use lash_core_execution::runtime::actor::round::{PinnedWaits, PolicyView, Recovery, fold};
use lash_core_store::store::RunTerminalKind;
use lash_durable::domain::OwnerKey;
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::{SessionId, ToolCallId};

use super::{admit_turn, run_of, turn_end, turn_settled};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::effects::spawn_key;
use crate::crash_matrix::services::{Tool, TurnScript};
use crate::crash_matrix::world::World;

#[derive(Default)]
pub struct EffectsCase {
    tag: String,
    session: Mutex<Option<SessionId>>,
}

impl EffectsCase {
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
impl Workload for EffectsCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let admitted =
            admit_turn(world, TurnScript::Effects, &format!("effects{}", self.tag)).await?;
        *self.session.lock_recover() = Some(admitted);
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
        let mut violations = Vec::new();
        match turn_end(nodes, &session).await {
            Some(end) if end.kind() == RunTerminalKind::Answered => {}
            other => violations.push(format!("the effects' turn ended {other:?}")),
        }
        violations.extend(
            world
                .notes()
                .into_iter()
                .filter(|note| note.starts_with("effect.refused")),
        );
        let outcomes = match outcomes(nodes, &session).await {
            Ok(outcomes) => outcomes,
            Err(error) => return vec![error],
        };
        for (tool, call, completed) in outcomes {
            if tool == Tool::Spawn {
                let registered = match world.backend() {
                    Ok(backend) => backend
                        .process_registry()
                        .get_process_by_start_key(&spawn_key(&call))
                        .await
                        .map(|record| record.is_some())
                        .unwrap_or(false),
                    Err(_) => false,
                };
                if registered != completed {
                    violations.push(format!(
                        "store-local start: call {call} completed {completed}, its process registered {registered}"
                    ));
                }
            }
        }
        violations
    }
}

/// Each settled call of `session`'s round: its tool, its call and whether
/// its outcome completed.
async fn outcomes(
    nodes: &SimNodes,
    session: &SessionId,
) -> Result<Vec<(Tool, ToolCallId, bool)>, String> {
    let owner = OwnerKey::Turn(session.clone(), run_of(session));
    let rows = nodes
        .database()
        .run_records(&owner)
        .await
        .map_err(|error| format!("the round's records do not read: {error}"))?;
    let waits = PinnedWaits::read(nodes.database().as_ref(), &rows)
        .await
        .map_err(|error| format!("the round's pinned waits do not read: {error}"))?;
    let folded = fold(&rows, &PolicyView::new([]), &waits)
        .map_err(|refusal| format!("the round's records do not fold: {refusal:?}"))?;
    let mut settled = Vec::new();
    for (id, recovery) in folded.recoveries() {
        let Some(execution) = folded.admitted(id) else {
            continue;
        };
        let tool = match execution.draft().tool().as_str() {
            name if name == Tool::Spawn.name() => Tool::Spawn,
            _ => continue,
        };
        let Recovery::Settled(outcome) = recovery else {
            return Err(format!("the {} call {id:?} never settled", tool.name()));
        };
        settled.push((
            tool,
            execution.call().clone(),
            matches!(outcome, SettledOutput::Completed(_)),
        ));
    }
    if settled.len() != 1 {
        return Err(format!(
            "the round settled {} of its one effect call",
            settled.len()
        ));
    }
    Ok(settled)
}
