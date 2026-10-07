//! Capability-segment implementations for [`SqliteStore`]:
//! [`SessionCommitStore`], [`lash_core_execution::TurnInputStore`],
//! [`lash_core_execution::QueuedWorkStore`], and [`StoreMaintenance`].
//!
//! Every operation scopes its session explicitly or through its request.
//!
//! The translation rules (see `conn.rs`, `lifecycle.rs`, `blobs.rs`):
//!
//! * Pure reads run through `self.conn.call(move |conn| { ... })`.
//! * Read-then-write paths run through `self.conn.write(move |tx| { ... })`
//!   (`BEGIN IMMEDIATE`, commit on `Ok`, rollback on `Err`) — this is the
//!   cross-process write-lock guard.
//! * Paths that may abandon partially-applied writes (the admissions) run
//!   through `self.conn.write_flow`, deciding commit vs rollback via
//!   [`TxOutcome`].
//! * The shared `*_conn` helpers (`try_load_session_head_meta_from_conn`,
//!   `Self::put_checkpoint_conn`, and the queued-work helpers) are
//!   synchronous and take a `&rusqlite::Connection`, so they are reused from
//!   inside these closures (a `&Transaction` derefs to `&Connection`).
//! * Closures must be `'static` + `Send`: every borrow of `self`/caller data is
//!   cloned into an owned value before being moved in.

use super::*;
use crate::session_sql::session_sql;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

fn read_session_state_version_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    fleet: lash_core_execution::FleetFormat,
) -> Result<u32, StoreError> {
    let marker = conn
        .query_row(
            session_sql().meta.select_state_version.sql(),
            params![session_id.as_str()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some(marker) = marker else {
        return Ok(lash_core_execution::store::CURRENT_SESSION_STATE_VERSION);
    };
    let marker = marker
        .map(|version| {
            u32::try_from(version).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "SessionStateVersion",
                message: format!("marker {version} is outside the unsigned 32-bit domain"),
            })
        })
        .transpose()?;
    lash_core_execution::store::resolve_session_state_version(marker, fleet)
}

/// Refuse a write into session `session_id` once its close began: the
/// `CloseSession` intent is the point of no return of its deletion, so it
/// accepts nothing more (FIG-3600 S7).
pub(crate) fn ensure_session_not_closing_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let closing: Option<i64> = conn
        .query_row(
            session_sql().meta.select_closing_intent.sql(),
            params![session_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?
        .flatten();
    match closing {
        None => Ok(()),
        Some(intent) => Err(StoreError::SessionClosing {
            session_id: session_id.clone(),
            intent: lash_core_execution::store::ControlIntentId::from_sequence(
                u64::try_from(intent).map_err(|_| {
                    stored_data_corrupt("SessionMeta", "closing_intent must be non-negative")
                })?,
            ),
        }),
    }
}

pub(crate) fn ensure_session_not_deleted_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let deleted = conn
        .query_row(
            session_sql().deleted_sqlite.exists.sql(),
            params![session_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sqlite_error)?
        .is_some();
    if deleted {
        Err(StoreError::SessionDeleted {
            session_id: session_id.clone(),
        })
    } else {
        Ok(())
    }
}

/// One ancestry node whose complete indexed run check found no live child,
/// session head, or anchor, under the severing transaction's writer lock.
struct RetirableAncestryNode {
    node_id: String,
    parent_node_id: Option<String>,
}

fn retirable_ancestry_node_conn(
    conn: &Connection,
    node_id: &str,
) -> Result<Option<RetirableAncestryNode>, StoreError> {
    let parent = conn
        .query_row(
            session_sql().graph_sqlite.select_retirable_parent.sql(),
            params![node_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    Ok(parent.map(|parent_node_id| RetirableAncestryNode {
        node_id: node_id.to_owned(),
        parent_node_id,
    }))
}

fn retire_ancestry_node_conn(
    conn: &Connection,
    witness: RetirableAncestryNode,
) -> Result<Option<String>, StoreError> {
    crate::conn::cached_execute(
        conn,
        session_sql().graph_sqlite.retire.sql(),
        params![witness.node_id],
    )
    .map_err(sqlite_error)?;
    Ok(witness.parent_node_id)
}

/// Reclaim the ancestry prefix with no live child, session-head root, or
/// explicit anchor. Every destructive step consumes its own complete check.
pub(crate) fn retire_unreachable_ancestry_conn(
    conn: &Connection,
    first_node_id: &str,
) -> Result<(), StoreError> {
    let mut node_id = first_node_id.to_string();
    loop {
        let Some(witness) = retirable_ancestry_node_conn(conn, &node_id)? else {
            return Ok(());
        };
        let Some(parent) = retire_ancestry_node_conn(conn, witness)? else {
            return Ok(());
        };
        node_id = parent;
    }
}

pub(crate) fn nearest_frame_node_id_conn(
    conn: &Connection,
    leaf_node_id: &str,
) -> Result<Option<String>, StoreError> {
    conn.query_row(
        session_sql().graph_sqlite.select_frame_node_id.sql(),
        params![leaf_node_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(sqlite_error)
}

mod admission;
pub(crate) use admission::{admit_at_checkpoint_sqlite, open_session_command_run_sqlite};
mod ingress_settlement;
mod maintenance;
mod queued_work;
pub(crate) mod session_commit;
mod session_fault;
pub(crate) mod turn_cancel;
mod turn_input;

use turn_cancel::*;
