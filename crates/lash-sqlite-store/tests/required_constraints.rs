// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_core_execution::StoreError;
use lash_sqlite_store::{
    RequiredConstraintFinding, SqliteDatabase, Store, inspect_required_constraints_at,
};

fn sqlite_sidecar(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}-{suffix}", path.display()))
}

fn wait_for_sqlite_connections_to_close(path: &std::path::Path) {
    let wal_path = sqlite_sidecar(path, "wal");
    let shm_path = sqlite_sidecar(path, "shm");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while wal_path.exists() || shm_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "SQLite fixture connections did not close"
        );
        std::thread::yield_now();
    }
}

#[tokio::test]
async fn fig2837_sqlite_missing_database_is_an_error_and_is_not_created() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("absent.db");
    let error = inspect_required_constraints_at(&path, SqliteDatabase::DurableCore)
        .await
        .expect_err("an absent database cannot produce a clean report");
    assert!(!path.exists(), "read-only inspection created the database");
    assert!(
        error.to_string().contains("sqlite"),
        "backend error must remain attributable: {error}"
    );
}

#[test]
fn fig2837_every_sqlite_registry_entry_names_an_inspectable_component() {
    use lash_core_execution::store_backend_support::required_constraints::{
        EXPECTED_CONSTRAINTS, SqliteConstraintDatabase,
    };

    let components = EXPECTED_CONSTRAINTS
        .iter()
        .flat_map(|constraint| constraint.sqlite_databases.iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        components,
        [
            SqliteConstraintDatabase::DurableCore,
            SqliteConstraintDatabase::ProcessRegistry,
            SqliteConstraintDatabase::Triggers,
        ]
        .into_iter()
        .collect()
    );
}

#[tokio::test]
async fn sqlite_inspection_is_read_only_and_tolerates_unrelated_additions() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.db");
    let store = Store::open(&path).await.expect("provision durable core");
    let fixture = rusqlite::Connection::open(&path).expect("open mutation fixture");
    fixture
        .execute_batch(
            "CREATE TABLE host_owned_extra (
                value TEXT CONSTRAINT ck_host_extra CHECK (value <> 'literal ) -- kept')
            );",
        )
        .expect("add unrelated table and check");
    let (busy, wal_frames, checkpointed_frames) = fixture
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .expect("checkpoint mutation fixture");
    assert_eq!(busy, 0, "fixture checkpoint must not be blocked");
    assert_eq!(
        wal_frames, checkpointed_frames,
        "fixture checkpoint must copy every WAL frame"
    );
    drop(fixture);
    drop(store);
    // `tokio-rusqlite` drops its connection on a worker thread after the
    // handle is dropped. SQLite removes WAL sidecars when the final connection
    // closes, so their removal is the completion signal rather than a sleep.
    wait_for_sqlite_connections_to_close(&path);
    let before = std::fs::read(&path).expect("read database before inspection");

    let report = inspect_required_constraints_at(&path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect existing database");
    let after = std::fs::read(&path).expect("read database after inspection");

    assert!(report.is_conformant(), "{report:?}");
    assert_eq!(
        before, after,
        "inspection must not mutate the database file"
    );
}

#[tokio::test]
async fn sqlite_inspection_distinguishes_missing_and_altered_named_checks() {
    let directory = tempfile::tempdir().expect("tempdir");
    let missing_path = directory.path().join("missing.db");
    rusqlite::Connection::open(&missing_path)
        .expect("open missing fixture")
        .execute_batch("CREATE TABLE pending_turn_inputs (claim_id TEXT, claim_token TEXT);")
        .expect("create missing fixture");
    let missing = inspect_required_constraints_at(&missing_path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect missing fixture");
    assert!(missing.findings().iter().any(|finding| matches!(
        finding,
        RequiredConstraintFinding::Missing { table, name, .. }
            if table == "pending_turn_inputs"
                && name == "ck_pending_turn_inputs_claim_identity_all_or_none"
    )));

    let altered_path = directory.path().join("altered.db");
    rusqlite::Connection::open(&altered_path)
        .expect("open altered fixture")
        .execute_batch(
            "CREATE TABLE pending_turn_inputs (
                claim_id TEXT,
                claim_token TEXT,
                CONSTRAINT ck_pending_turn_inputs_claim_identity_all_or_none
                    CHECK (claim_id IS NULL OR claim_token IS NOT NULL)
            );",
        )
        .expect("create altered fixture");
    let altered = inspect_required_constraints_at(&altered_path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect altered fixture");
    assert!(altered.findings().iter().any(|finding| matches!(
        finding,
        RequiredConstraintFinding::Altered { table, name, .. }
            if table == "pending_turn_inputs"
                && name == "ck_pending_turn_inputs_claim_identity_all_or_none"
    )));

    let unrelated_path = directory.path().join("unrelated-grammar.db");
    rusqlite::Connection::open(&unrelated_path)
        .expect("open unrelated-grammar fixture")
        .execute_batch(
            "CREATE TABLE pending_turn_inputs (
                claim_id TEXT,
                claim_owner_id TEXT,
                claim_owner_incarnation_id TEXT,
                claim_token TEXT,
                CONSTRAINT ck_pending_turn_inputs_claim_identity_all_or_none
                    CHECK ((claim_id IS NULL AND claim_owner_id IS NULL
                            AND claim_owner_incarnation_id IS NULL AND claim_token IS NULL)
                        OR (claim_id IS NOT NULL AND claim_owner_id IS NOT NULL
                            AND claim_owner_incarnation_id IS NOT NULL AND claim_token IS NOT NULL)),
                CONSTRAINT ck_host_extra CHECK (printf('%q', claim_id) GLOB '*')
            );",
        )
        .expect("create unrelated-grammar fixture");
    let unrelated = inspect_required_constraints_at(&unrelated_path, SqliteDatabase::DurableCore)
        .await
        .expect("unsupported grammar in an unrelated check is ignored");
    assert!(!unrelated.findings().iter().any(|finding| matches!(
        finding,
        RequiredConstraintFinding::Altered { name, .. }
            if name == "ck_pending_turn_inputs_claim_identity_all_or_none"
    )));
}

