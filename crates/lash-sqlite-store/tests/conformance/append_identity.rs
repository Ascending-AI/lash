#[tokio::test]
async fn sqlite_append_receipt_refuses_oversized_stored_identity_encoding_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append-receipt-corrupt-version.db");
    let store = Arc::new(Store::open(&path).await.expect("open store"));
    lash_core::testing::conformance::append_receipt_corrupt_identity_encoding_version_is_refused(
        store as Arc<dyn RuntimePersistence>,
        move || async move {
            let conn = rusqlite::Connection::open(path).expect("open raw SQLite receipt fixture");
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
    .await;
}
