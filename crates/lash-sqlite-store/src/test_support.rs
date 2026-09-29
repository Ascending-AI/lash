//! [`StoreTestSupport`] is the home for every `*_for_testing` hook the
//! lash-core conformance and differential suites need from this backend; the
//! production store traits carry none.

use super::*;
use lash_core_execution::store::{DecodedRowCounts, GraphRowCorruption, StoreTestSupport};

#[async_trait::async_trait]
impl StoreTestSupport for SqliteStore {
    fn decoded_row_counts_for_testing(&self) -> DecodedRowCounts {
        DecodedRowCounts {
            graph_node_bodies: self.decoded_graph_node_bodies.load(AtomicOrdering::Relaxed),
            usage_rows: self.decoded_usage_rows.load(AtomicOrdering::Relaxed),
            usage_holes: self.decoded_usage_holes.load(AtomicOrdering::Relaxed),
            turn_receipt_bodies: self
                .decoded_turn_receipt_bodies
                .load(AtomicOrdering::Relaxed),
        }
    }

    async fn corrupt_graph_row_for_testing(
        &self,
        node_id: &lash_core_execution::NodeId,
        corruption: GraphRowCorruption,
    ) -> Result<(), StoreError> {
        let node_id = node_id.clone();
        let fleet = self.fleet_format;
        self.conn
            .write(move |tx| {
                let sql = &crate::session_sql::session_sql().graph_sqlite;
                match corruption {
                    GraphRowCorruption::DeleteRow => {
                        crate::conn::cached_execute(
                            tx,
                            sql.delete_by_id_for_testing.sql(),
                            params![node_id.as_str()],
                        )?;
                    }
                    GraphRowCorruption::SetParent(parent) => {
                        crate::conn::cached_execute(
                            tx,
                            sql.set_parent_for_testing.sql(),
                            params![
                                node_id.as_str(),
                                parent.as_ref().map(lash_core_execution::NodeId::as_str)
                            ],
                        )?;
                    }
                    GraphRowCorruption::SetFramePointer(frame) => {
                        crate::conn::cached_execute(
                            tx,
                            sql.set_frame_pointer_for_testing.sql(),
                            params![node_id.as_str(), frame.as_str()],
                        )?;
                    }
                    GraphRowCorruption::SetBodyBytes(bytes) => {
                        let bytes =
                            i64::try_from(bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
                        crate::conn::cached_execute(
                            tx,
                            sql.set_body_bytes_for_testing.sql(),
                            params![node_id.as_str(), bytes],
                        )?;
                    }
                    GraphRowCorruption::SetPayloadKindToPlugin => {
                        let (parent, body): (Option<String>, String) = tx.query_row(
                            "SELECT parent_node_id, node_json FROM graph_nodes WHERE node_id = ?1",
                            params![node_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )?;
                        let mut node =
                            lash_core_execution::SessionNodeRecord::decode_storage_body_for_fleet(
                                node_id.as_str().to_owned(),
                                parent,
                                &body,
                                fleet,
                            )
                            .map_err(|error| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                            })?;
                        node.payload = lash_core_execution::SessionNodePayload::Plugin {
                            plugin_type: "corrupt-anchor-test".to_owned(),
                            body: lash_core_execution::session_graph::SharedJsonValue::new(
                                serde_json::json!({}),
                            ),
                        };
                        let body = node.encode_storage_body(fleet).map_err(|error| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                        })?;
                        let bytes =
                            i64::try_from(body.len()).map_err(|_| rusqlite::Error::InvalidQuery)?;
                        crate::conn::cached_execute(
                            tx,
                            sql.set_body_for_testing.sql(),
                            params![node_id.as_str(), body, bytes],
                        )?;
                    }
                }
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn set_head_current_frame_for_testing(
        &self,
        session_id: &SessionId,
        frame: Option<lash_core_execution::FrameNodeId>,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write(move |tx| {
                let head_json: String = tx.query_row(
                    crate::session_sql::session_sql()
                        .head
                        .select_head_json
                        .sql(),
                    params![session_id.as_str()],
                    |row| row.get(0),
                )?;
                let mut head: serde_json::Value = serde_json::from_str(&head_json)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                head["current_frame_node_id"] = serde_json::to_value(frame)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                let head_json = serde_json::to_string(&head)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                crate::conn::cached_execute(
                    tx,
                    crate::session_sql::session_sql().head.set_head_json.sql(),
                    params![session_id.as_str(), head_json],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn rewrite_session_tool_access_for_testing(
        &self,
        session_id: &SessionId,
        schema_version: u32,
        tool_access: Option<serde_json::Value>,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write(move |tx| {
                let head_json: String = tx.query_row(
                    crate::session_sql::session_sql()
                        .head
                        .select_head_json
                        .sql(),
                    params![session_id.as_str()],
                    |row| row.get(0),
                )?;
                let mut head: serde_json::Value = serde_json::from_str(&head_json)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                head["schema_version"] = serde_json::json!(schema_version);
                let config = head
                    .get_mut("config")
                    .and_then(serde_json::Value::as_object_mut)
                    .ok_or(rusqlite::Error::InvalidQuery)?;
                match tool_access {
                    Some(tool_access) => {
                        config.insert("tool_access".to_string(), tool_access);
                    }
                    None => {
                        config.remove("tool_access");
                    }
                }
                let head_json = serde_json::to_string(&head)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                crate::conn::cached_execute(
                    tx,
                    crate::session_sql::session_sql().head.set_head_json.sql(),
                    params![session_id.as_str(), head_json],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn stamp_session_state_version_for_testing(
        &self,
        session_id: &SessionId,
        version: u32,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write(move |tx| {
                crate::conn::cached_execute(
                    tx,
                    crate::session_sql::session_sql()
                        .meta
                        .set_state_version
                        .sql(),
                    params![session_id.as_str(), i64::from(version)],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn stamp_session_state_version_and_corrupt_payload_for_testing(
        &self,
        session_id: &SessionId,
        version: u32,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write(move |tx| {
                crate::conn::cached_execute(
                    tx,
                    crate::session_sql::session_sql()
                        .meta
                        .set_state_version
                        .sql(),
                    params![session_id.as_str(), i64::from(version)],
                )?;
                crate::conn::cached_execute(
                    tx,
                    crate::session_sql::session_sql()
                        .head
                        .corrupt_head_json
                        .sql(),
                    params![session_id.as_str()],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }
}

/// An unbound durable-core store on a fresh memory store set. The store holds
/// the store set's anchors, so the database lives as long as it does.
#[cfg(test)]
pub(crate) async fn memory_store() -> tokio_rusqlite::Result<Arc<SqliteStore>> {
    memory_store_with_options(crate::SqliteStoreSetOptions::memory().store).await
}

/// [`memory_store`] with explicit store options.
#[cfg(test)]
pub(crate) async fn memory_store_with_options(
    options: StoreOptions,
) -> tokio_rusqlite::Result<Arc<SqliteStore>> {
    crate::SqliteStoreSet::memory_with_options_and_clock(
        crate::SqliteStoreSetOptions {
            store: options,
            ..crate::SqliteStoreSetOptions::memory()
        },
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await?
    .open_store()
    .await
}
