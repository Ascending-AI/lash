use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::{
    EffectHost, ProcessContinuationStore, ProcessExecutionEnvStore, ProcessRegistry,
    RuntimePersistence, SessionStoreFactory, TriggerStore,
};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteProcessRegistry, SqliteSessionStoreFactory, SqliteTriggerStore, Store,
};
use serde::{Deserialize, Serialize};

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

const REGENERATE_ENV: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SqliteVersions {
    durable_core: i32,
    processes: i32,
    triggers: i32,
    effects: i32,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_durable_fixture_reads_with_identical_semantics() {
    let fixture_dir = fixture_dir();
    let recorded: SqliteVersions = serde_json::from_slice(
        &std::fs::read(fixture_dir.join("versions.json"))
            .expect("read committed SQLite durable-fixture versions"),
    )
    .expect("decode committed SQLite durable-fixture versions");
    let current = current_versions().await;
    assert_eq!(
        recorded, current,
        "declared SQLite durable schema versions changed without fixture regeneration; run \
         LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p lash-sqlite-store --test \
         durable_read_fixture regenerate_sqlite_durable_fixture -- --ignored --exact"
    );

    let temp = tempfile::tempdir().expect("SQLite fixture tempdir");
    copy_sqlite_fixture(&fixture_dir, temp.path());
    let handles = open_handles(temp.path(), fixture::FIXTURE_READ_MS).await;
    let expected: fixture::ExpectedFixture = serde_json::from_slice(
        &std::fs::read(fixture_dir.join("expected.json"))
            .expect("read committed SQLite durable-fixture expectations"),
    )
    .expect("decode committed SQLite durable-fixture expectations");
    fixture::assert_semantics(&handles, &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_v32_session_relation_is_refused_before_row_decode() {
    let fixture_dir = fixture_dir();
    let temp = tempfile::tempdir().expect("SQLite fixture tempdir");
    copy_sqlite_fixture(&fixture_dir, temp.path());
    let durable_core = temp.path().join("durable-core.db");
    let connection = rusqlite::Connection::open(&durable_core).expect("open copied v32 fixture");
    let updated = connection
        .execute(
            "UPDATE session_meta
             SET relation_kind = ?1
             WHERE session_id = 'durable-read-fixture'",
            ["legacy"],
        )
        .expect("inject an unknown denormalized relation kind");
    assert_eq!(updated, 1, "the v32 proof must mutate its fixture row");
    connection
        .pragma_update(None, "user_version", 32)
        .expect("stamp the pre-denormalization v32 fixture");
    drop(connection);

    let open_error = match Store::open(&durable_core).await {
        Err(error) => error,
        Ok(store) => {
            let decode_error = store
                .load_session_meta()
                .await
                .expect_err("the unknown relation kind must fail strict row decoding");
            panic!(
                "SQLite v32 opened before failing later as stored-data corruption: {decode_error}"
            );
        }
    };
    let message = open_error.to_string();
    assert!(
        message.contains("supports schema version 36"),
        "open refusal must name the current reject-and-recreate boundary: {message}"
    );
    assert!(
        message.contains("reports version 32"),
        "open refusal must name the stale v32 fixture: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_prior_component_encoding_fixture_is_refused_at_hydration() {
    let temp = tempfile::tempdir().expect("SQLite refusal fixture tempdir");
    let database = temp.path().join("durable-core.db");
    std::fs::copy(
        prior_component_fixture_dir().join("durable-core.db"),
        &database,
    )
    .expect("copy committed SQLite component-version refusal fixture");
    assert_eq!(user_version(&database), 36);
    let store = Store::open(&database)
        .await
        .expect("open SQLite component-version refusal fixture");
    fixture::assert_prior_component_encoding_is_refused(&store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "writes the committed golden fixture; set LASH_REGENERATE_DURABLE_READ_FIXTURES=1"]
async fn regenerate_sqlite_durable_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge replacing the committed SQLite fixture"
    );
    let temp = tempfile::tempdir().expect("SQLite generator tempdir");
    let handles = open_handles(temp.path(), fixture::FIXTURE_WRITE_MS).await;
    let expected = fixture::seed(&handles).await;
    drop(handles);
    checkpoint_files(temp.path());

    let destination = fixture_dir();
    std::fs::create_dir_all(&destination).expect("create SQLite fixture directory");
    for name in database_names() {
        std::fs::copy(temp.path().join(name), destination.join(name))
            .unwrap_or_else(|error| panic!("copy generated SQLite fixture {name}: {error}"));
    }
    std::fs::write(
        destination.join("expected.json"),
        json_with_newline(&expected),
    )
    .expect("write SQLite fixture expectations");
    std::fs::write(
        destination.join("versions.json"),
        json_with_newline(&versions_at(temp.path())),
    )
    .expect("write SQLite fixture versions");
}

async fn open_handles(root: &Path, timestamp_ms: u64) -> fixture::FixtureHandles {
    std::fs::create_dir_all(root).expect("create SQLite fixture root");
    let clock = Arc::new(lash_core::testing::TestClock::new(timestamp_ms));
    let runtime = Arc::new(
        Store::open_with_clock(
            &root.join("durable-core.db"),
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
        )
        .await
        .expect("open SQLite durable-core fixture")
        .with_commit_count_seed_for_testing(0),
    );
    let processes = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &root.join("processes.db"),
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
            root.join("process-sessions"),
        )
        .await
        .expect("open SQLite process fixture"),
    );
    let triggers = Arc::new(
        SqliteTriggerStore::open_with_clock(
            &root.join("triggers.db"),
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
        )
        .await
        .expect("open SQLite trigger fixture")
        .with_incarnation_for_testing("durable-read-trigger-incarnation"),
    );
    let effect_path = root.join("effects.db");
    let priming_effects = SqliteEffectHost::open_with_clock(
        &effect_path,
        Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
    )
    .await
    .expect("prime SQLite effect fixture schema");
    drop(priming_effects);
    rusqlite::Connection::open(&effect_path)
        .expect("open SQLite effect fixture for deterministic secret")
        .execute(
            "UPDATE await_event_meta SET signing_secret = ?1 WHERE singleton = 1",
            rusqlite::params![vec![0x88_u8; 32]],
        )
        .expect("install deterministic SQLite await-event signing secret");
    let effects = Arc::new(
        SqliteEffectHost::open_with_clock(
            &effect_path,
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
        )
        .await
        .expect("open SQLite effect fixture"),
    );
    let session_factory = Arc::new(
        SqliteSessionStoreFactory::new(root)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );
    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
        runtime: Arc::clone(&runtime) as Arc<dyn RuntimePersistence>,
        session_factory: session_factory as Arc<dyn SessionStoreFactory>,
        processes: Arc::clone(&processes) as Arc<dyn ProcessRegistry>,
        continuations: processes as Arc<dyn ProcessContinuationStore>,
        process_envs: runtime as Arc<dyn ProcessExecutionEnvStore>,
        triggers: triggers as Arc<dyn TriggerStore>,
        effects: effects as Arc<dyn EffectHost>,
    }
}

