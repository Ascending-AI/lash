use lash_sqlite_store::Store;

#[tokio::test]
async fn sqlite_39_to_40_adds_durable_turn_cancel_requests() {
    let dir = tempfile::tempdir().expect("SQLite turn-cancel migration tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        Store::open(&path)
            .await
            .expect("create current SQLite catalog"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open SQLite 39 fixture");
    connection
        .execute("DROP TABLE turn_cancel_requests", [])
        .expect("remove the version-40 table");
    connection
        .pragma_update(None, "user_version", 39)
        .expect("stamp SQLite durable-core 39");
    drop(connection);

    drop(Store::open(&path).await.expect("migrate SQLite 39 to 40"));
    let connection = rusqlite::Connection::open(&path).expect("inspect migrated SQLite catalog");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .expect("read migrated SQLite version"),
        40
    );
    assert!(
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'turn_cancel_requests')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("read migrated turn-cancel table")
    );
}
