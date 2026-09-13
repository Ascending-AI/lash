use super::*;

lash_conformance::session_delete_blob_reclaim_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres session-delete blob conformance: database is not configured");
        return;
    };
    let storage = Arc::new(storage);
    let make_storage = Arc::clone(&storage);
    (database_lock, "postgres", move || {
        let storage = Arc::clone(&make_storage);
        sync_await(async move {
            reset(&storage).await;
            lash_conformance::SessionDeleteBlobHandles {
                factory: Arc::new(storage.session_store_factory()) as Arc<dyn SessionStoreFactory>,
                probe: Arc::new(crate::blob_probe::PostgresBlobProbe::new(
                    storage,
                    "fail_session_blob_delete",
                )) as Arc<dyn lash_conformance::SessionDeleteBlobProbe>,
            }
        })
    })
});
