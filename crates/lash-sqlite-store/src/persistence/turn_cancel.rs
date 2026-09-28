//! Turn-cancel and follow-on helpers the commit and ingress paths share,
//! each running inside the caller's write transaction.

use super::*;

/// Withdraw one row for the host (FIG-3927): an open row is cancelled; a row
/// a root admitted is that root's to settle or release, so the cancel
/// changes nothing and answers the root that holds it.
pub(super) fn cancel_pending_turn_input_row_conn(
    conn: &Connection,
    row: PendingTurnInputRow,
) -> Result<lash_core_execution::PendingTurnInputCancelOutcome, StoreError> {
    let admitted_root = row.admitted_root.clone();
    let mut input = pending_turn_input_from_row(row)?;
    match input.state.kind() {
        lash_core_execution::runtime::TurnInputStateKind::Cancelled => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCancelled(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::Completed => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCompleted(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::PendingActive
        | lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn
        | lash_core_execution::runtime::TurnInputStateKind::Accepted => {
            if let Some(root) = admitted_root {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::AlreadyAdmitted {
                        input,
                        root: TurnId::from(root),
                    },
                );
            }
            let cancelled = crate::conn::cached_execute(
                conn,
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs
                    .cancel
                    .sql(),
                params![
                    input.session_id.as_str(),
                    input.input_id.as_str(),
                    lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
                ],
            )
            .map_err(sqlite_error)?;
            if cancelled != 1 {
                return Err(StoreError::Backend(format!(
                    "open turn input `{}` was not withdrawn under the write lock",
                    input.input_id
                )));
            }
            input.state = lash_core_execution::TurnInputState::Cancelled(input.state.ingress());
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}

/// The follow-on the head of `session_id` owes (ADR 0101 §3), read inside
/// the caller's transaction.
pub(super) fn pending_follow_on_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<lash_core_execution::store::PendingFollowOn>, StoreError> {
    let json = conn
        .query_row(
            crate::session_sql::session_sql()
                .head
                .select_pending_follow_on
                .sql(),
            params![session_id.as_str()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(sqlite_error)?
        .flatten();
    lash_core_execution::store::pending_follow_on::decode_pending_follow_on(
        session_id,
        json.as_deref(),
    )
}

/// Raise the owed follow-on's recovery count under the drive `fence` (ADR
/// 0101 §3), inside the caller's write transaction. The head revision does
/// not move.
pub(super) fn raise_pending_follow_on_conn(
    conn: &Connection,
    fence: &lash_core_execution::store::DriveFence,
    follow_on_turn_id: &lash_core_execution::TurnId,
) -> Result<lash_core_execution::store::PendingFollowOn, StoreError> {
    require_drive_fence_conn(conn, fence)?;
    let session_id = fence.session();
    let not_pending = || StoreError::FollowOnNotPending {
        session_id: session_id.clone(),
        follow_on_turn_id: follow_on_turn_id.clone(),
    };
    let pending = pending_follow_on_conn(conn, session_id)?
        .filter(|pending| pending.is_turn(follow_on_turn_id))
        .ok_or_else(not_pending)?;
    let raised = pending.raised()?;
    let updated =
        crate::conn::cached_execute(
            conn,
            crate::session_sql::session_sql()
                .head
                .raise_pending_follow_on
                .sql(),
            params![
                session_id.as_str(),
                lash_core_execution::store::pending_follow_on::encode_pending_follow_on(Some(
                    &raised
                ),)?,
                follow_on_turn_id.as_str(),
            ],
        )
        .map_err(sqlite_error)?;
    if updated != 1 {
        return Err(not_pending());
    }
    Ok(raised)
}

/// Refuse `fence` unless it is the session's current drive fence, read in the
/// caller's transaction.
pub(super) fn require_drive_fence_conn(
    conn: &Connection,
    fence: &lash_core_execution::store::DriveFence,
) -> Result<(), StoreError> {
    super::drive_epoch::require_fence_conn(conn, fence.session(), fence)
}

pub(crate) fn decode_stored_json<T: serde::de::DeserializeOwned>(
    json: &str,
    label: &str,
) -> Result<T, StoreError> {
    serde_json::from_str(json)
        .map_err(|err| StoreError::Backend(format!("failed to decode {label}: {err}")))
}

pub(crate) fn load_turn_cancel_request_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
    let json = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests_sqlite
                .select_record
                .sql(),
            params![session_id.as_str(), turn_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    json.map(|json| decode_stored_json(&json, "turn cancel request"))
        .transpose()
}

pub(super) fn load_turn_cancel_intent_snapshot_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests_sqlite
                .select_record_with_revision
                .sql(),
            params![session_id.as_str(), turn_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((json, revision)) = row else {
        return Ok(lash_core_execution::TurnCancelIntentSnapshot::Absent);
    };
    let record: lash_core_execution::TurnCancelRequestRecord =
        decode_stored_json(&json, "turn cancel request")?;
    let revision = u64::try_from(revision)
        .map_err(|_| StoreError::Backend("turn cancel intent revision is negative".to_string()))?;
    if revision == 0 {
        return Err(StoreError::Backend(
            "turn cancel intent revision is zero".to_string(),
        ));
    }
    Ok(lash_core_execution::TurnCancelIntentSnapshot::Present {
        request: record.request,
        revision,
    })
}

pub(crate) fn append_turn_cancel_outcome_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedInput,
) -> Result<(), StoreError> {
    let Some(mut record) = load_turn_cancel_request_conn(conn, session_id, turn_id)? else {
        return Ok(());
    };
    record
        .outcome
        .get_or_insert_default()
        .affected_inputs
        .push(affected);
    crate::conn::cached_execute(
        conn,
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_sqlite
            .update_record
            .sql(),
        params![session_id.as_str(), turn_id.as_str(), encode_json(&record)?],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Record one wake a turn cancel deferred on the cancellation (FIG-3543).
pub(super) fn append_turn_cancel_wake_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedWake,
) -> Result<(), StoreError> {
    let Some(mut record) = load_turn_cancel_request_conn(conn, session_id, turn_id)? else {
        return Ok(());
    };
    record
        .outcome
        .get_or_insert_default()
        .affected_wakes
        .push(affected);
    crate::conn::cached_execute(
        conn,
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_sqlite
            .update_record
            .sql(),
        params![session_id.as_str(), turn_id.as_str(), encode_json(&record)?],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub(super) fn reconcile_turn_cancel_winner_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    observed: &lash_core_execution::TurnCancelIntentSnapshot,
    evidence: &lash_core_execution::facade_support::TurnCancellationEvidence,
) -> Result<bool, StoreError> {
    let actual = load_turn_cancel_intent_snapshot_conn(conn, session_id, turn_id)?;
    if actual != *observed {
        return Ok(false);
    }
    let mut record = load_turn_cancel_request_conn(conn, session_id, turn_id)?.unwrap_or(
        lash_core_execution::TurnCancelRequestRecord {
            request: lash_core_execution::facade_support::TurnCancelRequest {
                address: lash_core_execution::facade_support::TurnAddress::new(session_id, turn_id),
                request_id: evidence.request_id.clone(),
                origin: evidence.origin.clone(),
                reason: evidence.reason.clone(),
                undelivered: evidence.undelivered,
                mode: evidence.mode,
            },
            outcome: None,
        },
    );
    let request = lash_core_execution::facade_support::TurnCancelRequest {
        address: lash_core_execution::facade_support::TurnAddress::new(session_id, turn_id),
        request_id: evidence.request_id.clone(),
        origin: evidence.origin.clone(),
        reason: evidence.reason.clone(),
        undelivered: evidence.undelivered,
        mode: evidence.mode,
    };
    let revision = match actual {
        lash_core_execution::TurnCancelIntentSnapshot::Absent => 1,
        lash_core_execution::TurnCancelIntentSnapshot::Present {
            request: ref prior,
            revision,
        } if prior == &request => revision,
        lash_core_execution::TurnCancelIntentSnapshot::Present { revision, .. } => {
            StoreError::checked_monotonic_increment("turn_cancel_intent_revision", revision)?
        }
    };
    record.request = request;
    let revision = i64::try_from(revision).map_err(|_| {
        StoreError::Backend("turn cancel intent revision exceeds SQLite range".to_string())
    })?;
    crate::conn::cached_execute(
        conn,
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_sqlite
            .upsert_record
            .sql(),
        params![
            session_id.as_str(),
            turn_id.as_str(),
            encode_json(&record)?,
            revision
        ],
    )
    .map_err(sqlite_error)?;
    Ok(true)
}

pub(super) fn requested_append_ancestor(
    stamp: &lash_core_execution::RuntimeTurnCommitStamp,
) -> Option<&str> {
    match &stamp.append_request_identity {
        lash_core_execution::AppendRequestIdentity::Append {
            requested_ancestor_node_id,
            ..
        } => requested_ancestor_node_id.as_deref(),
        lash_core_execution::AppendRequestIdentity::PlainCommit
        | lash_core_execution::AppendRequestIdentity::SemanticBoundary { .. } => None,
    }
}

pub(super) fn append_identity_columns(
    identity: &lash_core_execution::AppendRequestIdentity,
) -> (Option<&str>, Option<i64>, Option<i64>) {
    match identity {
        lash_core_execution::AppendRequestIdentity::PlainCommit => (None, None, None),
        lash_core_execution::AppendRequestIdentity::Append {
            encoding_version,
            request_hash,
            requested_node_count,
            ..
        } => (
            Some(request_hash.as_str()),
            Some(*requested_node_count as i64),
            Some(i64::from(*encoding_version)),
        ),
        // A semantic-boundary identity persists without a node count; the
        // NULL count is what distinguishes its family on decode (FIG-2480).
        lash_core_execution::AppendRequestIdentity::SemanticBoundary {
            operation: _,
            encoding_version,
            request_hash,
        } => (
            Some(request_hash.as_str()),
            None,
            Some(i64::from(*encoding_version)),
        ),
    }
}
