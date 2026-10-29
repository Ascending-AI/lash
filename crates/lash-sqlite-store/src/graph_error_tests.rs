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

/// D12: UNIQUE diagnostics come from keys held by the failed insert's write
/// transaction, independent of optional prose; unrelated failures stay storage
/// failures even when the attempted keys are occupied.
#[test]
fn sqlite_graph_collisions_use_transactional_identity() {
    let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory SQLite");
    conn.execute_batch(crate::schema::SCHEMA).expect("schema");
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("claim the writer before inserting");
    let insert = crate::session_sql::session_sql().graph.insert.sql();
    tx.execute(
        insert,
        params!["session-a", "node-a", None::<&str>, 3, "frame", 2, "{}"],
    )
    .expect("seed graph uniqueness fixture");
    tx.execute(
        "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = 'node-a'",
        [],
    )
    .expect("retired rows still occupy unique keys");
    let session = SessionId::from("session-a");
    for message in [None, Some("unrelated diagnostic".to_owned())] {
        // Generation-only, global node-id-only, and both keys occupied.
        for (session_id, node_id, generation, generation_conflict) in [
            (session.clone(), "node-b", 3, true),
            (SessionId::from("session-b"), "node-a", 4, false),
            (session.clone(), "node-a", 3, true),
        ] {
            let raw = tx
                .execute(
                    insert,
                    params![
                        session_id.as_str(),
                        node_id,
                        None::<&str>,
                        generation,
                        "frame",
                        2,
                        "{}"
                    ],
                )
                .expect_err("occupied unique key must abort the insert");
            let rusqlite::Error::SqliteFailure(code, _) = raw else {
                panic!("expected SQLite constraint failure: {raw:?}");
            };
            assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE);
            let error = sqlite_graph_node_insert_error(
                &tx,
                rusqlite::Error::SqliteFailure(code, message.clone()),
                &session_id,
                generation,
                &lash_vocabulary::identity::NodeId::from(node_id),
            );
            if generation_conflict {
                assert!(
                    matches!(error, StoreError::GraphGenerationCollision {
                    session_id: ref actual, generation: 3
                } if actual == session_id),
                    "{error:?}"
                );
            } else {
                assert!(
                    matches!(error, StoreError::NodeIdCollision { ref node_id }
                    if node_id.as_str() == "node-a"),
                    "{error:?}"
                );
            }
        }
    }
    let raw = tx
        .execute(
            insert,
            params!["session-a", "node-a", None::<&str>, 3, "frame", -1, "{}"],
        )
        .expect_err("CHECK failure takes precedence over occupied keys");
    let error = sqlite_graph_node_insert_error(
        &tx,
        raw,
        &session,
        3,
        &lash_vocabulary::identity::NodeId::from("node-a"),
    );
    assert!(
        matches!(error, StoreError::StorageFailure { .. }),
        "{error:?}"
    );
    let error = sqlite_graph_node_insert_error(
        &tx,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
            None,
        ),
        &session,
        4,
        &lash_vocabulary::identity::NodeId::from("absent"),
    );
    assert!(
        matches!(error, StoreError::StorageFailure { .. }),
        "a UNIQUE error without either key occupied has no collision identity: {error:?}"
    );
}

/// FIG-7552: the real UNIQUE extended result code reaches the host fault.
#[test]
fn sqlite_constraint_preserves_extended_code_in_host_fault() {
    use lash_core_store::store::StoreFault;
    let conn = rusqlite::Connection::open_in_memory().expect("SQLite");
    conn.execute_batch(
        "CREATE TABLE codes (value TEXT UNIQUE); INSERT INTO codes VALUES ('same');",
    )
    .expect("constraint fixture");
    let raw = conn
        .execute("INSERT INTO codes VALUES ('same')", [])
        .expect_err("duplicate value");
    let error = sqlite_error(raw);
    let fault = StoreFault::of_store_error(&error).expect("host store fault");
    for through_engine in [false, true] {
        let durable = lash_durable::DurableError::from(
            lash_execution_contracts::module_artifacts::ArtifactStoreError::StoreFault(
                fault.clone(),
            ),
        );
        let forwarded = if through_engine {
            lash_execution_contracts::plugin_error::PluginError::from(
                lash_execution_contracts::process::linking::EnginePortError::from(durable),
            )
        } else {
            lash_execution_contracts::plugin_error::registry_error(durable)
        };
        let lash_execution_contracts::plugin_error::PluginError::StoreUnavailable {
            fault: forwarded,
        } = forwarded
        else {
            panic!("database fault lost its host classification: {forwarded:?}");
        };
        assert_eq!(forwarded.storage_code(), fault.storage_code());
    }
    let encoded = serde_json::to_value(&fault).expect("serialize host fault");
    // https://www.sqlite.org/rescode.html#constraint_unique defines 2067.
    assert_eq!(
        encoded["code"],
        serde_json::json!({
            "backend": "sqlite", "extended_code": 2067
        }),
        "{error:?}"
    );
    assert_eq!(
        serde_json::from_value::<StoreFault>(encoded).expect("decode fault"),
        fault
    );
}