#[tokio::test]
async fn fig2837_sqlite_quoted_identifiers_cannot_forge_a_named_check() {
    let directory = tempfile::tempdir().expect("tempdir");
    let bracket_path = directory.path().join("bracket.db");
    let bracket = rusqlite::Connection::open(&bracket_path).expect("open bracket fixture");
    bracket
        .execute_batch(
            "CREATE TABLE session_ingress_sequence (
                enqueue_seq INTEGER,
                [CONSTRAINT ck_session_ingress_sequence_positive
                    CHECK (enqueue_seq > 0)] TEXT
            );
            INSERT INTO session_ingress_sequence(enqueue_seq) VALUES (0);",
        )
        .expect("a quoted column name does not constrain the sequence");
    drop(bracket);

    let report = inspect_required_constraints_at(&bracket_path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect bracket-identifier fixture");
    assert!(report.findings().iter().any(|finding| matches!(
        finding,
        RequiredConstraintFinding::Missing { table, name, .. }
            if table == "session_ingress_sequence"
                && name == "ck_session_ingress_sequence_positive"
    )));

    let quoted_keyword_path = directory.path().join("quoted-keyword.db");
    rusqlite::Connection::open(&quoted_keyword_path)
        .expect("open quoted-keyword fixture")
        .execute_batch(
            "CREATE TABLE session_ingress_sequence (
                enqueue_seq INTEGER,
                \"constraint\" ck_session_ingress_sequence_positive
                    CHECK (enqueue_seq > 0)
            );",
        )
        .expect("create a column named like the declaration keyword");
    let report = inspect_required_constraints_at(&quoted_keyword_path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect quoted-keyword fixture");
    assert!(report.findings().iter().any(|finding| matches!(
        finding,
        RequiredConstraintFinding::Missing { name, .. }
            if name == "ck_session_ingress_sequence_positive"
    )));

    let genuine_path = directory.path().join("genuine.db");
    let genuine = rusqlite::Connection::open(&genuine_path).expect("open genuine fixture");
    genuine
        .execute_batch(
            "CREATE TABLE session_ingress_sequence (
                \"SESSION_ID\" TEXT NOT NULL PRIMARY KEY,
                \"ENQUEUE_SEQ\" INTEGER NOT NULL,
                CONSTRAINT \"CK_SESSION_INGRESS_SEQUENCE_POSITIVE\"
                    CHECK ([ENQUEUE_SEQ] > 0)
            );",
        )
        .expect("create genuinely quoted lowercase identifiers");
    // The custom table shadows the schema's own declaration; the rest of the
    // catalog comes straight out of the provisioning text so the
    // fragment-carried tables complete.
    for statement in
        lash_sqlite_store::testing::database_provisioning_statements(SqliteDatabase::DurableCore)
    {
        genuine
            .execute_batch(statement)
            .expect("apply the shared provisioning statements");
    }
    assert!(
        genuine
            .execute(
                "INSERT INTO session_ingress_sequence(session_id, enqueue_seq) VALUES ('s', 0)",
                [],
            )
            .is_err(),
        "the genuine named check must reject a non-positive sequence"
    );
    drop(genuine);
    let report = inspect_required_constraints_at(&genuine_path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect genuine quoted check");
    assert!(report.is_conformant(), "{report:?}");
}

