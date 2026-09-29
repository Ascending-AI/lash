use super::{SqliteProcessRegistry, SqliteTriggerStore};
use lash_core_execution::StoreSchemaVerdict;
use lash_core_execution::compat::CompatRefusal;
use lash_sqlite_store::{SqliteDatabase, SqliteStore, verify_schema_at};

#[tokio::test]
async fn sqlite_open_refusal_names_the_writing_release() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("stamp store"),
    );
    let conn = rusqlite::Connection::open(&path).expect("open stamp");
    conn.execute("UPDATE lash_compat SET version = 2, min_reader = 2", [])
        .expect("raise reader floor");
    conn.execute(
        "UPDATE release_stamp SET schema_versions = 'unreadable'",
        [],
    )
    .expect("leave the release readable while its version tuple is not");
    drop(conn);
    let error = SqliteStore::open_file_for_testing(&path)
        .await
        .err()
        .expect("incompatible store refuses open");
    assert!(
        error
            .to_string()
            .contains(&format!("Writing release: {}", env!("CARGO_PKG_VERSION"))),
        "{error}"
    );
}

#[tokio::test]
async fn sqlite_malformed_stamp_refusal_names_the_writing_release() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("stamp store"),
    );
    let conn = rusqlite::Connection::open(&path).expect("open stamp");
    conn.execute("UPDATE lash_compat SET component = 'wrong'", [])
        .expect("malform compatibility stamp");
    drop(conn);
    let error = SqliteStore::open_file_for_testing(&path)
        .await
        .err()
        .expect("malformed stamp refuses open");
    assert!(
        error
            .to_string()
            .contains(&format!("Writing release: {}", env!("CARGO_PKG_VERSION"))),
        "{error}"
    );
}

#[tokio::test]
async fn sqlite_process_registry_refuses_an_unstamped_populated_catalog_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unstamped-processes.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy process db");
    conn.execute("CREATE TABLE processes (id TEXT PRIMARY KEY)", [])
        .expect("leave a pre-stamp process table");
    drop(conn);

    let status = verify_schema_at(&path, SqliteDatabase::ProcessRegistry).await;
    assert_eq!(
        status.verdict,
        StoreSchemaVerdict::Refused {
            refusal: CompatRefusal::Unstamped {
                component: "sqlite-registry".to_owned(),
                writing_release: None,
            },
        }
    );
    assert!(
        SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .is_err()
    );
    let conn = rusqlite::Connection::open(&path).expect("inspect refused registry");
    let stamps: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'lash_compat'",
            [],
            |row| row.get(0),
        )
        .expect("inspect compatibility table");
    assert_eq!(stamps, 0);
}

#[tokio::test]
async fn sqlite_trigger_store_refuses_an_unstamped_populated_catalog_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unstamped-triggers.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy trigger db");
    conn.execute(
        "CREATE TABLE trigger_subscriptions (id TEXT PRIMARY KEY)",
        [],
    )
    .expect("leave a pre-stamp trigger table");
    drop(conn);

    let status = verify_schema_at(&path, SqliteDatabase::Triggers).await;
    assert_eq!(
        status.verdict,
        StoreSchemaVerdict::Refused {
            refusal: CompatRefusal::Unstamped {
                component: "sqlite-triggers".to_owned(),
                writing_release: None,
            },
        }
    );
    assert!(SqliteTriggerStore::open(&path).await.is_err());
    let conn = rusqlite::Connection::open(&path).expect("inspect refused trigger store");
    let stamps: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'lash_compat'",
            [],
            |row| row.get(0),
        )
        .expect("inspect compatibility table");
    assert_eq!(stamps, 0);
}
