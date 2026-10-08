#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core_execution::{DeploymentStore, ProcessExecutionEnvStore};
use lash_sqlite_store::{SqliteStoreSet, SqliteStoreSetOptions};
use serde::{Deserialize, Serialize};

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

const REGENERATE_ENV: &str = "LASH_REGENERATE";

/// The database file a SQLite deployment keeps its store in.
const DATABASE: &str = "lash.db";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SqliteVersions {
    database: i32,
}

/// What this build writes, it reads back with the same meaning: seed a fresh
/// store set, reopen every handle at the read instant, and assert the semantics
/// of what the seed returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_seed_round_trips_through_a_fresh_store() {
    let temp = tempfile::tempdir().expect("SQLite round-trip tempdir");
    let written_now = seed_fresh_store(temp.path()).await;
    let handles = open_handles(temp.path(), fixture::FIXTURE_READ_MS).await;
    Box::pin(fixture::assert_semantics(&handles, &written_now)).await;
}

/// One way a seed's expectations can drift from what the store holds.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-4495 release cut: requires an explicit tagged LASH_RELEASE_FIXTURES_DIR"]
async fn release_sqlite_fixture_reads_retained_semantics() {
    let corpus =
        PathBuf::from(std::env::var_os("LASH_RELEASE_FIXTURES_DIR").expect(
            "release read-back requires LASH_RELEASE_FIXTURES_DIR; no source-tree fallback",
        ));
    let source = corpus.join("sqlite-stores");
    let expected: fixture::ExpectedFixture = serde_json::from_slice(
        &std::fs::read(source.join("expected.json")).expect("read retained SQLite expectations"),
    )
    .expect("decode retained SQLite expectations");
    let versions: SqliteVersions = serde_json::from_slice(
        &std::fs::read(source.join("versions.json")).expect("read retained SQLite versions"),
    )
    .expect("decode retained SQLite versions");
    let session_bytes = std::fs::read(corpus.join("session-at-rest").join(DATABASE))
        .expect("read the retained session catalog");
    assert_eq!(
        session_bytes,
        std::fs::read(source.join(DATABASE)).expect("read the retained database")
    );
    let temp = tempfile::tempdir().expect("release SQLite read-back tempdir");
    // Read the separately named session artifact through the same typed stores.
    std::fs::write(temp.path().join(DATABASE), session_bytes)
        .expect("install the retained session catalog copy");
    assert_eq!(versions_at(temp.path()), versions);
    let handles = open_handles(temp.path(), fixture::FIXTURE_READ_MS).await;
    Box::pin(fixture::assert_semantics(&handles, &expected)).await;
    println!("release SQLite read-back: 4 catalog fixtures, retained semantics matched");
}

async fn seed_fresh_store(root: &Path) -> fixture::ExpectedFixture {
    let handles = open_handles(root, fixture::FIXTURE_WRITE_MS).await;
    Box::pin(fixture::seed(&handles)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "regenerates fixtures/durable-read/v1/sqlite"]
async fn regenerate_sqlite_durable_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge writing the SQLite fixture tree"
    );
    let temp = tempfile::tempdir().expect("SQLite generator tempdir");
    let handles = open_handles(temp.path(), fixture::FIXTURE_WRITE_MS).await;
    let expected = Box::pin(fixture::seed(&handles)).await;
    drop(handles);
    pin_attachment_write_token(&temp.path().join(DATABASE));
    checkpoint(temp.path());

    let destination = fixture_dir();
    std::fs::create_dir_all(&destination).expect("create SQLite fixture directory");
    std::fs::copy(temp.path().join(DATABASE), destination.join(DATABASE))
        .unwrap_or_else(|error| panic!("copy the generated SQLite fixture: {error}"));
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
        .expect("open the SQLite fixture to pin the attachment write token");
    let rewritten = connection
        .execute(
            "UPDATE attachment_pending_writes SET write_id = ?1 WHERE attachment_id = ?2",
            rusqlite::params![
                fixture::FIXTURE_ATTACHMENT_WRITE_ID,
                fixture::fixture_attachment_id().as_str()
            ],
        )
        .expect("pin the SQLite fixture attachment write token");
    assert_eq!(
        rewritten, 1,
        "the fixture seeds exactly one pending attachment write to pin; {rewritten} were rewritten"
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
        .expect("open the SQLite fixture for a deterministic release stamp")
        .execute(
            "UPDATE release_stamp SET written_at_epoch_ms = ?1 WHERE singleton = 1",
            rusqlite::params![timestamp_ms as i64],
        )
        .expect("pin the SQLite release stamp instant");
    assert_eq!(
        pinned, 1,
        "priming the database must have stamped exactly one release row; {pinned} were rewritten"
    );
}

