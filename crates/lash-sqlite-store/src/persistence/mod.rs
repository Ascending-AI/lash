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
use lash_core::SelectedQueuedWorkClaimOutcome;
use lash_core::store::queued_work::{TurnWorkClaimPrefix, TurnWorkEmptyScanDiagnostic};
use lash_sansio::TurnId;

pub(crate) const LOAD_TURN_FAILURE_SETTLEMENTS_SQL: &str = "SELECT turn_id, result_json
     FROM runtime_turn_commits
     WHERE session_id = ?1
       AND result_json LIKE '%\"failure_evidence\"%'
     ORDER BY committed_at_ms, turn_id";

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
    session_id: &str,
) -> Result<TurnFailureSettlementLoad, StoreError> {
    let mut statement = conn
        .prepare(LOAD_TURN_FAILURE_SETTLEMENTS_SQL)
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![session_id], |row| {
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
    session_id: &str,
) -> Result<u32, StoreError> {
    let marker = conn
        .query_row(
            "SELECT session_state_version FROM session_meta WHERE session_id = ?1",
            params![session_id],
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
    session_id: &str,
) -> Result<(), StoreError> {
    let deleted = conn
        .query_row(
            "SELECT 1 FROM deleted_sessions WHERE session_id = ?1",
            params![session_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(sqlite_error)?
        .is_some();
    if deleted {
        Err(StoreError::SessionDeleted {
            session_id: session_id.to_string(),
        })
    } else {
        Ok(())
    }
}

/// The assignment half of turn-input settlement, shared by both settlement
/// regimes so only the predicate differs (ADR 0069 §5). `?3` is the settled
/// lifecycle state.
const PENDING_TURN_INPUT_COLUMNS: &str = "enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json, enqueued_at_ms, claim_id, claim_fencing_token, claim_owner_id, claim_owner_incarnation_id, claim_token, claim_session_lease_generation";

const TURN_INPUT_SETTLEMENT_ASSIGNMENTS: &str = "state = ?3,
                                     claim_id = NULL,
                                     claim_owner_id = NULL,
                                     claim_owner_incarnation_id = NULL,
                                     claim_token = NULL,
                                     claim_session_lease_generation = 0";

/// The lifecycle states an unclaimed settlement can never overwrite.
///
/// A cancelled or already-settled row is terminal: an unclaimed settlement that
/// finds one lost the head CAS.
const UNCLAIMED_TURN_INPUT_TERMINAL_STATES: [lash_core::TurnInputState; 2] = [
    lash_core::TurnInputState::Completed,
    lash_core::TurnInputState::Cancelled,
];

/// Whether an unclaimed row is still open for settlement.
fn unclaimed_turn_input_is_settleable(state: &str) -> bool {
    !UNCLAIMED_TURN_INPUT_TERMINAL_STATES
        .iter()
        .any(|terminal| terminal.as_str() == state)
}

/// The same terminal set spelled as the body of a SQL `IN (...)` list, so the
/// unclaimed settlement predicate and its Rust twin above cannot drift from
/// the enum.
fn unclaimed_turn_input_terminal_states_sql() -> String {
    UNCLAIMED_TURN_INPUT_TERMINAL_STATES
        .iter()
        .map(|state| format!("'{}'", state.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
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
                "SELECT g.parent_node_id
                 FROM graph_nodes AS g
                 WHERE g.node_id = ?1 AND g.tombstoned = 0
                   AND NOT EXISTS (
                       SELECT 1 FROM graph_nodes AS child
                       WHERE child.parent_node_id = g.node_id
                         AND child.tombstoned = 0
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM session_head AS head
                       WHERE head.leaf_node_id = g.node_id
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM node_anchors AS anchor
                       WHERE anchor.node_id = g.node_id
                   )",
                params![node_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        conn.execute(
            "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
            params![node_id],
        )
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
        "SELECT frame_node_id FROM graph_nodes
         WHERE node_id = ?1 AND tombstoned = 0",
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
