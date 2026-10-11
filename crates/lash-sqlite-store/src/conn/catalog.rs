//! A connection's verified, stored members of the finite build catalog.
//!
//! Runtime nodes never enter the cache. SQLite's update hook invalidates it
//! on blob updates or deletion, including rolled-back mutations; data_version
//! invalidates changes from other connections. New entries are retained only
//! across operations that changed no blobs and no external data version.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use rusqlite::{Connection, OptionalExtension, hooks::Action};

#[derive(Default)]
struct State {
    version: Option<(i64, i64)>,
    changes: u64,
    hooks: u64,
    inserted: Vec<i64>,
    nodes: BTreeMap<NodeId, StoredMember>,
    rowids: BTreeMap<i64, NodeId>,
}

struct StoredMember {
    node: EncodedNode,
    rowid: i64,
}

impl State {
    fn clear(&mut self) {
        self.nodes.clear();
        self.rowids.clear();
        self.inserted.clear();
    }

    fn remove(&mut self, id: NodeId) {
        if let Some(member) = self.nodes.remove(&id) {
            self.rowids.remove(&member.rowid);
        }
    }
}

#[derive(Default)]
pub(crate) struct StoredCatalog(Mutex<State>);

struct Operation {
    catalog: Arc<StoredCatalog>,
    checked_version: bool,
    pending: BTreeMap<NodeId, StoredMember>,
    total_before: u64,
    hooks_before: u64,
    cacheable: bool,
}

thread_local! {
    static OPERATION: RefCell<Option<Operation>> = const { RefCell::new(None) };
}

struct OperationGuard;

impl Drop for OperationGuard {
    fn drop(&mut self) {
        OPERATION.with_borrow_mut(|slot| {
            slot.take();
        });
    }
}

fn version(connection: &Connection) -> rusqlite::Result<(i64, i64)> {
    connection.query_row(
        "SELECT data_version, schema_version FROM pragma_data_version, pragma_schema_version",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

fn refresh(
    connection: &Connection,
    state: &mut State,
    operation: &mut Operation,
) -> rusqlite::Result<()> {
    // SQLite does not emit update hooks for truncate deletes. Counting all
    // hooks against total_changes catches those and any other omitted change.
    if connection.total_changes() - operation.total_before != state.hooks - operation.hooks_before {
        state.clear();
        operation.cacheable = false;
    }
    // REPLACE can remove a conflicting hash without a delete hook, and may
    // either reuse its rowid or allocate another. Check only inserted rows.
    for rowid in std::mem::take(&mut state.inserted) {
        if let Some(id) = state.rowids.get(&rowid).copied() {
            state.remove(id);
        }
        if state.nodes.is_empty() {
            continue;
        }
        let hash: Option<String> = connection
            .prepare_cached("SELECT hash FROM main.blobs WHERE rowid = ?1")?
            .query_row([rowid], |row| row.get(0))
            .optional()?;
        if let Some(id) = hash.as_deref().and_then(NodeId::from_hex) {
            state.remove(id);
        }
    }
    Ok(())
}

impl StoredCatalog {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn observe(self: &Arc<Self>, connection: &Connection) {
        let catalog = Arc::clone(self);
        connection.update_hook(Some(move |action, database: &str, table: &str, rowid| {
            let mut state = catalog.state();
            state.hooks += 1;
            if database == "main" && table == "blobs" {
                state.changes += 1;
                if matches!(action, Action::SQLITE_UPDATE | Action::SQLITE_DELETE) {
                    state.clear();
                } else if matches!(action, Action::SQLITE_INSERT) && !state.nodes.is_empty() {
                    state.inserted.push(rowid);
                }
            }
        }));
    }

    pub(crate) fn run<T>(
        self: &Arc<Self>,
        connection: &mut Connection,
        call: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let before = version(connection)?;
        let (changes, hooks_before) = {
            let state = self.state();
            (state.changes, state.hooks)
        };
        let total_before = connection.total_changes();
        OPERATION.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "a connection operation is synchronous");
            *slot = Some(Operation {
                catalog: Arc::clone(self),
                checked_version: false,
                pending: BTreeMap::new(),
                total_before,
                hooks_before,
                cacheable: true,
            });
        });
        let _guard = OperationGuard;
        let result = call(connection);
        let mut operation = OPERATION.with_borrow_mut(Option::take);
        let after = version(connection)?;
        let mut state = self.state();
        if let Some(operation) = operation.as_mut() {
            refresh(connection, &mut state, operation)?;
        }
        if before != after {
            state.clear();
            state.version = None;
        } else if result.is_ok()
            && changes == state.changes
            && let Some(operation) = operation
            && operation.cacheable
        {
            for (id, member) in operation.pending {
                state.rowids.insert(member.rowid, id);
                state.nodes.insert(id, member);
            }
        }
        result
    }
}

