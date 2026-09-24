use super::*;

lash_conformance::session_delete_blob_reclaim_tests!({
    ((), "sqlite", || {
        let backend = TestBackend::blocking(SUBSTRATE);
        let factory = backend.session_store_factory();
        let attachments = backend.attachment_store();
        let probe = Arc::new(crate::blob_probe::SqliteBlobProbe::new(
            backend.database_uri(SqliteDatabase::DurableCore),
            "fail_session_blob_delete",
            Some(Arc::new(backend)),
        ));
        lash_conformance::SessionDeleteBlobHandles {
            factory: factory as Arc<dyn SessionStoreFactory>,
            probe,
            attachments,
        }
    })
});
