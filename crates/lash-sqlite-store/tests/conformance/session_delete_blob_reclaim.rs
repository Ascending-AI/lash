use super::*;

struct SqliteSessionDeleteBlobProbe {
    _dir: Arc<tempfile::TempDir>,
    path: std::path::PathBuf,
}

#[async_trait::async_trait]
impl lash_core::testing::conformance::SessionDeleteBlobProbe for SqliteSessionDeleteBlobProbe {
    async fn blob_exists(&self, blob_ref: &lash_core::BlobRef) -> bool {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite blob probe");
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)",
            rusqlite::params![blob_ref.as_str()],
            |row| row.get(0),
        )
        .expect("query SQLite blob existence")
    }

    async fn fail_next_blob_delete(&self) {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite delete fault");
        conn.execute_batch(
            "CREATE TRIGGER fail_session_blob_delete
             BEFORE DELETE ON blobs
             BEGIN
                 SELECT RAISE(ABORT, 'injected session blob delete failure');
             END;",
        )
        .expect("install SQLite blob-delete failure");
    }

    async fn clear_blob_delete_failure(&self) {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite fault cleanup");
        conn.execute_batch("DROP TRIGGER fail_session_blob_delete")
            .expect("remove SQLite blob-delete failure");
    }

    async fn checkpoint_component_edge_exists(
        &self,
        checkpoint_ref: &lash_core::BlobRef,
        blob_ref: &lash_core::BlobRef,
    ) -> Option<bool> {
        Some(
            rusqlite::Connection::open(&self.path)
                .expect("open SQLite checkpoint-edge probe")
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM checkpoint_blob_refs
                         WHERE checkpoint_ref = ?1 AND blob_ref = ?2
                     )",
                    rusqlite::params![checkpoint_ref.as_str(), blob_ref.as_str()],
                    |row| row.get(0),
                )
                .expect("query SQLite checkpoint edge"),
        )
    }

    async fn break_factory_gc_scope(&self, checkpoint_ref: &lash_core::BlobRef) -> bool {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite GC fault");
        assert_eq!(
            conn.execute(
                "UPDATE blobs SET content = X'FF' WHERE hash = ?1",
                rusqlite::params![checkpoint_ref.as_str()],
            )
            .expect("corrupt rooted SQLite checkpoint manifest"),
            1
        );
        true
    }
}

#[tokio::test]
async fn sqlite_session_delete_blob_reclaim_conformance() {
    lash_core::testing::conformance::session_delete_blob_reclaim_conformance("sqlite", || {
        let dir = Arc::new(tempfile::tempdir().expect("tempdir"));
        let path = dir.path().join("durable-core.db");
        let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()));
        let probe = Arc::new(SqliteSessionDeleteBlobProbe { _dir: dir, path });
        lash_core::testing::conformance::SessionDeleteBlobHandles {
            factory: factory as Arc<dyn SessionStoreFactory>,
            probe,
        }
    })
    .await;
}
