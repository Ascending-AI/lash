use lash_sqlite_store::{SESSION_SCHEMA_VERSION, Store};

const RETAINED_PRIOR_DURABLE_CORE_GENERATION: i32 = 49;

#[tokio::test]
async fn sqlite_retained_prior_durable_core_is_refused_at_open() {
    assert_eq!(
        RETAINED_PRIOR_DURABLE_CORE_GENERATION + 1,
        SESSION_SCHEMA_VERSION
    );
    let dir = tempfile::tempdir().expect("SQLite predecessor-refusal tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        Store::open(&path)
            .await
            .expect("create current SQLite catalog"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open SQLite predecessor fixture");
    connection
        .pragma_update(None, "user_version", RETAINED_PRIOR_DURABLE_CORE_GENERATION)
        .expect("stamp retained SQLite durable-core predecessor");
    drop(connection);

    let error = Store::open(&path)
        .await
        .err()
        .expect("the retained SQLite durable-core predecessor must be refused at open")
        .to_string();
    assert!(
        error.contains(&format!("supports schema version {SESSION_SCHEMA_VERSION}"))
            && error.contains(&format!(
                "database reports version {RETAINED_PRIOR_DURABLE_CORE_GENERATION}"
            )),
        "the predecessor refusal must identify expected and found versions: {error}"
    );
}

#[tokio::test]
async fn sqlite_41_graph_sequence_shape_is_rejected_without_migration() {
    let dir = tempfile::tempdir().expect("SQLite graph-sequence cutover tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        Store::open(&path)
            .await
            .expect("create current SQLite catalog"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open SQLite 41 fixture");
    connection
        .execute("ALTER TABLE graph_nodes ADD COLUMN seq INTEGER", [])
        .expect("restore the version-40 graph sequence column");
    connection
        .execute(
            "CREATE INDEX idx_graph_nodes_session_seq ON graph_nodes(session_id, seq)",
            [],
        )
        .expect("restore the version-40 graph sequence index");
    connection
        .pragma_update(None, "user_version", 41)
        .expect("stamp SQLite durable-core 41");
    drop(connection);

    let error = Store::open(&path)
        .await
        .err()
        .expect("version-41 graph shape must be rejected")
        .to_string();
    assert_eq!(
        error,
        "Error(\"Unsupported lash durable core schema: this binary supports schema version 50, but the database reports version 41. There is no migration chain — delete the durable core database and start fresh.\")"
    );
    let connection = rusqlite::Connection::open(&path).expect("inspect refused SQLite catalog");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .expect("read refused SQLite version"),
        41,
        "the rejected open must not relabel the old graph shape"
    );
}
