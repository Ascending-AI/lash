//! Each batch member captures wake facts in memory on its writer connection.
//! Triggers pass actor identities and values together; no capture rows or
//! actor rereads are needed. The member starts with empty capture, flushes
//! hints inside its savepoint only on success, and discards capture on every
//! exit. Only the outer committed transaction rings listeners.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core_ids::clock::Clock;
use lash_durable::{ActorKey, ActorState, BootId, NodeId, Owner, WakeBatch, Woken};
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;

use super::doorbell::{Doorbell, Rung};
use super::{INSERT, SqliteDurableStore, rows};
use crate::conn::cached_execute;

pub(crate) struct NodeWakeWriter {
    clock: Arc<dyn Clock>,
    sink: lash_durable::runner::WakeSink,
    captured: Capture,
    /// Rings the listeners the batch's rows name once it commits.
    doorbell: Doorbell,
    /// The listeners the batch in flight wrote rows for.
    rung: std::sync::Mutex<Rung>,
}

/// Connection-local capture, owned by the current batch member.
#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Captured>>);

#[derive(Default)]
struct Captured {
    woken: BTreeMap<ActorKey, Woken>,
    appended: BTreeSet<ActorKey>,
}

impl NodeWakeWriter {
    pub(crate) fn new(store: &SqliteDurableStore, doorbell: Doorbell) -> Self {
        Self {
            clock: Arc::clone(&store.clock),
            sink: store.conn.wakes().clone(),
            captured: Capture::default(),
            doorbell,
            rung: std::sync::Mutex::default(),
        }
    }

    /// The batch committed: ring the listeners its rows name, once each.
    pub(crate) fn committed(&self) {
        let rung = std::mem::take(&mut *self.rung());
        self.doorbell.ring(&rung);
    }

    /// The batch did not commit: its rows ring nobody.
    pub(crate) fn discard(&self) {
        *self.rung() = Rung::default();
        self.begin_member();
    }

    fn rung(&self) -> std::sync::MutexGuard<'_, Rung> {
        self.rung
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The capture this writer's triggers fill.
    pub(crate) fn capture(&self) -> Capture {
        self.captured.clone()
    }

    /// A failed or rolled-back member must never donate hints to a sibling.
    pub(crate) fn begin_member(&self) {
        *self.captured.lock() = Captured::default();
    }

    pub(crate) fn flush(&self, tx: &Connection) -> rusqlite::Result<()> {
        let Captured { woken, appended } = std::mem::take(&mut *self.captured.lock());
        if woken.is_empty() && appended.is_empty() {
            return Ok(());
        }
        let now = i64::try_from(self.clock.timestamp_ms())
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let peers = if woken
            .values()
            .any(|wake| wake.owner.is_none() && wake.state == ActorState::Ready)
        {
            tx.prepare_cached(crate::durable::NODE_WAKE_PEERS)?
                .query_map([now], |row| row.get::<_, String>(0).map(NodeId::new))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let mut batch = WakeBatch::default();
        for wake in woken.values() {
            let planned = self.sink.plan(wake, &peers);
            batch.ready.extend(planned.ready);
            for (node, actors) in planned.owned {
                batch.owned.entry(node).or_default().extend(actors);
            }
        }
        batch.appended = appended;
        for (node, payload) in rows(&batch) {
            cached_execute(tx, INSERT, rusqlite::params![node, payload, now])?;
            self.rung().add(node.as_deref());
        }
        Ok(())
    }
}

impl Capture {
    /// The owner revision advanced without changing follow facts. Remove
    /// its generic revision hint inside this member; mail routing and other
    /// actors' appended logs remain captured. A refused member still drops
    /// its whole capture through the writer's normal rollback path.
    pub(crate) fn unchanged_follow(&self, actor: &ActorKey) {
        self.lock().appended.remove(actor);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Captured> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Install callbacks before creating the connection-local capture triggers.
    pub(crate) fn register(&self, connection: &Connection) -> rusqlite::Result<()> {
        let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_INNOCUOUS;
        let capture = self.clone();
        connection.create_scalar_function("lash_capture_woken", 5, flags, move |ctx| {
            let actor = captured_actor(ctx.get::<String>(0)?)?;
            let state: Option<String> = ctx.get(1)?;
            let mut captured = capture.lock();
            // Deletion removes the actor just as the former capture join did.
            let Some(state) = state else {
                captured.woken.remove(&actor);
                return Ok(None::<String>);
            };
            // Later ownership/state writes refresh an already captured actor:
            // routing must use the member's final facts, not its first wake.
            if ctx.get::<bool>(4)? || captured.woken.contains_key(&actor) {
                let node: Option<String> = ctx.get(2)?;
                let boot: Option<String> = ctx.get(3)?;
                let state = super::super::actor_state(&state)
                    .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
                captured.woken.insert(
                    actor.clone(),
                    Woken {
                        actor,
                        state,
                        owner: node.zip(boot).map(|(node, boot)| Owner {
                            node: NodeId::new(node),
                            boot: BootId::new(boot),
                        }),
                    },
                );
            }
            Ok(None::<String>)
        })?;
        let capture = self.clone();
        connection.create_scalar_function("lash_capture_appended", 1, flags, move |ctx| {
            capture
                .lock()
                .appended
                .insert(captured_actor(ctx.get::<String>(0)?)?);
            Ok(None::<String>)
        })
    }
}

fn captured_actor(actor: String) -> rusqlite::Result<ActorKey> {
    ActorKey::parse(&actor).map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
}
