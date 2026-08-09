use super::*;

#[test]
fn sqlite_busy_and_locked_errors_are_typed_as_contention() {
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
    ] {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("synthetic contention".to_string()),
        );
        assert!(matches!(sqlite_error(error), StoreError::Contended));
    }
}

#[test]
fn sqlite_graph_generation_uniqueness_is_typed() {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory SQLite");
    conn.execute_batch(
        "CREATE TABLE graph_nodes (
             node_id TEXT NOT NULL UNIQUE,
             session_id TEXT NOT NULL,
             generation INTEGER NOT NULL,
             UNIQUE(session_id, generation)
         );
         INSERT INTO graph_nodes VALUES ('node-a', 'session-a', 3);",
    )
    .expect("seed graph uniqueness fixture");
    let raw = conn
        .execute(
            "INSERT INTO graph_nodes VALUES ('node-b', 'session-a', 3)",
            [],
        )
        .expect_err("duplicate generation must violate SQLite uniqueness");
    let error = sqlite_graph_node_insert_error(raw, "session-a", 3, "node-b");
    assert!(matches!(
        error,
        StoreError::GraphGenerationCollision {
            ref session_id,
            generation: 3
        } if session_id == "session-a"
    ));
}
