use crate::{SessionId, StoreError, store_sqlx_error};

pub(crate) async fn retire_scope(
    pool: &sqlx::PgPool,
    scope: &lash_core::ExecutionScope,
) -> Result<(), StoreError> {
    let scope_id = scope
        .journal_identity()
        .map_err(|error| StoreError::Backend(error.to_string()))?
        .key()
        .to_string();
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    crate::await_event::lock_scope(&mut tx, &scope_id)
        .await
        .map_err(store_sqlx_error)?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT session_id, authorization_json FROM lash_turn_cancel_closure_authorizations ORDER BY session_id, turn_id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    for (session_id, encoded) in rows {
        let authorization: lash_core::TurnCancelClosureAuthorization =
            serde_json::from_str(&encoded).map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "TurnCancelClosureAuthorization",
                message: error.to_string(),
            })?;
        if authorization.admitted_scope() == scope {
            return Err(StoreError::TurnCancelClosureLifecyclePinned {
                session_id: SessionId::from(session_id),
                pending_count: 1,
            });
        }
    }
    sqlx::query(
        "INSERT INTO lash_turn_cancel_retired_scopes (scope_id) VALUES ($1) ON CONFLICT DO NOTHING",
    )
    .bind(&scope_id)
    .execute(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(())
}

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
) -> Result<(), StoreError> {
    for session_id in session_ids {
        ensure_session_not_pinned_tx(tx, session_id).await?;
    }
    Ok(())
}
