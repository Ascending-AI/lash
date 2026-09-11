use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_abandoned_attachment_write_recovery_survives_cold_reopen() {
    let Some(database_url) = database_url() else {
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&database_url).await;
    for reclaimed in [false, true] {
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect initial Postgres attachment recovery authority");
        reset(&storage).await;
        let reopen_url = database_url.clone();
        lash_conformance::abandoned_attachment_write_recovery_after_cold_reopen(
            Arc::new(storage.session_store_factory()),
            reclaimed,
            move || async move {
                storage.pool().close().await;
                let reopened = PostgresStorage::connect(&reopen_url)
                    .await
                    .expect("reconnect Postgres attachment recovery authority");
                Arc::new(reopened.session_store_factory()) as Arc<dyn SessionStoreFactory>
            },
        )
        .await;
    }
    drop(database_lock);
}
