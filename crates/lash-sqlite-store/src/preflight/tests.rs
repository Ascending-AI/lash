//! The preflight surface exists because opening the store answers the schema
//! question only by performing most of the open. Each test here first pins the
//! open path's side effect, then shows the preflight answering the same
//! question without it — the characterization is half the evidence, so it is
//! asserted rather than described.

use std::time::Duration;

use lash_core::{StorePreflight, StoreSchemaOutcome, StoreSchemaVerdict};

use super::{SqliteDatabase, SqliteStorePreflight, verify_schema_at};
use crate::Store;

fn temp_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

#[tokio::test]
async fn open_creates_a_missing_database_and_preflight_does_not() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");

    // Red side: today's only way to ask "will this open?" is to open, and the
    // open path carries `SQLITE_OPEN_CREATE`.
    assert!(!path.exists());
    let store = Store::open(&path).await.expect("open creates the database");
    drop(store);
    assert!(
        path.exists(),
        "the open path provisions the database it was asked about"
    );

    // Green side: the same question over a path that does not exist, answered
    // without bringing one into existence.
    let absent = root.path().join("not-here.db");
    let preflight = SqliteStorePreflight::for_durable_core(&absent);
    let status = preflight.schema_status().await.expect("read schema status");
    assert_eq!(status.databases.len(), 1);
    assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Absent);
    assert!(
        !absent.exists(),
        "preflight must not create the database it reports as absent"
    );
    assert_eq!(
        status.outcome(),
        StoreSchemaOutcome::Ready,
        "an unprovisioned deployment has nothing that refuses and nothing undecided"
    );
}

#[tokio::test]
async fn preflight_reports_a_rewound_version_that_open_would_refuse() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    Store::open(&path).await.expect("provision the database");
    let expected = SqliteDatabase::DurableCore.expected_version();

    rewind_user_version(&path, expected - 1);

    let found = verify_schema_at(&path, SqliteDatabase::DurableCore).await;
    assert_eq!(
        found.verdict,
        StoreSchemaVerdict::Mismatch {
            found: expected - 1
        }
    );
    assert_eq!(found.expected, expected);
    assert!(found.verdict.refuses_open());

    // The verdict is the refusal the open would have produced, reached without
    // producing it.
    let refusal = match Store::open(&path).await {
        Ok(_) => panic!("a rewound database must be refused at open"),
        Err(err) => err.to_string(),
    };
    assert!(refusal.contains(&(expected - 1).to_string()), "{refusal}");
    assert!(refusal.contains(&expected.to_string()), "{refusal}");
}

#[tokio::test]
async fn preflight_answers_while_another_connection_holds_the_write_lock() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    Store::open(&path).await.expect("provision the database");
    let expected = SqliteDatabase::DurableCore.expected_version();
    rewind_user_version(&path, expected - 1);

    let holder = rusqlite::Connection::open(&path).expect("open holder connection");
    holder
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("hold the write lock");

    // Red side: the open path takes `BEGIN IMMEDIATE` before it reads
    // `user_version`, so with the write lock held it cannot even reach the
    // question. It blocks on the busy handler instead of reporting the version.
    let blocked = tokio::time::timeout(Duration::from_secs(2), async {
        Store::open(&path)
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    })
    .await;
    assert!(
        blocked.is_err(),
        "open must still be waiting for the write lock, not answering: {blocked:?}"
    );

    // Green side: the read-only path takes a shared lock, so the same question
    // is answered under the same contention.
    let answered = tokio::time::timeout(
        Duration::from_secs(5),
        verify_schema_at(&path, SqliteDatabase::DurableCore),
    )
    .await
    .expect("preflight answers while the write lock is held");
    assert_eq!(
        answered.verdict,
        StoreSchemaVerdict::Mismatch {
            found: expected - 1
        }
    );

    holder.execute_batch("ROLLBACK").expect("release the lock");
}