async fn open_handles(root: &Path, timestamp_ms: u64) -> fixture::FixtureHandles {
    std::fs::create_dir_all(root).expect("create SQLite fixture root");
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(timestamp_ms));
    // Prime and reopen one coherent substrate.
    let options = SqliteStoreSetOptions {
        process_id_mint: lash_core_execution::ProcessIdMint::sequential_for_testing(),
        ..SqliteStoreSetOptions::standard(lash_sqlite_store::SqliteSynchronous::Normal)
    };
    let priming = SqliteStoreSet::open_with_options_and_clock(
        root.join(DATABASE),
        options.clone(),
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await
    .expect("prime SQLite fixture schemas");
    drop(priming);
    pin_release_stamp_instant(&root.join(DATABASE), timestamp_ms);
    let stores = SqliteStoreSet::open_with_options_and_clock(
        root.join(DATABASE),
        options,
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await
    .expect("open the coherent SQLite fixture");
    let runtime = stores.session_store_factory();
    let processes = stores.process_registry();
    drop(stores);
    // The handles outlive the dropped assembly; take ownership only to pin
    // the fixture's otherwise random identities.
    let runtime = Arc::new(
        Arc::try_unwrap(runtime)
            .unwrap_or_else(|_| panic!("fixture owns the catalog handle"))
            .with_commit_count_seed_for_testing(0),
    );

    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
        store: Arc::clone(&runtime) as Arc<dyn DeploymentStore>,
        processes: Arc::clone(&processes)
            as Arc<dyn lash_core_execution::ConformanceProcessRegistry>,
        process_envs: runtime as Arc<dyn ProcessExecutionEnvStore>,
    }
}

fn versions_at(root: &Path) -> SqliteVersions {
    SqliteVersions {
        database: user_version(&root.join(DATABASE)),
    }
}

fn user_version(path: &Path) -> i32 {
    rusqlite::Connection::open(path)
        .unwrap_or_else(|error| panic!("open {} for schema version: {error}", path.display()))
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or_else(|error| panic!("read {} schema version: {error}", path.display()))
}

fn checkpoint(root: &Path) {
    let path = root.join(DATABASE);
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

fn fixture_dir() -> PathBuf {
    source_manifest_dir().join("../../fixtures/durable-read/v1/sqlite")
}

fn source_manifest_dir() -> PathBuf {
    let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY");
    if workspace.is_none()
        && std::env::var(REGENERATE_ENV).as_deref() == Ok("1")
        && Path::new(env!("CARGO_MANIFEST_DIR")).is_relative()
    {
        panic!("Buck2 fixture regeneration requires BUILD_WORKSPACE_DIRECTORY");
    }
    source_manifest_dir_for(workspace)
}

fn source_manifest_dir_for(workspace: Option<std::ffi::OsString>) -> PathBuf {
    workspace.map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        |root| PathBuf::from(root).join("crates/lash-sqlite-store"),
    )
}

fn json_with_newline(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("encode SQLite fixture JSON");
    bytes.push(b'\n');
    bytes
}
