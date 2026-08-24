use lash_sqlite_store::Store;

#[tokio::test]
async fn sqlite_39_to_41_adds_turn_cancel_requests_and_session_state_marker() {
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
        .execute(
            "ALTER TABLE session_meta DROP COLUMN session_state_version",
            [],
        )
        .expect("remove the version-41 marker");
    connection
        .pragma_update(None, "user_version", 39)
        .expect("stamp SQLite durable-core 39");
    drop(connection);

    drop(Store::open(&path).await.expect("migrate SQLite 39 to 41"));
    let connection = rusqlite::Connection::open(&path).expect("inspect migrated SQLite catalog");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .expect("read migrated SQLite version"),
        41
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
    assert!(column_exists(
        &connection,
        "session_meta",
        "session_state_version"
    ));
}

#[tokio::test]
async fn sqlite_40_to_41_adds_session_state_marker() {
    let dir = tempfile::tempdir().expect("SQLite state-version migration tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        Store::open(&path)
            .await
            .expect("create current SQLite catalog"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open SQLite 40 fixture");
    connection
        .execute(
            "ALTER TABLE session_meta DROP COLUMN session_state_version",
            [],
        )
        .expect("remove the version-41 marker");
    connection
        .pragma_update(None, "user_version", 40)
        .expect("stamp SQLite durable-core 40");
    drop(connection);

    drop(Store::open(&path).await.expect("migrate SQLite 40 to 41"));
    let connection = rusqlite::Connection::open(&path).expect("inspect migrated SQLite catalog");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .expect("read migrated SQLite version"),
        41
    );
    assert!(column_exists(
        &connection,
        "session_meta",
        "session_state_version"
    ));
}

fn column_exists(connection: &rusqlite::Connection, table: &str, column: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            rusqlite::params![table, column],
            |row| row.get(0),
        )
        .expect("inspect migrated SQLite column")
}
