use lash_sqlite_store::{
    RequiredConstraintFinding, SqliteDatabase, Store, inspect_required_constraints_at,
};

fn sqlite_sidecar(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}-{suffix}", path.display()))
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
    use lash_core::store_backend_support::required_constraints::{
        SQLITE_EXPECTED_CONSTRAINTS, SqliteConstraintDatabase,
    };

    let components = SQLITE_EXPECTED_CONSTRAINTS
        .iter()
        .map(|constraint| {
            constraint.sqlite_database.unwrap_or_else(|| {
                panic!(
                    "{}.{} has no SQLite database component",
                    constraint.table, constraint.name
                )
            })
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        components,
        [
            SqliteConstraintDatabase::DurableCore,
            SqliteConstraintDatabase::ProcessRegistry,
            SqliteConstraintDatabase::Triggers,
            SqliteConstraintDatabase::EffectReplay,
        ]
        .into_iter()
        .collect()
    );
}

#[tokio::test]
async fn sqlite_inspection_is_read_only_and_tolerates_unrelated_additions() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.db");
    drop(Store::open(&path).await.expect("provision durable core"));
    rusqlite::Connection::open(&path)
        .expect("open mutation fixture")
        .execute_batch(
            "CREATE TABLE host_owned_extra (
                value TEXT CONSTRAINT ck_host_extra CHECK (value <> 'literal ) -- kept')
            );",
        )
        .expect("add unrelated table and check");
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
                && name == "ck_pending_turn_inputs_claim_id_token_all_or_none"
    )));

    let altered_path = directory.path().join("altered.db");
    rusqlite::Connection::open(&altered_path)
        .expect("open altered fixture")
        .execute_batch(
            "CREATE TABLE pending_turn_inputs (
                claim_id TEXT,
                claim_token TEXT,
                CONSTRAINT ck_pending_turn_inputs_claim_id_token_all_or_none
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
                && name == "ck_pending_turn_inputs_claim_id_token_all_or_none"
    )));

    let unrelated_path = directory.path().join("unrelated-grammar.db");
    rusqlite::Connection::open(&unrelated_path)
        .expect("open unrelated-grammar fixture")
        .execute_batch(
            "CREATE TABLE pending_turn_inputs (
                claim_id TEXT,
                claim_token TEXT,
                CONSTRAINT ck_pending_turn_inputs_claim_id_token_all_or_none
                    CHECK ((claim_id IS NULL AND claim_token IS NULL)
                        OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)),
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
            if name == "ck_pending_turn_inputs_claim_id_token_all_or_none"
    )));
}

#[tokio::test]
async fn fig2837_sqlite_quoted_identifiers_cannot_forge_a_named_check() {
    let directory = tempfile::tempdir().expect("tempdir");
    let bracket_path = directory.path().join("bracket.db");
    let bracket = rusqlite::Connection::open(&bracket_path).expect("open bracket fixture");
    bracket
        .execute_batch(
            "CREATE TABLE runtime_effect_replay (
                status TEXT,
                [CONSTRAINT ck_runtime_effect_replay_status
                    CHECK (status IN ('in_progress', 'completed', 'failed'))] TEXT
            );
            INSERT INTO runtime_effect_replay(status) VALUES ('invalid');",
        )
        .expect("a quoted column name does not constrain status");
    drop(bracket);

    let report = inspect_required_constraints_at(&bracket_path, SqliteDatabase::EffectReplay)
        .await
        .expect("inspect bracket-identifier fixture");
    assert!(report.findings().iter().any(|finding| matches!(
        finding,
        RequiredConstraintFinding::Missing { table, name, .. }
            if table == "runtime_effect_replay"
                && name == "ck_runtime_effect_replay_status"
    )));

    let quoted_keyword_path = directory.path().join("quoted-keyword.db");
    rusqlite::Connection::open(&quoted_keyword_path)
        .expect("open quoted-keyword fixture")
        .execute_batch(
            "CREATE TABLE runtime_effect_replay (
                status TEXT,
                \"constraint\" ck_runtime_effect_replay_status
                    CHECK (status IN ('in_progress', 'completed', 'failed'))
            );",
        )
        .expect("create a column named like the declaration keyword");
    let report =
        inspect_required_constraints_at(&quoted_keyword_path, SqliteDatabase::EffectReplay)
            .await
            .expect("inspect quoted-keyword fixture");
    assert!(report.findings().iter().any(|finding| matches!(
        finding,
        RequiredConstraintFinding::Missing { name, .. }
            if name == "ck_runtime_effect_replay_status"
    )));

    let genuine_path = directory.path().join("genuine.db");
    let genuine = rusqlite::Connection::open(&genuine_path).expect("open genuine fixture");
    genuine
        .execute_batch(
            "CREATE TABLE runtime_effect_replay (
                \"STATUS\" TEXT,
                CONSTRAINT \"CK_RUNTIME_EFFECT_REPLAY_STATUS\"
                    CHECK ([STATUS] IN ('in_progress', 'completed', 'failed'))
            );",
        )
        .expect("create genuinely quoted lowercase identifiers");
    assert!(
        genuine
            .execute(
                "INSERT INTO runtime_effect_replay(status) VALUES ('invalid')",
                [],
            )
            .is_err(),
        "the genuine named check must reject invalid status"
    );
    drop(genuine);
    let report = inspect_required_constraints_at(&genuine_path, SqliteDatabase::EffectReplay)
        .await
        .expect("inspect genuine quoted check");
    assert!(report.is_conformant(), "{report:?}");
}

