//! Turn-cancel, follow-on and commit-identity helpers the commit and ingress
//! paths share, each running inside the caller's transaction.

use super::*;
use lash_core_execution::store_backend_support::turn_cancel::*;

/// Load one cancellation record. Affected-input payloads are receipt
/// snapshots on `lash_turn_cancel_affected_inputs`, so no cross-table
/// snapshot isolation is needed to keep the evidence whole.
pub(super) async fn load_turn_cancel_request_pg(
    pool: &sqlx::PgPool,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
    let mut connection = acquire_runtime_connection(pool).await?;
    let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
    let record = load_turn_cancel_request_in_tx(&mut tx, session_id, turn_id, false).await?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(record)
}

pub(super) async fn load_turn_cancel_intent_snapshot_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
    let row: Option<TurnCancelIntentRow> = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_postgres
            .select_request_with_revision_for_update
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    turn_cancel_snapshot_from_row(session_id, turn_id, row)
}

pub(super) async fn load_turn_cancel_intent_snapshot_pg(
    pool: &sqlx::PgPool,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
    let mut connection = acquire_runtime_connection(pool).await?;
    let row = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests
            .select_request_with_revision
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    turn_cancel_snapshot_from_row(session_id, turn_id, row)
}

async fn load_turn_cancel_request_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    lock_request: bool,
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
    let statements = &crate::turn_ingress::turn_ingress_sql().cancel_requests_postgres;
    let metadata_sql = if lock_request {
        statements.select_request_for_update.sql()
    } else {
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests
            .select_request
            .sql()
    };
    let row: Option<TurnCancelRequestRow> = sqlx::query_as(metadata_sql)
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let affected_rows: Vec<TurnCancelAffectedRow> = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_affected_inputs
            .select_by_turn
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    turn_cancel_record_from_rows(session_id, turn_id, row, affected_rows).map(Some)
}

pub(super) async fn load_turn_cancel_request_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
    load_turn_cancel_request_in_tx(tx, session_id, turn_id, true).await
}

