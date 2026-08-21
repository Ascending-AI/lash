use super::*;

struct PostgresSessionDeleteBlobProbe {
    storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl lash_core::testing::conformance::SessionDeleteBlobProbe for PostgresSessionDeleteBlobProbe {
    async fn blob_exists(&self, blob_ref: &lash_core::BlobRef) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM lash_blobs WHERE hash = $1)")
            .bind(blob_ref.as_str())
            .fetch_one(self.storage.pool())
            .await
            .expect("query Postgres blob existence")
    }

    async fn fail_next_blob_delete(&self) {
        sqlx::query(
            "CREATE OR REPLACE FUNCTION lash_fail_session_blob_delete()
             RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 RAISE EXCEPTION 'injected session blob delete failure';
             END
             $$",
        )
        .execute(self.storage.pool())
        .await
        .expect("create Postgres blob-delete failure function");
        sqlx::query(
            "CREATE TRIGGER fail_session_blob_delete
             BEFORE DELETE ON lash_blobs
             FOR EACH ROW EXECUTE FUNCTION lash_fail_session_blob_delete()",
        )
        .execute(self.storage.pool())
        .await
        .expect("install Postgres blob-delete failure");
    }

    async fn clear_blob_delete_failure(&self) {
        sqlx::query("DROP TRIGGER fail_session_blob_delete ON lash_blobs")
            .execute(self.storage.pool())
            .await
            .expect("remove Postgres blob-delete trigger");
        sqlx::query("DROP FUNCTION lash_fail_session_blob_delete()")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_session_delete_blob_reclaim_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres session-delete blob conformance: database is not configured");
        return;
    };
    let storage = Arc::new(storage);
    let make_storage = Arc::clone(&storage);
    lash_core::testing::conformance::session_delete_blob_reclaim_conformance("postgres", || {
        let storage = Arc::clone(&make_storage);
        sync_await(async move {
            reset(&storage).await;
            lash_core::testing::conformance::SessionDeleteBlobHandles {
                factory: Arc::new(storage.session_store_factory()) as Arc<dyn SessionStoreFactory>,
                probe: Arc::new(PostgresSessionDeleteBlobProbe { storage })
                    as Arc<dyn lash_core::testing::conformance::SessionDeleteBlobProbe>,
            }
        })
    })
    .await;
}
