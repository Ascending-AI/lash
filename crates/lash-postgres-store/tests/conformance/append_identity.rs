lash_conformance::append_receipt_identity_corruption_tests!({
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres corrupt receipt test: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    storage.store().admit_session(&lash_core_execution::testing::store_fixtures::root_session_request(
        &SessionId::from("root"),
    )).await.expect("admit corrupt-receipt root");
    let pool = storage.pool().clone();
    (
        _database_lock,
        Arc::new(storage.store()) as Arc<dyn RuntimeStore>,
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
});
