#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::{
    EffectHost, ProcessContinuationStore, ProcessExecutionEnvStore, RuntimePersistence,
    SessionStoreFactory, TriggerStore,
};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteProcessRegistry, SqliteSessionStoreFactory, SqliteTriggerStore, Store,
};
use serde::{Deserialize, Serialize};

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

const REGENERATE_ENV: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES";
const LATEST_GENERATION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-75-63f3438c/sqlite-expected.json",
];
const BOUNDARY_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-80-fbbeedbb5/sqlite-expected.json",
];
const OUTGOING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-81-c938416cc8/sqlite-expected.json",
];
const RETIRING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-82-10ae0a31f/sqlite-expected.json",
];
const DEPARTING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-83-04b02aef6/sqlite-expected.json",
];
const FRESHEST_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-78-a9506225c8c1/sqlite-expected.json",
];
const CURRENT_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-79-171f490d0eb3/sqlite-expected.json",
];
const NEWEST_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-77-e102f2b9f861/sqlite-expected.json",
];
const LATEST_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-76-023dd85246b4/sqlite-expected.json",
];
const CURRENT_GENERATION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-74-6c82924d/sqlite-expected.json",
];
const FRESHEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-73-079ae4b4/sqlite-expected.json",
];
const NEWEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-72-d2676b45/sqlite-expected.json",
];
const LATEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-71-bb852f91/sqlite-expected.json",
];
const IMMEDIATE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-70-26d22e05/sqlite-expected.json",
];
const CURRENT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-69-330850ee/sqlite-expected.json",
];
const PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-68-9680a9bd/sqlite-expected.json",
];
const PREVIOUS_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-67-02339d79/sqlite-expected.json",
];
const HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-66-847ba3b0/sqlite-expected.json",
];
const OLDER_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-65-11f6b0eb/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-65-2bc03f0b/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-65-6a89236a/sqlite-expected.json",
];
const ANCIENT_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-64-41ad1609/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-64-dba005a2/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-64-25d7281b/sqlite-expected.json",
];
const EARLIER_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-fe2964c7/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-037d9999/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-1b2b8afc/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-75082e3d/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-8bbd7b94/sqlite-expected.json",
];
const EARLIEST_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-62-7861e438/sqlite-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-62-ee717fab/sqlite-expected.json",
];

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
         LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p lash-internal-sqlite-store --test \
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

