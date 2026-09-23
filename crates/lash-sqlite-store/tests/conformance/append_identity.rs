lash_conformance::append_receipt_identity_corruption_tests!({
    let deployment = TestDeployment::open(SUBSTRATE).await;
    let store = deployment.store().await;
    let mutation = deployment.clone();
    (
        deployment,
        store as Arc<dyn RuntimePersistence>,
        move || async move {
            let conn = mutation.raw(SqliteDatabase::DurableCore);
            conn.execute(
                "UPDATE runtime_turn_commits
                 SET identity_encoding_version = ?1
                 WHERE turn_id LIKE '%corrupt-identity-version%'
                   AND turn_id NOT LIKE '%corrupt-identity-version-seed%'",
                rusqlite::params![i64::from(u32::MAX) + 1],
            )
            .expect("install oversized SQLite append identity version");
        },
    )
});
