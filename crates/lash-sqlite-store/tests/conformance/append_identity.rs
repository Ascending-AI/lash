lash_conformance::append_receipt_identity_corruption_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let mutation = backend.clone();
    (
        backend,
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
