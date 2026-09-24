use super::*;

lash_conformance::abandoned_attachment_recovery_tests!({
    let Some(database_url) = database_url() else {
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = super::attachment_bytes(&bytes_root);
    ((database_lock, bytes_root), move || {
        let database_url = database_url.clone();
        let make_bytes = Arc::clone(&make_bytes);
        async move {
            let reopen_url = database_url.clone();
            let storage = PostgresStorage::connect(&database_url)
                .await
                .expect("connect initial Postgres attachment recovery authority");
            reset(storage.pool()).await;
            (
                Arc::new(storage.session_store_factory()) as Arc<dyn SessionStoreFactory>,
                make_bytes,
                move || async move {
                    storage.pool().close().await;
                    let reopened = PostgresStorage::connect(&reopen_url)
                        .await
                        .expect("reconnect Postgres attachment recovery authority");
                    Arc::new(reopened.session_store_factory()) as Arc<dyn SessionStoreFactory>
                },
            )
        }
    })
});

lash_conformance::attachment_condemnation_recovery_tests!({
    let Some(database_url) = database_url() else {
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect initial Postgres attachment condemnation authority");
    reset(storage.pool()).await;
    let reopen_url = database_url.clone();
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = super::attachment_bytes(&bytes_root);
    (
        (database_lock, bytes_root),
        Arc::new(storage.session_store_factory()),
        make_bytes,
        move || async move {
            storage.pool().close().await;
            let reopened = PostgresStorage::connect(&reopen_url)
                .await
                .expect("reconnect Postgres attachment condemnation authority");
            Arc::new(reopened.session_store_factory()) as Arc<dyn SessionStoreFactory>
        },
    )
});