#[tokio::test]
async fn every_declared_database_is_reported_and_undeclared_ones_are_not() {
    let root = temp_root();
    let core = root.path().join("durable-core.db");
    let registry = root.path().join("processes.db");
    Store::open(&core).await.expect("provision durable core");

    let status = SqliteStorePreflight::for_session_store_root(root.path())
        .with_process_registry(&registry)
        .schema_status()
        .await
        .expect("read schema status");

    let names: Vec<&str> = status
        .databases
        .iter()
        .map(|database| database.name.as_str())
        .collect();
    assert_eq!(names, vec!["durable core", "process registry"]);
    assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Matches);
    assert_eq!(status.databases[1].verdict, StoreSchemaVerdict::Absent);
    assert!(
        !registry.exists(),
        "reading a declared but unprovisioned database must not provision it"
    );
}

#[tokio::test]
async fn a_file_that_is_not_a_database_is_undecided_rather_than_refused() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    std::fs::write(&path, b"this is not a SQLite database").expect("write junk");

    let found = verify_schema_at(&path, SqliteDatabase::DurableCore).await;
    match &found.verdict {
        StoreSchemaVerdict::Unreadable { reason } => assert!(!reason.is_empty()),
        other => panic!("expected an undecided verdict, got {other:?}"),
    }
    assert!(
        !found.verdict.refuses_open(),
        "an unreadable database is undecided; a refusal needs a version to name"
    );
}

#[tokio::test]
async fn reading_a_hot_wal_database_leaves_its_bytes_untouched() {
    // The deleted read-write fallback made this false: such a connection
    // checkpoints a hot WAL and deletes it on close, which rewrites the main
    // file. The invariant is byte equality of the database itself, asserted
    // rather than described.
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    let store = Store::open(&path).await.expect("provision the database");
    // Leave the WAL hot: a live writer that has not checkpointed is precisely
    // the state a boot-time probe finds.
    store
        .conn
        .call(|c| {
            c.execute_batch("CREATE TABLE lash_preflight_probe (id INTEGER PRIMARY KEY)")?;
            Ok(())
        })
        .await
        .expect("write without checkpointing");
    assert!(
        path.with_extension("db-wal").exists(),
        "the test needs a hot WAL to be meaningful"
    );

    let before = std::fs::read(&path).expect("read the database before");
    let found = verify_schema_at(&path, SqliteDatabase::DurableCore).await;
    let after = std::fs::read(&path).expect("read the database after");

    assert_eq!(found.verdict, StoreSchemaVerdict::Matches);
    assert_eq!(
        before, after,
        "a preflight read must not rewrite the database it inspected"
    );
    assert!(
        path.with_extension("db-wal").exists(),
        "a preflight read must not checkpoint away the write-ahead log"
    );
}

#[tokio::test]
async fn a_preflight_connection_refuses_to_write_even_if_asked() {
    // `PRAGMA query_only` is the enforced form of the module's promise: the
    // engine rejects a write on this connection, so the guarantee does not
    // depend on which statements this module happens to send.
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    Store::open(&path).await.expect("provision the database");

    let conn = crate::conn::SqliteConnection::open_readonly(&path)
        .await
        .expect("open read-only");
    let refusal = conn
        .call(|c| {
            c.pragma_update(None, "query_only", true)?;
            c.execute_batch("CREATE TABLE lash_should_not_exist (id INTEGER)")
        })
        .await
        .expect_err("a query_only connection must refuse a write");
    assert!(
        refusal.to_string().to_lowercase().contains("readonly"),
        "{refusal}"
    );
}

#[test]
fn every_database_kind_publishes_the_version_its_open_enforces() {
    assert_eq!(
        SqliteDatabase::DurableCore.expected_version(),
        i64::from(crate::schema::SCHEMA_VERSION)
    );
    assert_eq!(
        SqliteDatabase::ProcessRegistry.expected_version(),
        i64::from(crate::schema::PROCESS_SCHEMA_VERSION)
    );
    assert_eq!(
        SqliteDatabase::Triggers.expected_version(),
        i64::from(crate::schema::TRIGGER_SCHEMA_VERSION)
    );
    assert_eq!(
        SqliteDatabase::EffectReplay.expected_version(),
        i64::from(crate::schema::EFFECT_SCHEMA_VERSION)
    );
}

fn rewind_user_version(path: &std::path::Path, version: i64) {
    let conn = rusqlite::Connection::open(path).expect("open for rewind");
    conn.pragma_update(None, "user_version", version)
        .expect("rewind user_version");
}
