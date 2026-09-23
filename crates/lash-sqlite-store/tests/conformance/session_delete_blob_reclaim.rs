use super::*;

lash_conformance::session_delete_blob_reclaim_tests!({
    ((), "sqlite", || {
        let deployment = TestDeployment::blocking(SUBSTRATE);
        let factory = deployment.session_store_factory();
        let probe = Arc::new(crate::blob_probe::SqliteBlobProbe::new(
            deployment.database_uri(SqliteDatabase::DurableCore),
            "fail_session_blob_delete",
            Some(Arc::new(deployment)),
        ));
        lash_conformance::SessionDeleteBlobHandles {
            factory: factory as Arc<dyn SessionStoreFactory>,
            probe,
        }
    })
});
