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
}

#[async_trait::async_trait]
impl lash_conformance::SessionDeleteBlobProbe for PostgresBlobProbe {
    async fn blob_exists(&self, blob_ref: &lash_core::BlobRef) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM lash_blobs WHERE hash = $1)")
            .bind(blob_ref.as_str())
            .fetch_one(self.storage.pool())
            .await
            .expect("query Postgres blob existence")
    }

    async fn fail_next_blob_delete(&self) {
        let function = self.fault_function();
        sqlx::query(&format!(
            "CREATE OR REPLACE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'injected blob delete failure'; END $$"
        ))
        .execute(self.storage.pool())
        .await
        .expect("create Postgres blob-delete fault function");
        sqlx::query(&format!(
            "CREATE TRIGGER {} BEFORE DELETE ON lash_blobs \
             FOR EACH ROW EXECUTE FUNCTION {function}()",
            self.fault_name
        ))
        .execute(self.storage.pool())
        .await
        .expect("install Postgres blob-delete fault");
    }

    async fn clear_blob_delete_failure(&self) {
        sqlx::query(&format!("DROP TRIGGER {} ON lash_blobs", self.fault_name))
            .execute(self.storage.pool())
            .await
            .expect("remove Postgres blob-delete trigger");
        sqlx::query(&format!("DROP FUNCTION {}()", self.fault_function()))
            .execute(self.storage.pool())
            .await
            .expect("remove Postgres blob-delete function");
    }

    async fn checkpoint_component_edge_exists(
        &self,
        checkpoint_ref: &lash_core::BlobRef,
        blob_ref: &lash_core::BlobRef,
    ) -> Option<bool> {
        Some(
            sqlx::query_scalar(
                "SELECT EXISTS(
                     SELECT 1 FROM lash_checkpoint_blob_refs
                     WHERE checkpoint_ref = $1 AND blob_ref = $2
                 )",
            )
            .bind(checkpoint_ref.as_str())
            .bind(blob_ref.as_str())
            .fetch_one(self.storage.pool())
            .await
            .expect("query Postgres checkpoint edge"),
        )
    }

    async fn break_factory_gc_scope(&self, checkpoint_ref: &lash_core::BlobRef) -> bool {
        assert_eq!(
            sqlx::query("UPDATE lash_blobs SET content = '\\xffffffff'::bytea WHERE hash = $1")
                .bind(checkpoint_ref.as_str())
                .execute(self.storage.pool())
                .await
                .expect("corrupt rooted Postgres checkpoint manifest")
                .rows_affected(),
            1
        );
        true
    }
}
