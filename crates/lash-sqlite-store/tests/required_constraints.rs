// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

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
