use std::sync::Arc;

use super::*;
use lash_core::testing::conformance::{
    GraphIntegrityCorruption, GraphIntegrityHandles, GraphIntegrityInjector, GraphIntegrityRead,
    GraphIntegrityTarget,
};

struct PostgresGraphIntegrityInjector {
    storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl GraphIntegrityInjector for PostgresGraphIntegrityInjector {
    async fn inject(&self, target: &GraphIntegrityTarget) {
        let result = match target.corruption {
            GraphIntegrityCorruption::OrphanLeaf => {
                sqlx::query("UPDATE lash_graph_nodes SET parent_node_id = $1 WHERE node_id = $2")
                    .bind(&target.missing_node_id)
                    .bind(&target.leaf_node_id)
                    .execute(self.storage.pool())
                    .await
                    .expect("inject orphaned Postgres graph leaf")
            }
            GraphIntegrityCorruption::DuplicateNodeId => {
                sqlx::query("ALTER TABLE lash_graph_nodes DROP CONSTRAINT lash_graph_nodes_pkey")
                    .execute(self.storage.pool())
                    .await
                    .expect("remove Postgres graph-node uniqueness for corruption injection");
                sqlx::query(
                    "ALTER TABLE lash_graph_nodes
                     DROP CONSTRAINT lash_graph_nodes_session_id_generation_key",
                )
                .execute(self.storage.pool())
                .await
                .expect("remove Postgres graph-generation uniqueness for corruption injection");
                sqlx::query(
                    "INSERT INTO lash_graph_nodes (
                         session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned
                     )
                     SELECT session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned
                     FROM lash_graph_nodes WHERE node_id = $1 LIMIT 1",
                )
                .bind(&target.leaf_node_id)
                .execute(self.storage.pool())
                .await
                .expect("inject duplicate Postgres graph node id")
            }
            GraphIntegrityCorruption::DanglingLeafId => {
                sqlx::query("UPDATE lash_sessions SET leaf_node_id = $1 WHERE session_id = $2")
                    .bind(&target.missing_node_id)
                    .bind(&target.session_id)
                    .execute(self.storage.pool())
                    .await
                    .expect("inject dangling Postgres graph leaf id")
            }
            GraphIntegrityCorruption::ParentCycle => {
                if target.read == GraphIntegrityRead::ActivePath {
                    sqlx::query(
                        "UPDATE lash_graph_nodes SET parent_node_id = $1 WHERE node_id = $2",
                    )
                    .bind(&target.leaf_node_id)
                    .bind(&target.root_node_id)
                    .execute(self.storage.pool())
                    .await
                    .expect("inject active Postgres graph parent cycle")
                } else {
                    let node_a_id = format!("{}-a", target.missing_node_id);
                    let node_b_id = format!("{}-b", target.missing_node_id);
                    let mut last_result = None;
                    for (node_id, parent_node_id, generation_offset) in [
                        (&node_a_id, &node_b_id, 1_i64),
                        (&node_b_id, &node_a_id, 2_i64),
                    ] {
                        let result = sqlx::query(
                            "INSERT INTO lash_graph_nodes (
                                 session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned
                             )
                             SELECT session_id, $1, $2, generation + $4, frame_node_id, node_json, tombstoned
                             FROM lash_graph_nodes WHERE node_id = $3 LIMIT 1",
                        )
                        .bind(node_id)
                        .bind(parent_node_id)
                        .bind(&target.leaf_node_id)
                        .bind(generation_offset)
                        .execute(self.storage.pool())
                        .await
                        .expect("inject inactive Postgres graph cycle node");
                        assert_eq!(result.rows_affected(), 1);
                        last_result = Some(result);
                    }
                    last_result.expect("inactive Postgres cycle inserts ran")
                }
            }
        };
        assert_eq!(result.rows_affected(), 1);
    }

    async fn load_whole_graph(
        &self,
        session_id: &str,
    ) -> Result<lash_core::SessionGraph, StoreError> {
        let mut tx = self
            .storage
            .pool()
            .begin()
            .await
            .map_err(store_sqlx_error)?;
        let leaf_node_id = load_session_head_meta_tx(&mut tx, session_id, false)
            .await?
            .and_then(|meta| meta.leaf_node_id);
        let graph = load_whole_graph_tx(&mut tx, session_id, leaf_node_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(graph)
    }

    async fn cleanup(&self, target: &GraphIntegrityTarget) {
        if target.corruption != GraphIntegrityCorruption::DuplicateNodeId {
            return;
        }
        sqlx::query(
            "DELETE FROM lash_graph_nodes duplicate
             USING lash_graph_nodes original
             WHERE duplicate.node_id = original.node_id
               AND duplicate.ctid > original.ctid",
        )
        .execute(self.storage.pool())
        .await
        .expect("remove duplicate Postgres graph rows after injection");
        sqlx::query(
            "ALTER TABLE lash_graph_nodes
             ADD CONSTRAINT lash_graph_nodes_pkey PRIMARY KEY (node_id)",
        )
        .execute(self.storage.pool())
        .await
        .expect("restore Postgres graph-node primary key after injection");
        sqlx::query(
            "ALTER TABLE lash_graph_nodes
             ADD CONSTRAINT lash_graph_nodes_session_id_generation_key
             UNIQUE (session_id, generation)",
        )
        .execute(self.storage.pool())
        .await
        .expect("restore Postgres graph-generation uniqueness after injection");
    }
}

async fn reset_graph_integrity_storage(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list Postgres graph-integrity tables");
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(storage.pool())
        .await
        .expect("reset Postgres graph-integrity tables");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_graph_integrity_conformance_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres graph-integrity conformance: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = Arc::new(
        PostgresStorage::connect(&database_url)
            .await
            .expect("connect Postgres graph-integrity storage"),
    );
    reset_graph_integrity_storage(&storage).await;
    lash_core::testing::conformance::graph_integrity_conformance(|_| {
        let storage = Arc::clone(&storage);
        async move {
            reset_graph_integrity_storage(&storage).await;
            GraphIntegrityHandles {
                runtime: Arc::new(storage.unbound_session_store()),
                injector: Arc::new(PostgresGraphIntegrityInjector { storage }),
            }
        }
    })
    .await;
    reset_graph_integrity_storage(&storage).await;
}
