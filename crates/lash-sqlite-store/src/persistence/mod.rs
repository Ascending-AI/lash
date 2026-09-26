//! The [`RuntimePersistence`] capability-segment implementations for
//! [`Store`]: [`SessionCommitStore`], [`SessionExecutionLeaseStore`],
//! [`QueuedWorkStore`], [`TurnInputStore`], and [`StoreMaintenance`].
//!
//! This is the tokio-rusqlite port of the prior store's `persistence.rs`. The
//! public surface is byte-for-byte the prior store async trait: identical method
//! names and signatures, so consumers swap backends with a path rename only.
//!
//! The translation rules (see `conn.rs`, `lifecycle.rs`, `blobs.rs`):
//!
//! * Pure reads run through `self.conn.call(move |conn| { ... })`.
//! * Read-then-write paths run through `self.conn.write(move |tx| { ... })`
//!   (`BEGIN IMMEDIATE`, commit on `Ok`, rollback on `Err`) — this is the
//!   cross-process write-lock guard.
//! * Paths that may abandon partially-applied writes (the queued-work claim)
//!   run through `self.conn.write_flow`, deciding commit vs rollback via
//!   [`TxOutcome`].
//! * The shared `*_conn` helpers (`try_load_session_head_meta_from_conn`,
//!   `Self::put_checkpoint_conn`, `Self::load_usage_deltas_conn`,
//!   `Self::load_session_graph_from_conn`, the queued-work helpers, …) are
//!   synchronous and take a `&rusqlite::Connection`, so they are reused from
//!   inside these closures (a `&Transaction` derefs to `&Connection`).
//! * Closures must be `'static` + `Send`: every borrow of `self`/caller data is
//!   cloned into an owned value before being moved in.

use super::*;
use crate::session_sql::session_sql;
use lash_core_execution::SelectedQueuedWorkClaimOutcome;
use lash_core_execution::store::queued_work::{TurnWorkClaimPrefix, TurnWorkEmptyScanDiagnostic};
use lash_sansio::SessionId;
use lash_sansio::TurnId;

fn load_turn_failure_settlements_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
) -> Result<Vec<lash_core_execution::TurnFailureSettlement>, StoreError> {
    let mut statement = conn
        .prepare(session_sql().turn_commits.select_failure_settlements.sql())
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![session_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?;
    let mut settlements = Vec::new();
    for row in rows {
        let (turn_id, result_json) = row.map_err(sqlite_error)?;
        let receipt = lash_core_execution::store::decode_runtime_commit_receipt(
            session_id,
            &turn_id,
            &result_json,
        )?;
        if !receipt.failure_evidence.is_empty() {
            settlements.push(lash_core_execution::TurnFailureSettlement {
                turn_id,
                evidence: receipt.failure_evidence,
            });
        }
    }
    Ok(settlements)
}

fn read_session_state_version_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
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
    lash_core_execution::store::resolve_session_state_version(marker)
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
            session_id: SessionId::from(session_id.to_string()),
        })
    } else {
        Ok(())
    }
}

/// The claim-candidate scan for `boundary`, rendered once at startup.
///
/// The boundary is a closed two-variant choice, so it selects a named statement
/// rather than splicing a predicate: an optional boundary filter — a
/// `COALESCE(?N, …)` or a `?N IS NULL OR …` — cannot use
/// `idx_queued_work_batches_ready`, and this query is the claim path's hottest.
fn sqlite_queued_work_claim_candidates_sql(boundary: QueuedWorkClaimBoundary) -> &'static str {
    let sql = crate::turn_ingress::turn_ingress_sql();
    match boundary {
        QueuedWorkClaimBoundary::Idle => sql.queued_batches_sqlite.claim_candidates_idle.sql(),
        QueuedWorkClaimBoundary::ActiveTurnCheckpoint => {
            sql.queued_batches_sqlite.claim_candidates_boundary.sql()
        }
    }
}

/// Reclaim the ancestry prefix with no live child, session-head root, or
/// explicit anchor. Reachability is derived at each destructive decision.
pub(crate) fn retire_unreachable_ancestry_conn(
    conn: &Connection,
    first_node_id: &str,
) -> Result<(), StoreError> {
    let mut node_id = first_node_id.to_string();
    loop {
        let parent_node_id = conn
            .query_row(
                session_sql().graph_sqlite.select_retirable_parent.sql(),
                params![node_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        conn.execute(session_sql().graph_sqlite.retire.sql(), params![node_id])
            .map_err(sqlite_error)?;
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        node_id = parent_node_id;
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

mod claim_support;
mod maintenance;
mod queued_run;
mod queued_run_assignment;
mod queued_run_selection;
mod queued_work;
use queued_run::*;
use queued_run_selection::*;
mod session_commit;
mod session_execution_lease;
mod session_ingress;
mod turn_input;
pub(crate) mod turn_park_feed;

use claim_support::*;
#[cfg(any(test, feature = "testing"))]
pub(crate) use session_ingress::{
    apply_session_ingress_settlement_conn, session_ingress_rows_conn,
};
