//! Same-file hints belong to the member that produced the work. TEMP capture
//! rows and the resulting wake rows roll back with that member's savepoint.
//!
//! The capture triggers also set the connection's capture mark
//! (`lash_wake_captured()`), and a member's flush reads the capture tables
//! only when the mark is set (FIG-6617). The tables are connection-local
//! and only those triggers fill them; every write on a connection with
//! wakes installed is a batch member, whose flush empties them or whose
//! savepoint undoes its own rows, so they are empty whenever the mark is
//! clear. A mark left set by rolled-back rows costs one empty read.

use std::ffi::{c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core_ids::clock::Clock;
use lash_durable::{ActorKey, ActorState, BootId, NodeId, Owner, WakeBatch, Woken};
use rusqlite::Connection;

use super::{INSERT, PRUNE, SqliteDurableStore, rows};
use crate::conn::cached_execute;

pub(crate) struct NodeWakeWriter {
    clock: Arc<dyn Clock>,
    sink: lash_durable::runner::WakeSink,
    retention: std::time::Duration,
    captured: CaptureMark,
}

/// Set by a capture trigger on the writer's connection, cleared once a
/// flush has emptied the capture tables.
#[derive(Clone, Default)]
pub(crate) struct CaptureMark(Arc<AtomicBool>);

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
            captured: CaptureMark::default(),
        }
    }

    /// The mark this writer's capture triggers set.
    pub(crate) fn mark(&self) -> CaptureMark {
        self.captured.clone()
    }

    pub(crate) fn flush(&self, tx: &Connection) -> rusqlite::Result<()> {
        if !self.captured.0.load(Ordering::Acquire) {
            return Ok(());
        }
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
            self.captured.0.store(false, Ordering::Release);
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
        self.captured.0.store(false, Ordering::Release);
        Ok(())
    }
}

impl CaptureMark {
    /// Register `lash_wake_captured()`, which sets this mark, on
    /// `connection`, before the capture triggers that call it exist.
    pub(crate) fn register(&self, connection: &Connection) -> rusqlite::Result<()> {
        let mark = Arc::into_raw(Arc::clone(&self.0))
            .cast_mut()
            .cast::<c_void>();
        // SAFETY: the handle is this live connection's, used on its own
        // thread. SQLite owns `mark` from here, also when the call fails, and
        // hands it to `release_mark` exactly once.
        #[expect(
            unsafe_code,
            reason = "rusqlite's safe functions need its `functions` feature"
        )]
        let status = unsafe {
            rusqlite::ffi::sqlite3_create_function_v2(
                connection.handle(),
                c"lash_wake_captured".as_ptr(),
                0,
                rusqlite::ffi::SQLITE_UTF8 | rusqlite::ffi::SQLITE_INNOCUOUS,
                mark,
                Some(set_mark),
                None,
                None,
                Some(release_mark),
            )
        };
        if status == rusqlite::ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(status),
                None,
            ))
        }
    }
}

/// `lash_wake_captured()`: set the mark SQLite holds as the function's data.
#[expect(unsafe_code, reason = "a SQLite function callback")]
unsafe extern "C" fn set_mark(
    context: *mut rusqlite::ffi::sqlite3_context,
    _arguments: c_int,
    _values: *mut *mut rusqlite::ffi::sqlite3_value,
) {
    // SAFETY: the data is the `AtomicBool` `CaptureMark::register` leaked, alive
    // until `release_mark`.
    unsafe {
        let mark = rusqlite::ffi::sqlite3_user_data(context).cast::<AtomicBool>();
        (*mark).store(true, Ordering::Release);
        rusqlite::ffi::sqlite3_result_null(context);
    }
}

/// Drop the mark's reference SQLite held.
#[expect(unsafe_code, reason = "a SQLite function destructor")]
unsafe extern "C" fn release_mark(mark: *mut c_void) {
    // SAFETY: `mark` came from `Arc::into_raw` in `CaptureMark::register`, and SQLite
    // releases it once.
    drop(unsafe { Arc::from_raw(mark.cast::<AtomicBool>()) });
}