/// A node already read and verified on this connection. The version check
/// happens after the caller establishes its read snapshot, before reuse.
pub(crate) fn get(connection: &Connection, id: NodeId) -> rusqlite::Result<Option<EncodedNode>> {
    OPERATION.with_borrow_mut(|slot| {
        let Some(operation) = slot else {
            return Ok(None);
        };
        let catalog = Arc::clone(&operation.catalog);
        let mut state = catalog.state();
        if !operation.checked_version {
            let current = version(connection)?;
            if state.version != Some(current) {
                state.clear();
                state.version = Some(current);
            }
            operation.checked_version = true;
        }
        refresh(connection, &mut state, operation)?;
        Ok(state.nodes.get(&id).map(|member| member.node.clone()))
    })
}

/// Records only exact build-catalog bytes read from the database; an insert
/// in this operation prevents publication, even if its transaction rolls back.
pub(crate) fn remember(connection: &Connection, id: NodeId, bytes: &[u8]) -> rusqlite::Result<()> {
    let Some(node) = lash_core_store::checkpoint_graph::node::immutable_catalog_node(id, bytes)
    else {
        return Ok(());
    };
    OPERATION.with_borrow_mut(|slot| {
        if let Some(operation) = slot {
            if operation.catalog.state().nodes.contains_key(&id)
                || operation.pending.contains_key(&id)
            {
                return Ok(());
            }
            let rowid: Option<i64> = connection
                .prepare_cached("SELECT rowid FROM main.blobs WHERE hash = ?1")?
                .query_row([id.to_hex()], |row| row.get(0))
                .optional()?;
            if let Some(rowid) = rowid {
                operation.pending.insert(id, StoredMember { node, rowid });
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TypedBody,
    };

    /// FIG-5894: a stored immutable release is reused across reads, but a
    /// local mutation, a rolled-back insertion, or another connection's
    /// mutation cannot leave it certified as stored.
    #[test]
    fn stored_catalog_reuse_never_hides_mutations_or_rolled_back_inserts() {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let id = staging
            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&[0]))
            .unwrap();
        let node = staging.nodes()[&id].clone();
        lash_core_store::checkpoint_graph::node::register_immutable_catalog(staging.nodes())
            .unwrap();
        let uri = format!(
            "file:catalog-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let mut connection = Connection::open(&uri).unwrap();
        let other = Connection::open(&uri).unwrap();
        connection
            .execute_batch("CREATE TABLE blobs(hash TEXT PRIMARY KEY, content BLOB)")
            .unwrap();
        let catalog = Arc::new(StoredCatalog::default());
        catalog.observe(&connection);
        let hash = id.to_hex();
        connection
            .execute(
                "INSERT INTO blobs VALUES (?1, ?2)",
                rusqlite::params![hash, &*node.bytes],
            )
            .unwrap();
        let read = |connection: &Connection| {
            assert!(get(connection, id).unwrap().is_none());
            let bytes: Vec<u8> = connection
                .query_row(
                    "SELECT content FROM blobs WHERE hash = ?1",
                    [&hash],
                    |row| row.get(0),
                )
                .unwrap();
            remember(connection, id, &bytes)?;
            Ok(())
        };
        catalog.run(&mut connection, |c| read(c)).unwrap();
        catalog
            .run(&mut connection, |c| {
                assert!(Arc::ptr_eq(&get(c, id)?.unwrap().bytes, &node.bytes));
                c.execute_batch("BEGIN; UPDATE blobs SET content = x'ff'; ROLLBACK;")?;
                assert!(
                    get(c, id)?.is_none(),
                    "even a rolled-back update invalidates"
                );
                Ok(())
            })
            .unwrap();
        catalog.run(&mut connection, |c| read(c)).unwrap();
        other.execute("DELETE FROM blobs", []).unwrap();
        catalog
            .run(&mut connection, |c| {
                assert!(get(c, id)?.is_none(), "another connection deleted the node");
                c.execute_batch("BEGIN")?;
                c.execute(
                    "INSERT INTO blobs VALUES (?1, ?2)",
                    rusqlite::params![hash, &*node.bytes],
                )?;
                read(c)?;
                c.execute_batch("ROLLBACK")?;
                Ok(())
            })
            .unwrap();
        catalog
            .run(&mut connection, |c| {
                assert!(
                    get(c, id)?.is_none(),
                    "a rolled-back insertion is not stored"
                );
                Ok(())
            })
            .unwrap();

        connection
            .execute(
                "INSERT INTO blobs VALUES (?1, ?2)",
                rusqlite::params![hash, &*node.bytes],
            )
            .unwrap();
        catalog.run(&mut connection, |c| read(c)).unwrap();
        catalog
            .run(&mut connection, |c| {
                assert!(get(c, id)?.is_some());
                c.execute_batch("DELETE FROM blobs")?;
                assert!(get(c, id)?.is_none(), "a truncate delete invalidates");
                Ok(())
            })
            .unwrap();
        connection
            .execute(
                "INSERT INTO blobs VALUES (?1, ?2)",
                rusqlite::params![hash, &*node.bytes],
            )
            .unwrap();
        catalog.run(&mut connection, |c| read(c)).unwrap();
        catalog
            .run(&mut connection, |c| {
                assert!(get(c, id)?.is_some());
                c.execute("INSERT OR REPLACE INTO blobs VALUES (?1, x'ff')", [&hash])?;
                assert!(
                    get(c, id)?.is_none(),
                    "a replacement invalidates the old bytes"
                );
                Ok(())
            })
            .unwrap();
    }
}