async fn current_versions() -> SqliteVersions {
    let temp = tempfile::tempdir().expect("SQLite current-version tempdir");
    let handles = open_handles(temp.path(), fixture::FIXTURE_READ_MS).await;
    drop(handles);
    versions_at(temp.path())
}

fn versions_at(root: &Path) -> SqliteVersions {
    SqliteVersions {
        durable_core: user_version(&root.join("durable-core.db")),
        processes: user_version(&root.join("processes.db")),
        triggers: user_version(&root.join("triggers.db")),
        effects: user_version(&root.join("effects.db")),
    }
}

fn user_version(path: &Path) -> i32 {
    rusqlite::Connection::open(path)
        .unwrap_or_else(|error| panic!("open {} for schema version: {error}", path.display()))
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or_else(|error| panic!("read {} schema version: {error}", path.display()))
}

fn checkpoint_files(root: &Path) {
    for name in database_names() {
        let path = root.join(name);
        let connection = rusqlite::Connection::open(&path)
            .unwrap_or_else(|error| panic!("open {} for WAL checkpoint: {error}", path.display()));
        let busy: i64 = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .unwrap_or_else(|error| panic!("checkpoint {}: {error}", path.display()));
        assert_eq!(
            busy,
            0,
            "WAL checkpoint remained busy for {}",
            path.display()
        );
        drop(connection);
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        assert!(
            !wal.exists(),
            "WAL file still exists after TRUNCATE checkpoint: {}",
            wal.display()
        );
    }
}

fn copy_sqlite_fixture(from: &Path, to: &Path) {
    for name in database_names() {
        std::fs::copy(from.join(name), to.join(name))
            .unwrap_or_else(|error| panic!("copy committed SQLite fixture {name}: {error}"));
    }
}

fn database_names() -> [&'static str; 4] {
    [
        "durable-core.db",
        "processes.db",
        "triggers.db",
        "effects.db",
    ]
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/durable-read/v1/sqlite")
}

fn prior_component_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/checkpoint-component-v1-refusal/sqlite")
}

fn json_with_newline(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("encode SQLite fixture JSON");
    bytes.push(b'\n');
    bytes
}
