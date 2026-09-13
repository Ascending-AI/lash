use super::*;

lash_conformance::session_delete_blob_reclaim_tests!({
    ((), "sqlite", || {
        let dir = Arc::new(tempfile::tempdir().expect("tempdir"));
        let path = dir.path().join("durable-core.db");
        let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()));
        let probe = Arc::new(crate::blob_probe::SqliteBlobProbe::new(
            path,
            "fail_session_blob_delete",
            Some(dir),
        ));
        lash_conformance::SessionDeleteBlobHandles {
            factory: factory as Arc<dyn SessionStoreFactory>,
            probe,
        }
    })
});
