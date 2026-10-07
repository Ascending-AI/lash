//! Turn-cancel and follow-on helpers the commit and ingress paths share,
//! each running inside the caller's write transaction.

use super::*;
use lash_core_execution::store_backend_support::turn_cancel::*;

/// Withdraw one row for the host at `now` (FIG-3927): an open row is
/// cancelled in one write (FIG-4098); a row a run admitted is that run's to
/// settle or release, so the cancel changes nothing and answers the run that
/// holds it.
pub(super) fn cancel_pending_turn_input_row_conn(
    conn: &Connection,
    row: PendingTurnInputRow,
    now: u64,
) -> Result<lash_core_execution::PendingTurnInputCancelOutcome, StoreError> {
    let admitted_run = row.admitted_run.clone();
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
            if let Some(run) = admitted_run {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::AlreadyAdmitted {
                        input,
                        run: TurnId::parse(run)?,
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

pub(crate) fn decode_stored_json<T: serde::de::DeserializeOwned>(
    json: &str,
    record_kind: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|err| crate::stored_data_corrupt(record_kind, err))
}

/// The cancel request turn `turn_id` of session `session_id` accepted, if
/// any.
pub(crate) fn load_turn_cancel_request_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core_execution::facade_support::TurnCancelRequest>, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests
                .select_request
                .sql(),
            params![session_id.as_str(), turn_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((request_id, origin, reason, disposition, mode)) = row else {
        return Ok(None);
    };
    Ok(Some(
        lash_core_execution::facade_support::TurnCancelRequest {
            address: lash_core_execution::facade_support::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
            undelivered: turn_cancel_undelivered_from_wire(&disposition)?,
            mode: turn_cancel_mode_from_wire(&mode)?,
        },
    ))
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