#[tokio::test]
async fn fig2837_sqlite_virtual_table_arguments_cannot_forge_a_named_check() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("virtual.db");
    let connection = rusqlite::Connection::open(&path).expect("open virtual-table fixture");
    connection
        .execute_batch(
            "CREATE VIRTUAL TABLE session_ingress_sequence USING rtree(
                id, min, max,
                +enqueue_seq CONSTRAINT ck_session_ingress_sequence_positive
                    CHECK(enqueue_seq > 0)
            );
            INSERT INTO session_ingress_sequence VALUES (1, 0, 1, 0);",
        )
        .expect("rtree module arguments do not declare a table CHECK");
    drop(connection);

    let error = inspect_required_constraints_at(&path, SqliteDatabase::DurableCore)
        .await
        .expect_err("a virtual table cannot produce a conformant constraint report");
    assert!(matches!(
        error,
        StoreError::RequiredConstraintInspectionInconclusive {
            backend: "sqlite",
            table,
            constraint,
            detail,
        } if table == "session_ingress_sequence"
            && constraint == "ck_session_ingress_sequence_positive"
            && detail.contains("virtual tables")
    ));
}

#[tokio::test]
async fn fig2837_sqlite_inspection_preserves_durable_state_and_reads_live_wal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let checkpointed_path = directory.path().join("checkpointed.db");
    let checkpointed =
        rusqlite::Connection::open(&checkpointed_path).expect("open checkpointed fixture");
    checkpointed
        .execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA user_version = {};
             CREATE TABLE session_ingress_sequence (
                 session_id TEXT NOT NULL PRIMARY KEY,
                 enqueue_seq INTEGER NOT NULL,
                 CONSTRAINT ck_session_ingress_sequence_positive
                     CHECK (enqueue_seq > 0)
             );",
            SqliteDatabase::DurableCore.expected_version()
        ))
        .expect("create and checkpoint fixture");
    for statement in
        lash_sqlite_store::testing::database_provisioning_statements(SqliteDatabase::DurableCore)
    {
        checkpointed
            .execute_batch(statement)
            .expect("apply the shared provisioning statements");
    }
    checkpointed
        .execute_batch(
            "INSERT INTO session_ingress_sequence(session_id, enqueue_seq)
                 VALUES ('session', 1);",
        )
        .expect("seed a conforming ingress-sequence row");
    checkpointed
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint the completed fixture");
    drop(checkpointed);
    for suffix in ["wal", "shm"] {
        let sidecar = sqlite_sidecar(&checkpointed_path, suffix);
        if sidecar.exists() {
            std::fs::remove_file(sidecar).expect("remove recoverable checkpointed sidecar");
        }
    }
    let checkpointed_before = std::fs::read(&checkpointed_path).expect("read main database");
    let report = inspect_required_constraints_at(&checkpointed_path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect checkpointed WAL database");
    assert!(report.is_conformant(), "{report:?}");
    assert_eq!(
        checkpointed_before,
        std::fs::read(&checkpointed_path).expect("reread main database"),
        "inspection must not change durable main-database bytes"
    );
    let checkpointed = rusqlite::Connection::open_with_flags(
        &checkpointed_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("reopen checkpointed fixture read-only");
    assert_eq!(
        checkpointed
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read user_version"),
        SqliteDatabase::DurableCore.expected_version()
    );
    assert_eq!(
        checkpointed
            .query_row(
                "SELECT enqueue_seq FROM session_ingress_sequence",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .expect("read stored row"),
        1
    );
    drop(checkpointed);

    let live_path = directory.path().join("live-wal.db");
    let live = rusqlite::Connection::open(&live_path).expect("open live WAL fixture");
    live.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE session_ingress_sequence (
             session_id TEXT NOT NULL PRIMARY KEY,
             enqueue_seq INTEGER NOT NULL,
             CONSTRAINT ck_session_ingress_sequence_positive
                 CHECK (enqueue_seq > 0)
         );",
    )
    .expect("commit schema to the live WAL");
    for statement in
        lash_sqlite_store::testing::database_provisioning_statements(SqliteDatabase::DurableCore)
    {
        live.execute_batch(statement)
            .expect("apply the shared provisioning statements");
    }
    live.execute_batch(
        "INSERT INTO session_ingress_sequence(session_id, enqueue_seq)
             VALUES ('session', 1);",
    )
    .expect("commit a conforming ingress-sequence row to the WAL");
    let wal_path = sqlite_sidecar(&live_path, "wal");
    assert!(wal_path.exists(), "fixture must retain a committed WAL");
    let main_before = std::fs::read(&live_path).expect("read live main database");
    let wal_before = std::fs::read(&wal_path).expect("read committed WAL");
    let report = inspect_required_constraints_at(&live_path, SqliteDatabase::DurableCore)
        .await
        .expect("inspect schema committed only in WAL");
    assert!(report.is_conformant(), "{report:?}");
    assert_eq!(main_before, std::fs::read(&live_path).expect("reread main"));
    assert_eq!(wal_before, std::fs::read(&wal_path).expect("reread WAL"));
    assert_eq!(
        live.query_row(
            "SELECT enqueue_seq FROM session_ingress_sequence",
            [],
            |row| { row.get::<_, i64>(0) }
        )
        .expect("read row committed in WAL"),
        1
    );
}
