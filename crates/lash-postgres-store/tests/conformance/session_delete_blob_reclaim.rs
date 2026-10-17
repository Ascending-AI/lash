use super::*;

lash_conformance::session_delete_blob_reclaim_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres session-delete blob conformance: database is not configured");
        return;
    };
    let reopen_url = database_fixture.url().to_owned();
    let storage = Arc::new(storage);
    let make_storage = Arc::clone(&storage);
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = super::attachment_bytes(&bytes_root);
    ((database_fixture, bytes_root), "postgres", move || {
        let storage = Arc::clone(&make_storage);
        let reopen_url = reopen_url.clone();
        let attachments = make_bytes();
        sync_await(async move {
            reset(storage.pool()).await;
            lash_conformance::SessionDeleteBlobHandles {
                factory: Arc::new(storage.session_store_factory()) as Arc<dyn DeploymentStore>,
                reopen: Arc::new(move || {
                    let url = reopen_url.clone();
                    sync_await(async move {
                        let reopened = lash_postgres_store::testing::connect(&url)
                            .await
                            .expect("reopen the attachment retention catalog");
                        Arc::new(reopened.session_store_factory()) as Arc<dyn DeploymentStore>
                    })
                }),
                probe: Arc::new(crate::blob_probe::PostgresBlobProbe::new(
                    storage,
                    "fail_session_blob_delete",
                )) as Arc<dyn lash_conformance::SessionDeleteBlobProbe>,
                attachments,
            }
        })
    })
});
