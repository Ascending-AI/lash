//! The turn seam: one session's plain turn, admitted, run to its commit and
//! released; once it committed, the host restarts node `a` as a rolling
//! deploy would: it stops it cleanly, which releases what it owns, and
//! starts a new boot of it.
//!
//! Laws: the turn answered, with exactly one head advance.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::sync::MutexExt as _;
use lash_durable::domain::TurnTerminal;
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::SessionId;

use super::{LOOK_EVERY, LOOKS, admit_turn, turn_end, turn_settled};
use crate::crash_matrix::deployment::{NODES, Workload};
use crate::crash_matrix::services::TurnScript;
use crate::crash_matrix::world::{World, poll};

/// Stop a serving node cleanly and start a new boot of it, as a rolling
/// deploy does. Another host's restart of the same node at the same time
/// makes this one's a no-op.
async fn restart_serving(host: &World, nodes: &SimNodes) {
    let serving = poll(host, LOOK_EVERY, LOOKS, || async {
        NODES.into_iter().find(|node| nodes.serving(node))
    })
    .await;
    let Some(serving) = serving else {
        return;
    };
    nodes.stop(serving);
    let stopped = poll(host, LOOK_EVERY, LOOKS, || async {
        (!nodes.serving(serving)).then_some(())
    })
    .await;
    if stopped.is_some() {
        nodes.start(serving);
    }
}

#[derive(Default)]
pub struct TurnCase {
    tag: String,
    session: Mutex<Option<SessionId>>,
}

impl TurnCase {
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
impl Workload for TurnCase {
    async fn seed(&self, world: &Arc<World>, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = admit_turn(world, TurnScript::Plain, &format!("turn{}", self.tag)).await?;
        *self.session.lock_recover() = Some(session.clone());
        let host = Arc::clone(world);
        world.spawn(async move {
            let Some(nodes) = host.nodes() else {
                return;
            };
            let committed = poll(&host, LOOK_EVERY, LOOKS, || async {
                turn_end(&nodes, &session).await.map(drop)
            })
            .await;
            if committed.is_none() {
                return;
            }
            // Long enough for the nodes to renew their leases.
            host.sleep(Duration::from_secs(5)).await;
            restart_serving(&host, &nodes).await;
            host.note(format!("restarted for {session}"));
        });
        Ok(())
    }

    async fn done(&self, world: &World, nodes: &SimNodes) -> bool {
        let Some(session) = self.session() else {
            return false;
        };
        turn_settled(nodes, &session).await && world.noted(&format!("restarted for {session}"))
    }

    async fn laws(&self, _world: &World, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let Some(session) = self.session() else {
            return vec!["the turn was never seeded".to_owned()];
        };
        match turn_end(nodes, &session).await {
            Some(end) if end.terminal == TurnTerminal::Answered && end.head_revision.is_some() => {
                Vec::new()
            }
            other => vec![format!(
                "the turn ended {other:?}, not answered with a head"
            )],
        }
    }
}
