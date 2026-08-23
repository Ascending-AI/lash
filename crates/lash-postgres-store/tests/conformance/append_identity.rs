#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_append_receipt_refuses_negative_stored_identity_encoding_version_when_configured()
{
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres corrupt receipt test: database is not configured");
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    lash_core::testing::conformance::append_receipt_corrupt_identity_encoding_version_is_refused(
        Arc::new(storage.session_store("root")) as Arc<dyn RuntimePersistence>,
        move || async move {
            sqlx::query(
                "UPDATE lash_runtime_turn_commits
                 SET identity_encoding_version = -1
                 WHERE turn_id LIKE '%corrupt-identity-version%'
                   AND turn_id NOT LIKE '%corrupt-identity-version-seed%'",
            )
            .execute(&pool)
            .await
            .expect("install negative Postgres append identity version");
        },
    )
    .await;
}
