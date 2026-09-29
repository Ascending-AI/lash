//! [`StoreTestSupport`] is the home for every `*_for_testing` hook the
//! lash-core conformance and differential suites need from this backend; the
//! production store traits carry none.

use crate::*;
use lash_core_execution::store::{DecodedRowCounts, GraphRowCorruption, StoreTestSupport};
use std::sync::atomic::Ordering;

#[async_trait::async_trait]
impl StoreTestSupport for PostgresStore {
    fn decoded_row_counts_for_testing(&self) -> DecodedRowCounts {
        DecodedRowCounts {
            graph_node_bodies: self.decoded_graph_node_bodies.load(Ordering::Relaxed),
            usage_rows: self.decoded_usage_rows.load(Ordering::Relaxed),
            usage_holes: self.decoded_usage_holes.load(Ordering::Relaxed),
            turn_receipt_bodies: self.decoded_turn_receipts.load(Ordering::Relaxed),
        }
    }

    async fn corrupt_graph_row_for_testing(
        &self,
        node_id: &lash_core_execution::NodeId,
        corruption: GraphRowCorruption,
    ) -> Result<(), StoreError> {
        let result = match corruption {
            GraphRowCorruption::DeleteRow => {
                sqlx::query("DELETE FROM lash_graph_nodes WHERE node_id = $1")
                    .bind(node_id.as_str())
                    .execute(&self.pool)
                    .await
            }
            GraphRowCorruption::SetParent(parent) => {
                sqlx::query("UPDATE lash_graph_nodes SET parent_node_id = $2 WHERE node_id = $1")
                    .bind(node_id.as_str())
                    .bind(parent.as_ref().map(|id| id.as_str()))
                    .execute(&self.pool)
                    .await
            }
            GraphRowCorruption::SetFramePointer(frame) => {
                sqlx::query("UPDATE lash_graph_nodes SET frame_node_id = $2 WHERE node_id = $1")
                    .bind(node_id.as_str())
                    .bind(frame.as_str())
                    .execute(&self.pool)
                    .await
            }
            GraphRowCorruption::SetBodyBytes(bytes) => {
                sqlx::query("UPDATE lash_graph_nodes SET body_bytes = $2 WHERE node_id = $1")
                    .bind(node_id.as_str())
                    .bind(i64::try_from(bytes).map_err(|_| {
                        StoreError::Backend("test body size exceeds BIGINT".to_string())
                    })?)
                    .execute(&self.pool)
                    .await
            }
            GraphRowCorruption::SetPayloadKindToPlugin => {
                let original: String = sqlx::query_scalar(
                    "SELECT node_json FROM lash_graph_nodes WHERE node_id = $1",
                )
                .bind(node_id.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(store_sqlx_error)?;
                let mut body: serde_json::Value = serde_json::from_str(&original)
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let object = body.as_object_mut().ok_or_else(|| {
                    StoreError::Backend("test graph node body is not an object".to_string())
                })?;
                object.retain(|key, _| key == "schema_version" || key == "timestamp");
                let payload = serde_json::to_value(GraphRowCorruption::plugin_payload())
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                object.extend(payload.as_object().ok_or_else(|| {
                    StoreError::Backend("test plugin payload is not an object".to_string())
                })?.clone());
                let json = serde_json::to_string(&body)
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                sqlx::query(
                    "UPDATE lash_graph_nodes SET node_json = $2, body_bytes = $3 WHERE node_id = $1",
                )
                .bind(node_id.as_str())
                .bind(&json)
                .bind(i64::try_from(json.len()).map_err(|_| {
                    StoreError::Backend("test body size exceeds BIGINT".to_string())
                })?)
                .execute(&self.pool)
                .await
            }
        }
        .map_err(store_sqlx_error)?;
        if result.rows_affected() != 1 {
            return Err(StoreError::Backend(format!(
                "test graph node `{node_id}` is missing"
            )));
        }
        Ok(())
    }

    async fn set_head_current_frame_for_testing(
        &self,
        session_id: &SessionId,
        frame: Option<lash_core_execution::FrameNodeId>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let mut head: serde_json::Value = sqlx::query_scalar::<_, String>(
            crate::session_sql::session_sql()
                .head
                .select_head_json_for_update
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)
        .and_then(|json| {
            serde_json::from_str(&json).map_err(|error| StoreError::Backend(error.to_string()))
        })?;
        head["current_frame_node_id"] =
            serde_json::to_value(frame).map_err(|error| StoreError::Backend(error.to_string()))?;
        sqlx::query(crate::session_sql::session_sql().head.set_head_json.sql())
            .bind(session_id.as_str())
            .bind(
                serde_json::to_string(&head)
                    .map_err(|error| StoreError::Backend(error.to_string()))?,
            )
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }
    async fn rewrite_session_tool_access_for_testing(
        &self,
        session_id: &SessionId,
        schema_version: u32,
        tool_access: Option<serde_json::Value>,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let head_json: String = sqlx::query_scalar(
            crate::session_sql::session_sql()
                .head
                .select_head_json_for_update
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut head: serde_json::Value = serde_json::from_str(&head_json).map_err(|error| {
            StoreError::Backend(format!("failed to decode test session head: {error}"))
        })?;
        head["schema_version"] = serde_json::json!(schema_version);
        let config = head
            .get_mut("config")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| {
                StoreError::Backend("test session head has no config object".to_string())
            })?;
        match tool_access {
            Some(tool_access) => {
                config.insert("tool_access".to_string(), tool_access);
            }
            None => {
                config.remove("tool_access");
            }
        }
        let head_json = serde_json::to_string(&head).map_err(|error| {
            StoreError::Backend(format!("failed to encode test session head: {error}"))
        })?;
        sqlx::query(crate::session_sql::session_sql().head.set_head_json.sql())
            .bind(session_id.as_str())
            .bind(head_json)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn stamp_session_state_version_for_testing(
        &self,
        session_id: &SessionId,
        version: u32,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        sqlx::query(
            crate::session_sql::session_sql()
                .meta
                .set_state_version
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(
            i32::try_from(version).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "SessionStateVersion",
                message: format!("test marker {version} exceeds PostgreSQL INTEGER"),
            })?,
        )
        .execute(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn stamp_session_state_version_and_corrupt_payload_for_testing(
        &self,
        session_id: &SessionId,
        version: u32,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            crate::session_sql::session_sql()
                .meta
                .set_state_version
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(
            i32::try_from(version).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "SessionStateVersion",
                message: format!("test marker {version} exceeds PostgreSQL INTEGER"),
            })?,
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        sqlx::query(
            crate::session_sql::session_sql()
                .head
                .corrupt_head_json
                .sql(),
        )
        .bind(session_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }
}

impl PostgresStore {
    /// Drive transaction admission time independently of record timestamps.
    pub fn with_lease_clock_for_testing(
        mut self,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        self.lease_clock_for_testing = Some(clock);
        self
    }

    /// Stand this store's writers up on `fleet_format` — typically one
    /// carrying a pin table via [`lash_core_execution::FleetFormat::with_writer_pins`]
    /// — so a test proves they emit the version `F` assigns rather than the
    /// build constant they would stamp anyway (FIG-3796).
    pub fn with_fleet_format_for_testing(
        mut self,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Self {
        self.fleet_format = fleet_format;
        self
    }
}

pub(crate) async fn set_transaction_lease_clock_for_testing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    clock: Option<&Arc<dyn lash_core_execution::Clock>>,
) -> Result<(), StoreError> {
    if let Some(clock) = clock {
        sqlx::query("SELECT set_config('lash.test_lease_epoch_ms', $1, true)")
            .bind(clock.timestamp_ms().to_string())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

impl PostgresStore {
    pub(crate) async fn set_transaction_lease_clock_for_testing(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), StoreError> {
        set_transaction_lease_clock_for_testing(tx, self.lease_clock_for_testing.as_ref()).await
    }
}