/// The read-back test above proves old bytes still mean the same thing. This one
/// proves the write side has not drifted away from them: a payload-shape change
/// that never touches `fixtures/` passes the schema-declaration gate and decodes
/// the old artifact unchanged, so without this law it only surfaces when someone
/// else regenerates (FIG-1433).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_durable_fixture_expectations_match_what_this_build_writes() {
    let temp = tempfile::tempdir().expect("SQLite write-shape tempdir");
    let handles = open_handles(temp.path(), fixture::FIXTURE_WRITE_MS).await;
    let written_now = Box::pin(fixture::seed(&handles)).await;
    drop(handles);
    fixture::assert_committed_expectations_match_current_writes(
        &std::fs::read(fixture_dir().join("expected.json"))
            .expect("read committed SQLite durable-fixture expectations"),
        &json_with_newline(&written_now),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_v32_session_relation_is_refused_before_row_decode() {
    let fixture_dir = fixture_dir();
    let temp = tempfile::tempdir().expect("SQLite fixture tempdir");
    copy_sqlite_fixture(&fixture_dir, temp.path());
    let durable_core = temp.path().join("durable-core.db");
    let connection = rusqlite::Connection::open(&durable_core).expect("open copied v32 fixture");
    connection
        .pragma_update(None, "ignore_check_constraints", true)
        .expect("permit manufacturing a pre-CHECK malformed fixture");
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
        .pragma_update(None, "ignore_check_constraints", false)
        .expect("restore CHECK enforcement after manufacturing the stale fixture");
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
        message.contains("supports schema version 69"),
        "open refusal must name the current reject-and-recreate boundary: {message}"
    );
    assert!(
        message.contains("reports version 32"),
        "open refusal must name the stale v32 fixture: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_v38_component_fixture_is_refused_before_hydration() {
    let temp = tempfile::tempdir().expect("SQLite refusal fixture tempdir");
    let database = temp.path().join("durable-core.db");
    std::fs::copy(
        prior_component_fixture_dir().join("durable-core.db"),
        &database,
    )
    .expect("copy committed SQLite component-version refusal fixture");
    assert_eq!(user_version(&database), 38);
    let open_error = match Store::open(&database).await {
        Err(error) => error,
        Ok(_) => panic!("the v38 fixture must be rejected at the schema boundary"),
    };
    let message = open_error.to_string();
    assert!(
        message.contains("supports schema version 69"),
        "open refusal must name the current schema boundary: {message}"
    );
    assert!(
        message.contains("reports version 38"),
        "open refusal must name the stale v38 fixture: {message}"
    );
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
    let expected = Box::pin(fixture::seed(&handles)).await;
    drop(handles);
    pin_attachment_write_token(&temp.path().join("durable-core.db"));
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

/// Replace the random write token `begin_attachment_write` minted while seeding
/// with the fixture's fixed one.
///
/// The token is minted inside the store, so it cannot be handed in; it is
/// rewritten afterwards instead. The row must exist and must be the only one,
/// or the fixture no longer matches what this generator believes it wrote.
fn pin_attachment_write_token(core_path: &Path) {
    let connection = rusqlite::Connection::open(core_path)
        .expect("open SQLite durable-core fixture to pin the attachment write token");
    let rewritten = connection
        .execute(
            "UPDATE attachment_manifest SET write_id = ?1 WHERE attachment_id = ?2",
            rusqlite::params![
                fixture::FIXTURE_ATTACHMENT_WRITE_ID,
                fixture::FIXTURE_ATTACHMENT_ID
            ],
        )
        .expect("pin the SQLite fixture attachment write token");
    assert_eq!(
        rewritten, 1,
        "the fixture seeds exactly one attachment manifest row to pin; {rewritten} were rewritten"
    );
}

/// Replace the host-clock instant the release stamp recorded while priming with
/// the fixture's frozen one.
///
/// The stamp is written by the schema-open path, which runs before any runtime
/// and therefore before any injected clock exists, so its instant is the real
/// wall clock and every regeneration would otherwise produce different bytes.
/// Rewriting it here is the same move the await-event signing secret already
/// gets: pin the one field the store mints from the environment, so the
/// committed fixture is reproducible. The reopen below leaves the row alone —
/// the update rule only advances the stamp for a strictly newer release.
fn pin_release_stamp_instant(core_path: &Path, timestamp_ms: u64) {
    let pinned = rusqlite::Connection::open(core_path)
        .expect("open SQLite durable-core fixture for a deterministic release stamp")
        .execute(
            "UPDATE release_stamp SET written_at_epoch_ms = ?1 WHERE singleton = 1",
            rusqlite::params![timestamp_ms as i64],
        )
        .expect("pin the SQLite durable-core release stamp instant");
    assert_eq!(
        pinned, 1,
        "priming the durable core must have stamped exactly one release row; {pinned} were rewritten"
    );
}

async fn open_handles(root: &Path, timestamp_ms: u64) -> fixture::FixtureHandles {
    std::fs::create_dir_all(root).expect("create SQLite fixture root");
    let clock = Arc::new(lash_core::testing::TestClock::new(timestamp_ms));
    // Durable core carries its own `await_event_meta` row, seeded by the schema
    // with `randomblob(32)`. Pin it before anything is written, so that nothing
    // the fixture seeds can be derived from a secret that changes per run.
    let core_path = root.join("durable-core.db");
    let priming_runtime =
        Store::open_with_clock(&core_path, Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .await
            .expect("prime SQLite durable-core fixture schema");
    drop(priming_runtime);
    rusqlite::Connection::open(&core_path)
        .expect("open SQLite durable-core fixture for deterministic secret")
        .execute(
            "UPDATE await_event_meta SET signing_secret = ?1 WHERE singleton = 1",
            rusqlite::params![fixture::FIXTURE_AWAIT_EVENT_SIGNING_SECRET.to_vec()],
        )
        .expect("install deterministic SQLite durable-core await-event signing secret");
    pin_release_stamp_instant(&core_path, timestamp_ms);
    let runtime = Arc::new(
        Store::open_with_clock(&core_path, Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
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
            rusqlite::params![fixture::FIXTURE_AWAIT_EVENT_SIGNING_SECRET.to_vec()],
        )
        .expect("install deterministic SQLite effect await-event signing secret");
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
        processes: Arc::clone(&processes) as Arc<dyn lash_core::ConformanceProcessRegistry>,
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
        let destination = to.join(name);
        std::fs::copy(from.join(name), &destination)
            .unwrap_or_else(|error| panic!("copy committed SQLite fixture {name}: {error}"));
        make_fixture_copy_writable(&destination);
    }
}

#[cfg(unix)]
fn make_fixture_copy_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .unwrap_or_else(|error| panic!("make copied SQLite fixture writable: {error}"));
}

#[cfg(not(unix))]
fn make_fixture_copy_writable(path: &Path) {
    let mut permissions = std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("read copied SQLite fixture permissions: {error}"))
        .permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("make copied SQLite fixture writable: {error}"));
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
