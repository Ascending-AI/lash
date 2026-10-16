use std::sync::Arc;

use lash_postgres_store::PostgresStorage;

pub(crate) struct PostgresBlobProbe {
    storage: Arc<PostgresStorage>,
    fault_name: &'static str,
}

impl PostgresBlobProbe {
    pub(crate) fn new(storage: Arc<PostgresStorage>, fault_name: &'static str) -> Self {
        assert!(matches!(
            fault_name,
            "fail_session_blob_delete" | "fail_process_prune_blob_delete"
        ));
        Self {
            storage,
            fault_name,
        }
    }

    fn fault_function(&self) -> String {
        format!("lash_{}", self.fault_name)
    }

    /// Fails blob deletes until cleared.
    async fn fail_blob_deletes(&self) {
        let function = self.fault_function();
        sqlx::query(&format!(
            "CREATE OR REPLACE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'injected blob delete failure'; END $$"
        ))
        .execute(self.storage.pool())
        .await
        .expect("create Postgres fault function");
        sqlx::query(&format!(
            "CREATE TRIGGER {} BEFORE DELETE ON lash_blobs \
             FOR EACH ROW EXECUTE FUNCTION {function}()",
            self.fault_name
        ))
        .execute(self.storage.pool())
        .await
        .expect("install Postgres fault");
    }
}

#[async_trait::async_trait]
impl lash_conformance::SessionDeleteBlobProbe for PostgresBlobProbe {
    async fn blob_exists(&self, blob_ref: &lash_core_execution::BlobRef) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM lash_blobs WHERE hash = $1)")
            .bind(blob_ref.as_str())
            .fetch_one(self.storage.pool())
            .await
            .expect("query Postgres blob existence")
    }

    async fn fail_next_blob_delete(&self) {
        self.fail_blob_deletes().await;
    }

    async fn fail_next_retirement_hold(&self) {
        lash_postgres_store::testing::cut_retirement_holds(
            self.storage.pool(),
            self.fault_name,
            true,
        )
        .await
        .expect("install Postgres retirement-hold fault");
    }

    async fn clear_injected_failure(&self) {
        sqlx::query(&format!(
            "DROP TRIGGER IF EXISTS {} ON lash_blobs",
            self.fault_name
        ))
        .execute(self.storage.pool())
        .await
        .expect("remove Postgres blob-delete fault trigger");
        lash_postgres_store::testing::cut_retirement_holds(
            self.storage.pool(),
            self.fault_name,
            false,
        )
        .await
        .expect("remove Postgres retirement-hold fault trigger");
        sqlx::query(&format!("DROP FUNCTION {}()", self.fault_function()))
            .execute(self.storage.pool())
            .await
            .expect("remove Postgres fault function");
    }

    async fn checkpoint_component_edge_exists(
        &self,
        checkpoint_ref: &lash_core_execution::BlobRef,
        blob_ref: &lash_core_execution::BlobRef,
    ) -> Option<bool> {
        Some(
            sqlx::query_scalar(
                "SELECT EXISTS(
                     SELECT 1 FROM lash_blob_edges
                     WHERE parent_hash = $1 AND child_hash = $2
                 )",
            )
            .bind(checkpoint_ref.as_str())
            .bind(blob_ref.as_str())
            .fetch_one(self.storage.pool())
            .await
            .expect("query Postgres checkpoint edge"),
        )
    }
}
