//! SQL commit ownership of engine waits.
use crate::*;
use lash_core_execution::store::{
    StoreTransition, WaitReceiptStore, WaitRequestReceipt, WaitResolutionReceipt,
    require_wait_request_matches, require_wait_resolution_matches,
};

fn sql() -> &'static lash_store_sql::wait_receipts::WaitReceiptStatements {
    static SQL: std::sync::LazyLock<lash_store_sql::wait_receipts::WaitReceiptStatements> =
        std::sync::LazyLock::new(|| {
            lash_store_sql::wait_receipts::WaitReceiptStatements::render(
                lash_store_sql::Dialect::postgres(),
            )
        });
    &SQL
}
fn decode<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|e| StoreError::Backend(e.to_string()))
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|e| StoreError::Backend(e.to_string()))
}
#[async_trait::async_trait]
impl WaitReceiptStore for PostgresStore {
    async fn record_wait_request(
        &self,
        request: &WaitRequestReceipt,
    ) -> Result<StoreTransition<WaitRequestReceipt>, StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        if let Some(session) = &request.session_id {
            super::runtime_persistence::lock_session_history_mutation_tx(&mut tx, session).await?;
            super::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session).await?;
        }
        let changed = sqlx::query(sql().insert_request.sql())
            .bind(&request.wait_id)
            .bind(&request.owner_key)
            .bind(request.session_id.as_ref().map(|id| id.as_str()))
            .bind(clamp_epoch_ms(request.started_at_ms))
            .bind(encode(request)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected()
            == 1;
        let json: String = sqlx::query_scalar(sql().select_request.sql())
            .bind(&request.wait_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let record: WaitRequestReceipt = decode(&json)?;
        require_wait_request_matches(&record, request)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(StoreTransition { record, changed })
    }
    async fn record_wait_resolution(
        &self,
        resolution: &WaitResolutionReceipt,
    ) -> Result<StoreTransition<WaitResolutionReceipt>, StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        let json: String = sqlx::query_scalar(sql().select_request.sql())
            .bind(&resolution.wait_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let request: WaitRequestReceipt = decode(&json)?;
        if let Some(session) = &request.session_id {
            super::runtime_persistence::lock_session_history_mutation_tx(&mut tx, session).await?;
            super::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session).await?;
        }
        let changed = sqlx::query(sql().resolve.sql())
            .bind(&resolution.wait_id)
            .bind(encode(resolution)?)
            .bind(clamp_epoch_ms(resolution.resolved_at_ms))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected()
            == 1;
        let json: String = sqlx::query_scalar(sql().select_resolution.sql())
            .bind(&resolution.wait_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let record: WaitResolutionReceipt = decode(&json)?;
        require_wait_resolution_matches(&record, resolution)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(StoreTransition { record, changed })
    }
    async fn retire_observation_receipts(
        &self,
        owner_key: &str,
        retired_at_ms: u64,
    ) -> Result<(), StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        sqlx::query(sql().retire.sql())
            .bind(owner_key)
            .bind(clamp_epoch_ms(retired_at_ms))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let tool_sql = lash_store_sql::tool_receipts::ToolReceiptStatements::render(
            lash_store_sql::Dialect::postgres(),
        );
        sqlx::query(tool_sql.retire.sql())
            .bind(owner_key)
            .bind(clamp_epoch_ms(retired_at_ms))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }
}
