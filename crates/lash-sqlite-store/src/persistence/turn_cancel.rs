//! Turn-cancel and follow-on helpers the commit and ingress paths share,
//! each running inside the caller's write transaction.

use super::*;
use lash_core_execution::store_backend_support::turn_cancel::*;

/// Withdraw one row for the host at `now` (FIG-3927): an open row is
/// cancelled, its ingress obligation settled in the same write (FIG-4098); a
/// row a root admitted is that root's to settle or release, so the cancel
/// changes nothing and answers the root that holds it.
pub(super) fn cancel_pending_turn_input_row_conn(
    conn: &Connection,
    row: PendingTurnInputRow,
    now: u64,
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
                    crate::clamp_epoch_ms(now),
                ],
            )
            .map_err(sqlite_error)?;
            if cancelled != 1 {
                return Err(StoreError::Backend(format!(
                    "open turn input `{}` was not withdrawn under the write lock",
                    input.input_id
                )));
            }
            input.state = lash_core_execution::TurnInputState::Cancelled {
                ingress: input.state.ingress(),
                at_ms: now.min(i64::MAX as u64),
            };
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
                .head_sqlite
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
                .head_sqlite
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
    record_kind: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|err| crate::stored_data_corrupt(record_kind, err))
}

pub(crate) fn load_turn_cancel_request_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests
                .select_request
                .sql(),
            params![session_id.as_str(), turn_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_affected_inputs
                .select_by_turn
                .sql(),
        )
        .map_err(sqlite_error)?;
    let affected = stmt
        .query_map(params![session_id.as_str(), turn_id.as_str()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    turn_cancel_record_from_rows(session_id, turn_id, row, affected).map(Some)
}

pub(super) fn load_turn_cancel_intent_snapshot_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests
                .select_request_with_revision
                .sql(),
            params![session_id.as_str(), turn_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    turn_cancel_snapshot_from_row(session_id, turn_id, row)
}

struct CancelItem<'a> {
    item_id: &'a str,
    disposition: lash_core_execution::TurnCancelUndeliveredInputPolicy,
    payload: String,
    kind: &'static str,
    batch_id: Option<&'a str>,
}

fn append_cancel_item(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    item: CancelItem<'_>,
) -> Result<(), StoreError> {
    if load_turn_cancel_intent_snapshot_conn(conn, session_id, turn_id)?
        == lash_core_execution::TurnCancelIntentSnapshot::Absent
    {
        return Ok(());
    }
    crate::conn::cached_execute(
        conn,
        crate::turn_ingress::turn_ingress_sql()
            .cancel_affected_inputs
            .append_at_next_ordinal
            .sql(),
        params![
            session_id.as_str(),
            turn_id.as_str(),
            item.item_id,
            turn_cancel_undelivered_wire(item.disposition),
            item.payload,
            item.kind,
            item.batch_id
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub(crate) fn append_turn_cancel_outcome_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedInput,
) -> Result<(), StoreError> {
    append_cancel_item(
        conn,
        session_id,
        turn_id,
        CancelItem {
            item_id: affected.input_id.as_str(),
            disposition: affected.disposition,
            payload: encode_json(&affected.payload)?,
            kind: AFFECTED_INPUT_KIND,
            batch_id: None,
        },
    )
}

pub(super) fn append_turn_cancel_wake_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedWake,
) -> Result<(), StoreError> {
    append_cancel_item(
        conn,
        session_id,
        turn_id,
        CancelItem {
            item_id: affected.batch_id.as_str(),
            disposition: affected.disposition,
            payload: encode_json(&affected.wake)?,
            kind: AFFECTED_WAKE_KIND,
            batch_id: Some(affected.batch_id.as_str()),
        },
    )
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
    let revision = i64::try_from(revision).map_err(|_| StoreError::RecordEncodingFailed {
        record_kind: "TurnCancelRequest".into(),
        message: "intent revision exceeds SQLite INTEGER".into(),
    })?;
    crate::conn::cached_execute(
        conn,
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests
            .upsert_record
            .sql(),
        params![
            session_id.as_str(),
            turn_id.as_str(),
            request.request_id,
            request.origin,
            request.reason,
            turn_cancel_undelivered_wire(request.undelivered),
            turn_cancel_mode_wire(request.mode),
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

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn replayed_cancellation_wake_has_one_receipt() {
        let root = tempfile::tempdir().expect("receipt fixture directory");
        for conn in [
            Connection::open_in_memory().expect("open memory receipt fixture"),
            Connection::open(root.path().join("receipts.db")).expect("open file receipt fixture"),
        ] {
            assert_replayed_wake_has_one_receipt(conn);
        }
    }

    fn assert_replayed_wake_has_one_receipt(conn: Connection) {
        conn.execute_batch(crate::schema::SCHEMA)
            .expect("create schema");
        let session = SessionId::from("session");
        let turn = TurnId::from("turn");
        let request = lash_core_execution::facade_support::TurnCancelRequest::new(
            lash_core_execution::facade_support::TurnAddress::new(&session, &turn),
            "request",
            None,
        );
        conn.execute(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests_sqlite
                .insert_first
                .sql(),
            params![
                session.as_str(),
                turn.as_str(),
                request.request_id,
                request.origin,
                request.reason,
                turn_cancel_undelivered_wire(request.undelivered),
                turn_cancel_mode_wire(request.mode)
            ],
        )
        .expect("seed request");
        let process_id = lash_sansio::ProcessId::fixture("process");
        let wake = lash_core_execution::ProcessWakeDelivery {
            version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            wake_id: "wake".into(),
            target_session_id: session.clone(),
            process_id: process_id.clone(),
            sequence: 1,
            event_type: "process.wake".into(),
            event_invocation: lash_core_execution::RuntimeInvocation {
                attribution: lash_core_execution::RuntimeAttribution::for_session(&session),
                subject: lash_core_execution::runtime::RuntimeSubject::ProcessEvent {
                    process_id,
                    sequence: 1,
                    event_type: "process.wake".into(),
                },
                caused_by: None,
                replay: None,
            },
            process_caused_by: None,
            authority: Default::default(),
            input: "wake payload".into(),
            created_at_ms: 0,
        };
        let affected = lash_core_execution::TurnCancelAffectedWake::deferred("batch".into(), wake);
        append_turn_cancel_wake_conn(&conn, &session, &turn, affected.clone())
            .expect("append wake");
        append_turn_cancel_wake_conn(&conn, &session, &turn, affected.clone())
            .expect("replay wake");
        let loaded = load_turn_cancel_request_conn(&conn, &session, &turn)
            .expect("load receipt")
            .expect("request exists");
        assert_eq!(
            loaded.outcome.expect("affected wake exists").affected_wakes,
            vec![affected]
        );
    }
}