pub(crate) async fn append_turn_cancel_outcome_conn(
    conn: &mut sqlx::PgConnection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedInput,
) -> Result<(), StoreError> {
    if !lock_turn_cancel_request_conn(conn, session_id, turn_id).await? {
        return Ok(());
    }
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_affected_inputs
            .append_at_next_ordinal
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(&*affected.input_id)
    .bind(turn_cancel_undelivered_wire(affected.disposition))
    .bind(encode_json(&affected.payload)?)
    .bind(AFFECTED_INPUT_KIND)
    .bind(None::<&str>)
    .execute(&mut *conn)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Record one wake a turn cancel deferred on the cancellation (FIG-3543).
pub(super) async fn append_turn_cancel_wake_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: &lash_core_execution::TurnCancelAffectedWake,
) -> Result<(), StoreError> {
    if !lock_turn_cancel_request_conn(tx, session_id, turn_id).await? {
        return Ok(());
    }
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_affected_inputs
            .append_at_next_ordinal
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(affected.batch_id.as_str())
    .bind(turn_cancel_undelivered_wire(affected.disposition))
    .bind(encode_json(&affected.wake)?)
    .bind(AFFECTED_WAKE_KIND)
    .bind(affected.batch_id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Lock the cancel-request row so concurrent appends serialize on the
/// ordinal next-val; `false` when there is no request to attach evidence to.
async fn lock_turn_cancel_request_conn(
    conn: &mut sqlx::PgConnection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<bool, StoreError> {
    let request_exists: Option<i32> = sqlx::query_scalar(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_postgres
            .lock_request
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(store_sqlx_error)?;
    Ok(request_exists.is_some())
}

pub(super) async fn reconcile_turn_cancel_winner_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    observed: &lash_core_execution::TurnCancelIntentSnapshot,
    evidence: &lash_core_execution::facade_support::TurnCancellationEvidence,
) -> Result<bool, StoreError> {
    let actual = load_turn_cancel_intent_snapshot_tx(tx, session_id, turn_id).await?;
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
    let revision = i64::try_from(revision).map_err(|_| {
        StoreError::Backend("turn cancel intent revision exceeds PostgreSQL BIGINT".to_string())
    })?;
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests
            .upsert_record
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(&evidence.request_id)
    .bind(&evidence.origin)
    .bind(&evidence.reason)
    .bind(turn_cancel_undelivered_wire(evidence.undelivered))
    .bind(turn_cancel_mode_wire(evidence.mode))
    .bind(revision)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(true)
}

/// The follow-on the head of `session_id` owes (ADR 0101 §3), read inside
/// the caller's transaction under a row lock: `FOR UPDATE` for a writer that
/// decides against it, `FOR SHARE` for an admission.
pub(super) async fn pending_follow_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    for_update: bool,
) -> Result<Option<lash_core_execution::store::PendingFollowOn>, StoreError> {
    let statement = if for_update {
        crate::session_sql::session_sql()
            .head_postgres
            .select_pending_follow_on_for_update
            .sql()
    } else {
        crate::session_sql::session_sql()
            .head_postgres
            .select_pending_follow_on_for_share
            .sql()
    };
    let json = sqlx::query_scalar::<_, Option<String>>(statement)
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .flatten();
    lash_core_execution::store::pending_follow_on::decode_pending_follow_on(
        session_id,
        json.as_deref(),
    )
}

/// Refuse `fence` unless it is the session's current drive fence, read in
/// the caller's transaction.
pub(super) async fn require_drive_fence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &lash_core_execution::store::DriveFence,
) -> Result<(), StoreError> {
    super::drive_epoch::require_fence_tx(tx, fence.session(), fence).await
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

pub(super) type AppendIdentityColumns<'a> = (Option<&'a str>, Option<i64>, Option<i32>);

pub(super) fn append_identity_columns(
    identity: &lash_core_execution::AppendRequestIdentity,
) -> Result<AppendIdentityColumns<'_>, StoreError> {
    // A semantic-boundary identity persists without a node count; the NULL
    // count is what distinguishes its family on decode (FIG-2480).
    let (encoding_version, request_hash, requested_node_count) = match identity {
        lash_core_execution::AppendRequestIdentity::PlainCommit => return Ok((None, None, None)),
        lash_core_execution::AppendRequestIdentity::Append {
            encoding_version,
            request_hash,
            requested_node_count,
            ..
        } => (
            *encoding_version,
            request_hash.as_str(),
            Some(*requested_node_count as i64),
        ),
        lash_core_execution::AppendRequestIdentity::SemanticBoundary {
            operation: _,
            encoding_version,
            request_hash,
        } => (*encoding_version, request_hash.as_str(), None),
    };
    let encoding_version =
        i32::try_from(encoding_version).map_err(|_| StoreError::RecordEncodingFailed {
            record_kind: "RuntimeCommitReceipt append identity".to_string(),
            message: format!(
                "identity_encoding_version `{}` does not fit PostgreSQL INTEGER",
                encoding_version
            ),
        })?;
    Ok((
        Some(request_hash),
        requested_node_count,
        Some(encoding_version),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires PostgreSQL"]
    async fn replayed_cancellation_wake_has_one_receipt() {
        let database =
            crate::testing::IsolatedDatabase::create(&crate::testing::required_database_url())
                .await;
        let storage = crate::PostgresStorage::connect(database.url())
            .await
            .expect("open receipt fixture");
        let session = SessionId::from("session");
        let turn = TurnId::from("turn");
        let mut tx = storage
            .pool()
            .begin()
            .await
            .expect("begin receipt transaction");
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests_postgres
                .insert_first
                .sql(),
        )
        .bind(session.as_str())
        .bind(turn.as_str())
        .bind("request")
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind("defer")
        .bind("immediate")
        .execute(&mut *tx)
        .await
        .expect("seed request");
        let process_id = lash_sansio::ProcessId::fixture("process");
        let wake = lash_core_execution::ProcessWakeDelivery {
            version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            target_session_id: session.clone(),
            process_id: process_id.clone(),
            sequence: 1,
            event_type: "process.wake".into(),
            process_caused_by: None,
            authority: Default::default(),
            input: "wake payload".into(),
            created_at_ms: 0,
        };
        let affected = lash_core_execution::TurnCancelAffectedWake::deferred("batch".into(), wake);
        append_turn_cancel_wake_tx(&mut tx, &session, &turn, &affected)
            .await
            .expect("append wake");
        append_turn_cancel_wake_tx(&mut tx, &session, &turn, &affected)
            .await
            .expect("replay wake");
        let loaded = load_turn_cancel_request_tx(&mut tx, &session, &turn)
            .await
            .expect("read receipt")
            .expect("request exists");
        assert_eq!(
            loaded.outcome.expect("affected wake exists").affected_wakes,
            vec![affected]
        );
        tx.rollback().await.expect("rollback fixture");
    }

    #[test]
    fn append_identity_columns_refuse_encoding_versions_that_do_not_fit_postgres_integer() {
        let identity = lash_core_execution::AppendRequestIdentity::Append {
            encoding_version: i32::MAX as u32 + 1,
            request_hash: "request-hash".to_string(),
            requested_node_count: 1,
            requested_ancestor_node_id: None,
        };

        let error = append_identity_columns(&identity)
            .expect_err("out-of-range encoding version must be refused");
        match error {
            StoreError::RecordEncodingFailed {
                record_kind,
                message,
            } => {
                assert_eq!(record_kind, "RuntimeCommitReceipt append identity");
                assert_eq!(
                    message,
                    "identity_encoding_version `2147483648` does not fit PostgreSQL INTEGER"
                );
            }
            other => panic!("expected typed record encoding failure, got {other:?}"),
        }
    }
}