#[tokio::test]
async fn fig2837_sqlite_inspection_preserves_durable_state_and_reads_live_wal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let checkpointed_path = directory.path().join("checkpointed.db");
    let checkpointed =
        rusqlite::Connection::open(&checkpointed_path).expect("open checkpointed fixture");
    checkpointed
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA user_version = 731;
             CREATE TABLE runtime_effect_replay (
                 status TEXT,
                 CONSTRAINT ck_runtime_effect_replay_status
                     CHECK (status IN ('in_progress', 'completed', 'failed'))
             );
             INSERT INTO runtime_effect_replay(status) VALUES ('completed');
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .expect("create and checkpoint fixture");
    drop(checkpointed);
    for suffix in ["wal", "shm"] {
        let sidecar = sqlite_sidecar(&checkpointed_path, suffix);
        if sidecar.exists() {
            std::fs::remove_file(sidecar).expect("remove recoverable checkpointed sidecar");
        }
    }
    let checkpointed_before = std::fs::read(&checkpointed_path).expect("read main database");
    let report = inspect_required_constraints_at(&checkpointed_path, SqliteDatabase::EffectReplay)
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
        731
    );
    assert_eq!(
        checkpointed
            .query_row("SELECT status FROM runtime_effect_replay", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("read stored row"),
        "completed"
    );
    drop(checkpointed);

    let live_path = directory.path().join("live-wal.db");
    let live = rusqlite::Connection::open(&live_path).expect("open live WAL fixture");
    live.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE runtime_effect_replay (
             status TEXT,
             CONSTRAINT ck_runtime_effect_replay_status
                 CHECK (status IN ('in_progress', 'completed', 'failed'))
         );
         INSERT INTO runtime_effect_replay(status) VALUES ('completed');",
    )
    .expect("commit schema and row to the live WAL");
    let wal_path = sqlite_sidecar(&live_path, "wal");
    assert!(wal_path.exists(), "fixture must retain a committed WAL");
    let main_before = std::fs::read(&live_path).expect("read live main database");
    let wal_before = std::fs::read(&wal_path).expect("read committed WAL");
    let report = inspect_required_constraints_at(&live_path, SqliteDatabase::EffectReplay)
        .await
        .expect("inspect schema committed only in WAL");
    assert!(report.is_conformant(), "{report:?}");
    assert_eq!(main_before, std::fs::read(&live_path).expect("reread main"));
    assert_eq!(wal_before, std::fs::read(&wal_path).expect("reread WAL"));
    assert_eq!(
        live.query_row("SELECT status FROM runtime_effect_replay", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("read row committed in WAL"),
        "completed"
    );
}
