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
use lash_core::SelectedQueuedWorkClaimOutcome;
use lash_core::store::queued_work::{TurnWorkClaimPrefix, TurnWorkEmptyScanDiagnostic};
use lash_sansio::SessionId;
use lash_sansio::TurnId;

struct CorruptTurnFailureReceipt {
    /// Operation storage key of the corrupt receipt, not a turn identity.
    turn_id: String,
    error: String,
}

struct TurnFailureSettlementLoad {
    settlements: Vec<lash_core::TurnFailureSettlement>,
    corrupt_receipts: Vec<CorruptTurnFailureReceipt>,
}

struct SessionLoadWithWarnings {
    read: PersistedSessionRead,
    corrupt_failure_receipts: Vec<CorruptTurnFailureReceipt>,
}

fn load_turn_failure_settlements_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
) -> Result<TurnFailureSettlementLoad, StoreError> {
    let mut statement = conn
        .prepare(session_sql().turn_commits.select_failure_settlements.sql())
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![session_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?;
    let mut settlements = Vec::new();
    let mut corrupt_rows = Vec::new();
    for row in rows {
        let (turn_id, result_json) = row.map_err(sqlite_error)?;
        let receipt: lash_core::store::RuntimeCommitReceipt =
            match serde_json::from_str(&result_json) {
                Ok(receipt) => receipt,
                Err(error) => {
                    corrupt_rows.push(CorruptTurnFailureReceipt {
                        turn_id,
                        error: error.to_string(),
                    });
                    continue;
                }
            };
        if !receipt.failure_evidence.is_empty() {
            settlements.push(lash_core::TurnFailureSettlement {
                turn_id,
                evidence: receipt.failure_evidence,
            });
        }
    }
    Ok(TurnFailureSettlementLoad {
        settlements,
        corrupt_receipts: corrupt_rows,
    })
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
        return Ok(lash_core::store::CURRENT_SESSION_STATE_VERSION);
    };
    let marker = marker
        .map(|version| {
            u32::try_from(version).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "SessionStateVersion",
                message: format!("marker {version} is outside the unsigned 32-bit domain"),
            })
        })
        .transpose()?;
    lash_core::store::resolve_session_state_version(marker)
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

const PENDING_TURN_INPUT_COLUMNS: &str = "enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json, enqueued_at_ms, claim_id, claim_fencing_token, claim_owner_id, claim_owner_incarnation_id, claim_token, claim_session_lease_generation";

/// The claim-release assignment tail: settling an input clears the whole
/// four-column identity family in one motion;
/// `ck_pending_turn_inputs_claim_identity_all_or_none` makes that all-or-none
/// shape load-bearing, so every release path shares this spelling. `?3` is the
/// settled lifecycle state the caller assigns alongside it (ADR 0069 §5).
const TURN_INPUT_CLAIM_RELEASE_ASSIGNMENTS: &str = "claim_id = NULL,
                                     claim_owner_id = NULL,
                                     claim_owner_incarnation_id = NULL,
                                     claim_token = NULL,
                                     claim_session_lease_generation = 0";

/// The terminal state set spelled as the body of a SQL `IN (...)` list, so the
/// unclaimed settlement predicate and the shared verdict's
/// [`unclaimed_turn_input_is_settleable`](lash_core::store_backend_support::unclaimed_turn_input_is_settleable)
/// cannot drift from the enum.
fn unclaimed_turn_input_terminal_states_sql() -> String {
    lash_core::store_backend_support::terminal_turn_input_states_sql()
}

const SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE: &str = "session_id = ?1
       AND available_at_ms <= ?2
       AND (
            claim_token IS NULL
            OR claim_session_lease_generation <> ?3
       )";

fn sqlite_queued_work_head_candidate_cte(boundary: QueuedWorkClaimBoundary) -> String {
    if boundary == QueuedWorkClaimBoundary::Idle {
        return format!(
            "queued_work_head_candidate AS (
            SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
            FROM (
                SELECT enqueue_seq AS head_enqueue_seq,
                       batch_id AS head_batch_id,
                       delivery_policy AS head_delivery_policy,
                       claim_id AS head_claim_id
                FROM queued_work_batches
                WHERE {SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
                ORDER BY enqueue_seq ASC
                LIMIT 1
            ) AS unfiltered_head
         )"
        );
    }
    let earliest_safe_boundary = DeliveryPolicy::EarliestSafeBoundary.as_str();
    format!(
        "queued_work_unfiltered_head AS (
            SELECT enqueue_seq AS head_enqueue_seq,
                   batch_id AS head_batch_id,
                   delivery_policy AS head_delivery_policy,
                   claim_id AS head_claim_id
            FROM queued_work_batches
            WHERE {SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
            ORDER BY enqueue_seq ASC
            LIMIT 1
         ),
         queued_work_head_candidate AS (
            SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
            FROM (
                SELECT candidate.enqueue_seq AS head_enqueue_seq,
                       candidate.batch_id AS head_batch_id,
                       candidate.delivery_policy AS head_delivery_policy,
                       candidate.claim_id AS head_claim_id
                FROM queued_work_batches AS candidate
                CROSS JOIN queued_work_unfiltered_head AS unfiltered
                WHERE candidate.session_id = ?1
                  AND candidate.available_at_ms <= ?2
                  AND (
                       candidate.claim_token IS NULL
                       OR candidate.claim_session_lease_generation <> ?3
                  )
                  AND (
                       (
                            candidate.enqueue_seq = unfiltered.head_enqueue_seq
                            AND unfiltered.head_delivery_policy = '{earliest_safe_boundary}'
                       )
                       OR (
                            unfiltered.head_delivery_policy <> '{earliest_safe_boundary}'
                            AND unfiltered.head_claim_id IS NOT NULL
                            AND (
                                 candidate.claim_id IS NULL
                                 OR candidate.claim_id <> unfiltered.head_claim_id
                            )
                       )
                  )
                ORDER BY candidate.enqueue_seq ASC
                LIMIT 1
            ) AS boundary_head
            WHERE head_delivery_policy = '{earliest_safe_boundary}'
         )"
    )
}

fn sqlite_queued_work_claim_candidates_sql(boundary: QueuedWorkClaimBoundary) -> String {
    let head_candidate = sqlite_queued_work_head_candidate_cte(boundary);
    format!(
        "WITH {head_candidate}
         SELECT {QUEUED_WORK_COLUMNS}
         FROM queued_work_batches
         CROSS JOIN queued_work_head_candidate
         WHERE {SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
           AND enqueue_seq >= head_enqueue_seq
           AND (head_claim_id IS NULL OR queued_work_batches.claim_id = head_claim_id)
         ORDER BY enqueue_seq ASC
         LIMIT COALESCE((
             SELECT CASE WHEN head_claim_id IS NULL THEN ?4 ELSE 9223372036854775807 END
             FROM queued_work_head_candidate
         ), 0)",
        QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
    )
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
mod queued_work;
mod session_commit;
mod session_execution_lease;
mod turn_input;

use claim_support::*;
