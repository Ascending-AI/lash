//! The tool-round seam: one turn whose model calls a slow `Once` write, a
//! flaky `Repeatable` and a quick `Once` write, so their outcomes commit out
//! of order; the round is presented and the model answers.
//!
//! Laws: the turn answered; the flaky tool's attempts advance only by its
//! known failure (attempt 1 fails, attempt 2 completes, and a crash reruns
//! an attempt at its own ordinal); and every model call that sees the
//! round sees its results in the order the model declared the calls.

use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_durable::domain::TurnTerminal;
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::SessionId;

use super::{admit_turn, turn_end, turn_settled};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::services::{Tool, TurnScript};
use crate::crash_matrix::world::World;

/// The order the model declares the round's calls in.
const DECLARED: [Tool; 3] = [Tool::WriteSlow, Tool::Flaky, Tool::WriteNow];

#[derive(Default)]
pub struct RoundCase {
    tag: String,
    session: Mutex<Option<SessionId>>,
}

impl RoundCase {
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
impl Workload for RoundCase {
    async fn seed(&self, world: &Arc<World>, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = admit_turn(world, TurnScript::Round, &format!("round{}", self.tag)).await?;
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
        let mut violations = Vec::new();
        match turn_end(nodes, &session).await {
            Some(end) if end.terminal == TurnTerminal::Answered => {}
            other => violations.push(format!("the round's turn ended {other:?}")),
        }
        for (call, entries) in world.ledger().of_tool(Tool::Flaky.name()) {
            let attempts: Vec<u32> = entries.iter().map(|entry| entry.attempt).collect();
            let ordered = attempts.windows(2).all(|pair| pair[0] <= pair[1]);
            if !ordered || attempts.first() != Some(&1) || attempts.last() != Some(&2) {
                violations.push(format!(
                    "retry ownership: flaky call {:?} ran attempts {attempts:?}",
                    call.1
                ));
            }
        }
        let declared = DECLARED.map(Tool::name).join(",");
        let prefix = format!("model.results {session} ");
        for note in world.notes() {
            if let Some(order) = note.strip_prefix(&prefix)
                && order != declared
            {
                violations.push(format!(
                    "declared order: a model call saw the round as {order}, not {declared}"
                ));
            }
        }
        violations
    }
}
