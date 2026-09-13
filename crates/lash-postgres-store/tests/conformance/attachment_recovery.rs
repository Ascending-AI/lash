use super::*;

lash_conformance::abandoned_attachment_recovery_tests!({
    let Some(database_url) = database_url() else {
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&database_url).await;
    (database_lock, move |_reclaimed: bool| {
        let database_url = database_url.clone();
        async move {
            let reopen_url = database_url.clone();
            let storage = PostgresStorage::connect(&database_url)
                .await
                .expect("connect initial Postgres attachment recovery authority");
            reset(&storage).await;
            (
                Arc::new(storage.session_store_factory()) as Arc<dyn SessionStoreFactory>,
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
    reset(&storage).await;
    let reopen_url = database_url.clone();
    (
        database_lock,
        Arc::new(storage.session_store_factory()),
        move || async move {
            storage.pool().close().await;
            let reopened = PostgresStorage::connect(&reopen_url)
                .await
                .expect("reconnect Postgres attachment condemnation authority");
            Arc::new(reopened.session_store_factory()) as Arc<dyn SessionStoreFactory>
        },
    )
});
