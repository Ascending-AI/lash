use std::path::PathBuf;
use std::sync::Arc;

pub(crate) struct SqliteBlobProbe {
    _guard: Option<Arc<tempfile::TempDir>>,
    path: PathBuf,
    fault_name: &'static str,
}

impl SqliteBlobProbe {
    pub(crate) fn new(
        path: PathBuf,
        fault_name: &'static str,
        guard: Option<Arc<tempfile::TempDir>>,
    ) -> Self {
        assert!(matches!(
            fault_name,
            "fail_session_blob_delete" | "fail_process_prune_blob_delete"
        ));
        Self {
            _guard: guard,
            path,
            fault_name,
        }
    }
}

#[async_trait::async_trait]
impl lash_conformance::SessionDeleteBlobProbe for SqliteBlobProbe {
    async fn blob_exists(&self, blob_ref: &lash_core::BlobRef) -> bool {
        rusqlite::Connection::open(&self.path)
            .expect("open SQLite blob probe")
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)",
                rusqlite::params![blob_ref.as_str()],
                |row| row.get(0),
            )
            .expect("query SQLite blob existence")
    }

    async fn fail_next_blob_delete(&self) {
        rusqlite::Connection::open(&self.path)
            .expect("open SQLite blob-delete fault")
            .execute_batch(&format!(
                "CREATE TRIGGER {} BEFORE DELETE ON blobs BEGIN \
                 SELECT RAISE(ABORT, 'injected blob delete failure'); END;",
                self.fault_name
            ))
            .expect("install SQLite blob-delete fault");
    }

    async fn clear_blob_delete_failure(&self) {
        rusqlite::Connection::open(&self.path)
            .expect("open SQLite blob-delete fault cleanup")
            .execute_batch(&format!("DROP TRIGGER {}", self.fault_name))
            .expect("remove SQLite blob-delete fault");
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
