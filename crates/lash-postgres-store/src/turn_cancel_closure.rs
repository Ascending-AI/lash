use crate::{SessionId, StoreError, store_sqlx_error};

pub(crate) async fn ensure_session_not_pinned_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let pending_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM lash_turn_cancel_closure_authorizations WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let pending_count =
        usize::try_from(pending_count).map_err(|_| StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelClosureAuthorization",
            message: "negative pending closure count".to_string(),
        })?;
    if pending_count != 0 {
        return Err(StoreError::TurnCancelClosureLifecyclePinned {
            session_id: session_id.clone(),
            pending_count,
        });
    }
    Ok(())
}

pub(crate) async fn ensure_sessions_not_pinned_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_ids: &[SessionId],
) -> Result<(), lash_core::MaintenanceFailure<lash_core::SessionBlobReclaimReport>> {
    for session_id in session_ids {
        ensure_session_not_pinned_tx(tx, session_id)
            .await
            .map_err(lash_core::MaintenanceFailure::failed_before_any_work)?;
    }
    Ok(())
}
