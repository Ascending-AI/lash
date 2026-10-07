//! The session-close seam: a session runs a plain turn and holds a process
//! living `Until` the session; once the turn committed, the host requests
//! the session's close (`mail.session`). The session actor closes itself,
//! one labelled step at a time (`session.close.*`): it cancels its turn,
//! revokes its waits, marks its `Until` process for cancel, waits for that
//! process's terminal (`wait.mint`), deletes its triggers, arms its
//! artifact cleanup and writes its tombstone.
//!
//! Laws: the close ends at its tombstone with the session's actor terminal;
//! its process ended cancelled by its parent's end before the tombstone;
//! the session's storage is deleted and it keeps no pending wait.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::runtime::durable::session_close::{SessionCloseRequested, request_session_close};
use lash_core::store::SessionLookup;
use lash_core::sync::MutexExt as _;
use lash_core_execution::{ProcessId, ScopeId};
use lash_durable::ActorState;
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::SessionId;
use serde_json::json;

use super::{
    LOOK_EVERY, LOOKS, admit_turn, ended, find, outcome, register, session_actor, state, turn_end,
};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::engine::hold;
use crate::crash_matrix::services::TurnScript;
use crate::crash_matrix::world::{World, poll, retry};

#[derive(Clone, Default)]
struct Seeded {
    session: Option<SessionId>,
    child: Option<ProcessId>,
}

#[derive(Default)]
pub struct CloseCase {
    tag: String,
    seeded: Mutex<Seeded>,
}

impl CloseCase {
    /// The case with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            seeded: Mutex::default(),
        }
    }

    fn seeded(&self) -> Seeded {
        self.seeded.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for CloseCase {
    async fn seed(&self, world: &Arc<World>, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = admit_turn(
            world,
            nodes,
            TurnScript::Plain,
            &format!("close{}", self.tag),
        )
        .await?;
        let child = register(world, hold("k"), Some(ScopeId::Session(session.clone()))).await?;
        *self.seeded.lock_recover() = Seeded {
            session: Some(session.clone()),
            child: Some(child),
        };
        let host = Arc::clone(world);
        let nodes = Arc::clone(nodes);
        world.spawn(async move {
            let committed = poll(&host, LOOK_EVERY, LOOKS, || async {
                turn_end(&nodes, &session).await.map(drop)
            })
            .await;
            if committed.is_none() {
                return;
            }
            let answer = retry(&host, |backend| {
                let session = session.clone();
                async move { request_session_close(&backend, &session).await }
            })
            .await;
            if matches!(
                answer,
                Ok(SessionCloseRequested::Requested | SessionCloseRequested::AlreadyClosed)
            ) {
                host.note("close.requested");
            }
        });
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        let seeded = self.seeded();
        let (Some(session), Some(child)) = (seeded.session, seeded.child) else {
            return false;
        };
        let Ok(actor) = session_actor(&session) else {
            return false;
        };
        state(nodes, &actor).await == Some(ActorState::Terminal) && ended(nodes, &[child]).await
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let seeded = self.seeded();
        let (Some(session), Some(child)) = (seeded.session, seeded.child) else {
            return vec!["the session was never seeded".to_owned()];
        };
        let mut violations = Vec::new();
        let database = nodes.database();
        match database.session_close(&session).await {
            Ok(Some(row)) if row.is_tombstone() => {}
            other => violations.push(format!("the close did not end at its tombstone: {other:?}")),
        }
        match outcome(world, &child).await {
            Some(end) if find(&end, "origin") == Some(&json!("parent_ended")) => {}
            other => violations.push(format!(
                "the session's process did not end by its parent's end: {other:?}"
            )),
        }
        if let Ok(actor) = session_actor(&session)
            && !database
                .pending_waits(&actor)
                .await
                .unwrap_or_default()
                .is_empty()
        {
            violations.push("the closed session kept pending waits".to_owned());
        }
        if let Ok(backend) = world.backend() {
            match backend
                .session_store_factory()
                .lookup_session(&session)
                .await
            {
                Ok(SessionLookup::Deleted) => {}
                other => violations.push(format!(
                    "the closed session's storage is not deleted: {other:?}"
                )),
            }
        }
        violations
    }

    fn bound(&self) -> Duration {
        Duration::from_secs(45)
    }
}
