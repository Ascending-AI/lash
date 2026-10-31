use rusqlite::{Connection, params};

/// ADR 0024: forks and admissions retain shared ancestors even after the
/// producer is deleted; a tombstone stops the retaining path.
#[test]
fn frame_retention_follows_forks_and_admissions_after_producer_deletion() {
    let conn = Connection::open_in_memory().expect("SQLite");
    for ddl in crate::schema::provisioning_statements() {
        conn.execute_batch(ddl).expect("schema");
    }
    conn.execute_batch(
        "INSERT INTO blobs(hash, content, encoding) VALUES (X'01', X'00', 0);
         INSERT INTO deleted_sessions(session_id, created_at_ms, head_revision, relation_kind)
             VALUES ('producer', 0, 1, 'root');
         INSERT INTO graph_nodes(session_id, node_id, parent_node_id, generation,
                                 frame_node_id, body_bytes, node_json, tombstoned) VALUES
             ('producer', 'frame', NULL, 0, 'frame', 0, '{}', 0),
             ('fork-one', 'leaf-one', 'frame', 1, 'frame', 0, '{}', 0),
             ('fork-two', 'leaf-two', 'frame', 1, 'frame', 0, '{}', 0),
             ('barrier', 'barrier', 'frame', 1, 'frame', 0, '{}', 1),
             ('barrier', 'past-barrier', 'barrier', 2, 'frame', 0, '{}', 0);
         INSERT INTO session_revisions(session_id, head_revision, leaf_node_id, config_ref) VALUES
             ('fork-one', 1, 'leaf-one', X'01'),
             ('barrier', 1, 'past-barrier', X'01');",
    )
    .expect("shared history and retained revisions");
    let retained = |owner: &str, node: &str| -> bool {
        conn.query_row(
            crate::session_sql::session_sql()
                .graph_sqlite
                .artifact_frame_is_retained
                .sql(),
            params![owner, node],
            |row| row.get(0),
        )
        .expect("frame retention")
    };
    assert!(retained("producer", "frame"));
    assert!(retained("fork-one", "leaf-one"));
    assert!(!retained("fork-two", "leaf-two"));
    assert!(
        !retained("fork-one", "frame"),
        "the target owner must match"
    );
    assert!(!retained("barrier", "barrier"));

    conn.execute(
        "DELETE FROM session_revisions WHERE session_id = 'fork-one'",
        [],
    )
    .expect("remove the fork's last retaining root");
    assert!(retained("barrier", "past-barrier"));
    assert!(
        !retained("producer", "frame"),
        "a tombstone breaks the remaining path"
    );

    conn.execute_batch(
        "INSERT INTO session_meta(session_id, relation_kind, retention_kind, admission_base_checkpoint_ref)
             VALUES ('fork-two', 'root', 'until_gc', X'01');",
    )
    .expect("an admission protects the other fork's nodes");
    assert!(retained("producer", "frame"));
    assert!(retained("fork-two", "leaf-two"));
    conn.execute(
        "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = 'leaf-two'",
        [],
    )
    .expect("retire the admitted descendant");
    assert!(!retained("producer", "frame"));
    assert!(!retained("fork-two", "leaf-two"));

    conn.execute_batch(
        "DELETE FROM session_meta WHERE session_id = 'fork-two';
         INSERT INTO session_revisions(session_id, head_revision, leaf_node_id, config_ref)
             VALUES ('pinned-fork', 1, 'frame', X'01');",
    )
    .expect("a retained revision directly pins the producer's frame");
    assert!(retained("producer", "frame"));
    conn.execute(
        "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = 'frame'",
        [],
    )
    .expect("retire the target");
    assert!(!retained("producer", "frame"));
}
