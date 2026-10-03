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
                crate::schema_layout::Schema::Main.dialect(),
            )
        });
    &SQL
}
fn finish<T>(result: Result<T, StoreError>) -> crate::conn::TxOutcome<Result<T, StoreError>> {
    match result {
        Ok(value) => crate::conn::TxOutcome::Commit(Ok(value)),
        Err(error) => crate::conn::TxOutcome::Rollback(Err(error)),
    }
}
fn decode<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|e| StoreError::Backend(e.to_string()))
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|e| StoreError::Backend(e.to_string()))
}
#[async_trait::async_trait]
impl WaitReceiptStore for SqliteStore {
    async fn record_wait_request(
        &self,
        request: &WaitRequestReceipt,
    ) -> Result<StoreTransition<WaitRequestReceipt>, StoreError> {
        let request = request.clone();
        self.conn
            .write_flow(move |tx| {
                Ok(finish((|| {
                    if let Some(session) = &request.session_id {
                        crate::persistence::ensure_session_not_deleted_conn(tx, session)?;
                    }
                    let changed = tx
                        .execute(
                            sql().insert_request.sql(),
                            params![
                                request.wait_id,
                                request.owner_key,
                                request.session_id.as_ref().map(|id| id.as_str()),
                                clamp_epoch_ms(request.started_at_ms),
                                encode(&request)?
                            ],
                        )
                        .map_err(sqlite_error)?
                        == 1;
                    let json: String = tx
                        .query_row(
                            sql().select_request.sql(),
                            params![request.wait_id],
                            |row| row.get(0),
                        )
                        .map_err(sqlite_error)?;
                    let record: WaitRequestReceipt = decode(&json)?;
                    require_wait_request_matches(&record, &request)?;
                    Ok(StoreTransition { record, changed })
                })()))
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn record_wait_resolution(
        &self,
        resolution: &WaitResolutionReceipt,
    ) -> Result<StoreTransition<WaitResolutionReceipt>, StoreError> {
        let resolution = resolution.clone();
        self.conn
            .write_flow(move |tx| {
                Ok(finish((|| {
                    let json: String = tx
                        .query_row(
                            sql().select_request.sql(),
                            params![resolution.wait_id],
                            |row| row.get(0),
                        )
                        .map_err(sqlite_error)?;
                    let request: WaitRequestReceipt = decode(&json)?;
                    if let Some(session) = &request.session_id {
                        crate::persistence::ensure_session_not_deleted_conn(tx, session)?;
                    }
                    let changed = tx
                        .execute(
                            sql().resolve.sql(),
                            params![
                                resolution.wait_id,
                                encode(&resolution)?,
                                clamp_epoch_ms(resolution.resolved_at_ms)
                            ],
                        )
                        .map_err(sqlite_error)?
                        == 1;
                    let json: String = tx
                        .query_row(
                            sql().select_resolution.sql(),
                            params![resolution.wait_id],
                            |row| row.get(0),
                        )
                        .map_err(sqlite_error)?;
                    let record: WaitResolutionReceipt = decode(&json)?;
                    require_wait_resolution_matches(&record, &resolution)?;
                    Ok(StoreTransition { record, changed })
                })()))
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn retire_observation_receipts(
        &self,
        owner_key: &str,
        retired_at_ms: u64,
    ) -> Result<(), StoreError> {
        let owner = owner_key.to_owned();
        self.conn
            .write(move |tx| {
                tx.execute(
                    sql().retire.sql(),
                    params![owner, clamp_epoch_ms(retired_at_ms)],
                )?;
                let tool_sql = lash_store_sql::tool_receipts::ToolReceiptStatements::render(
                    crate::schema_layout::Schema::Main.dialect(),
                );
                tx.execute(
                    tool_sql.retire.sql(),
                    params![owner, clamp_epoch_ms(retired_at_ms)],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }
}
