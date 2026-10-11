//! Same-file hints belong to the member that produced the work. TEMP capture
//! rows and the resulting wake rows roll back with that member's savepoint.

use std::sync::Arc;

use lash_core_execution::Clock;
use lash_durable::{ActorKey, ActorState, BootId, NodeId, Owner, WakeBatch, Woken};
use rusqlite::Connection;

use super::{INSERT, PRUNE, SqliteDurableStore, rows};
use crate::conn::cached_execute;

pub(crate) struct NodeWakeWriter {
    clock: Arc<dyn Clock>,
    sink: lash_durable::runner::WakeSink,
    retention: std::time::Duration,
}

enum Captured {
    Woken(Woken),
    Appended(String),
}

impl NodeWakeWriter {
    pub(crate) fn new(store: &SqliteDurableStore) -> Self {
        Self {
            clock: Arc::clone(&store.clock),
            sink: store.conn.wakes().clone(),
            retention: store.conn.operational.wake_retention,
        }
    }

    pub(crate) fn flush(&self, tx: &Connection) -> rusqlite::Result<()> {
        let mut woken = Vec::new();
        let mut appended = Vec::new();
        let mut statement = tx.prepare_cached(crate::durable::NODE_WAKE_ROWS)?;
        let captured = statement.query_map([], |row| {
            let actor: String = row.get(0)?;
            let Some(state) = row.get::<_, Option<String>>(1)? else {
                return Ok(Captured::Appended(actor));
            };
            let node: Option<String> = row.get(2)?;
            let boot: Option<String> = row.get(3)?;
            Ok(Captured::Woken(Woken {
                actor: ActorKey::parse(&actor)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
                state: super::super::actor_state(&state)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
                owner: node.zip(boot).map(|(node, boot)| Owner {
                    node: NodeId::new(node),
                    boot: BootId::new(boot),
                }),
            }))
        })?;
        for capture in captured {
            match capture? {
                Captured::Woken(wake) => woken.push(wake),
                Captured::Appended(actor) => appended.push(actor),
            }
        }
        if woken.is_empty() && appended.is_empty() {
            return Ok(());
        }
        let now = i64::try_from(self.clock.timestamp_ms())
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let peers = if woken
            .iter()
            .any(|wake| wake.owner.is_none() && wake.state == ActorState::Ready)
        {
            tx.prepare_cached(crate::durable::NODE_WAKE_PEERS)?
                .query_map([now], |row| row.get::<_, String>(0).map(NodeId::new))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let mut batch = WakeBatch::default();
        for wake in &woken {
            let planned = self.sink.plan(wake, &peers);
            batch.ready.extend(planned.ready);
            for (node, actors) in planned.owned {
                batch.owned.entry(node).or_default().extend(actors);
            }
        }
        for actor in appended {
            batch.appended.insert(
                ActorKey::parse(&actor)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
            );
        }
        for (node, payload) in rows(&batch) {
            cached_execute(tx, INSERT, rusqlite::params![node, payload, now])?;
        }
        let oldest =
            now.saturating_sub(i64::try_from(self.retention.as_millis()).unwrap_or(i64::MAX));
        cached_execute(tx, PRUNE, [oldest])?;
        tx.execute_batch("DELETE FROM temp.lash_woken; DELETE FROM temp.lash_appended")?;
        Ok(())
    }
}
