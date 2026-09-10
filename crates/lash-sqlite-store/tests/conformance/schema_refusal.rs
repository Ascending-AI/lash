use super::{SqliteProcessRegistry, SqliteTriggerStore};

#[tokio::test]
async fn sqlite_process_registry_rejects_pre_unit_external_owner_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-unit-external-owner-processes.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy process db");
    conn.pragma_update(None, "user_version", 12)
        .expect("stamp legacy process schema");
    drop(conn);

    let error = match SqliteProcessRegistry::open(&path, dir.path().join("sessions")).await {
        Ok(_) => panic!("pre-unit-external-owner process stores must be recreated"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash process registry schema"));
    assert!(message.contains("supports schema version 32"));
    assert!(message.contains("database reports version 12"));
    assert!(message.contains(
        "drain affected sessions and recreate the whole Lash trust domain with this version. Reset the tombstones, await-event revocation ledger, effect journal, and Restate state together; see docs/adr/0049-session-ids-are-used-once.md."
    ));
}

#[tokio::test]
async fn sqlite_trigger_store_rejects_pre_keyed_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-keyed-triggers.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy trigger db");
    conn.pragma_update(None, "user_version", 1)
        .expect("stamp legacy trigger schema");
    drop(conn);

    let error = match SqliteTriggerStore::open(&path).await {
        Ok(_) => panic!("pre-keyed trigger stores must be recreated"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash trigger store schema"));
    assert!(message.contains("supports schema version 8"));
    assert!(message.contains("database reports version 1"));
    assert!(message.contains(
        "drain affected sessions and recreate the whole Lash trust domain with this version. Reset the tombstones, await-event revocation ledger, effect journal, and Restate state together; see docs/adr/0049-session-ids-are-used-once.md."
    ));
}
