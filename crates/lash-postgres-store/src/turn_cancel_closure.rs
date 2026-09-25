use crate::{SessionId, StoreError, store_sqlx_error};

/// Advisory-lock namespace for session-free scopes, disjoint from every
/// session-keyed namespace so a process or runtime-operation scope never
/// contends with a session whose id happens to hash alike.
const SCOPE_LOCK_NAMESPACE: i64 = 563;

/// Serialize a session-free scope's closure authorization, its closure
/// commit, and its retirement against each other.
pub(crate) async fn lock_scope(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text_seeded
            .sql(),
    )
    .bind(scope_id)
    .bind(SCOPE_LOCK_NAMESPACE)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn retire_scope(
    pool: &sqlx::PgPool,
    scope: &lash_core_execution::ExecutionScope,
) -> Result<(), StoreError> {
    let scope_id = scope
        .journal_identity()
        .map_err(|error| StoreError::Backend(error.to_string()))?
        .key()
        .to_string();
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    lock_scope(&mut tx, &scope_id)
        .await
        .map_err(store_sqlx_error)?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .closures
            .list_all
            .sql(),
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    for (session_id, encoded) in rows {
        let authorization: lash_core_execution::TurnCancelClosureAuthorization =
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
        crate::turn_ingress::turn_ingress_sql()
            .retired_scopes_postgres
            .insert_new
            .sql(),
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
        crate::turn_ingress::turn_ingress_sql()
            .closures
            .count_by_session
            .sql(),
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
