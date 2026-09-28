#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use lash_core_execution::{
    ProcessContinuationStore, ProcessExecutionEnvStore, RuntimePersistence, SessionStoreFactory,
    TriggerStore,
};
use lash_postgres_store::PostgresStorage;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;

mod support;

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

const REGENERATE_ENV: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES";
const FIXTURE_SCHEMA: &str = "lash_durable_read_fixture";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PostgresVersion {
    schema: i32,
}

/// What this build writes, it reads back with the same meaning: seed a fresh
/// schema, reopen every handle at the read instant, and assert the semantics of
/// what the seed returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_seed_round_trips_through_a_fresh_store_when_configured() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping Postgres durable round trip: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    recreate_fixture_schema(&database_url).await;
    let fixture_database_url = fixture_database_url(&database_url);
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("provision the Postgres round-trip schema");
    let written_now = {
        let handles = open_handles(&storage, fixture::FIXTURE_WRITE_MS);
        Box::pin(fixture::seed(&handles)).await
    };
    storage.pool().close().await;
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("reopen the Postgres round-trip schema");
    let handles = open_handles(&storage, fixture::FIXTURE_READ_MS);
    Box::pin(fixture::assert_semantics(&handles, &written_now)).await;
    drop(handles);
    storage.pool().close().await;
    drop_fixture_schema(&database_url).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "writes the release-capture fixture tree; set LASH_REGENERATE_DURABLE_READ_FIXTURES=1"]
async fn regenerate_postgres_durable_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge writing the Postgres fixture tree"
    );
    let database_url = support::database_url()
        .expect("set LASH_POSTGRES_DATABASE_URL to an owned throwaway database");
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    recreate_fixture_schema(&database_url).await;
    let fixture_database_url = fixture_database_url(&database_url);
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("provision Postgres durable-fixture schema");
    install_fixed_catalog_identity(&storage).await;
    let handles = open_handles(&storage, fixture::FIXTURE_WRITE_MS);
    let expected = Box::pin(fixture::seed(&handles)).await;
    normalize_fixture_rows(&storage).await;
    drop(handles);
    storage.pool().close().await;

    let destination = fixture_dir();
    std::fs::create_dir_all(&destination).expect("create Postgres fixture directory");
    std::fs::write(
        destination.join("expected.json"),
        json_with_newline(&expected),
    )
    .expect("write Postgres fixture expectations");
    std::fs::write(
        destination.join("version.json"),
        json_with_newline(&PostgresVersion {
            schema: PostgresStorage::schema_version(),
        }),
    )
    .expect("write Postgres fixture version");
    std::fs::write(destination.join("fixture.sql"), pg_dump(&database_url))
        .expect("write Postgres fixture dump");
    drop_fixture_schema(&database_url).await;
}

fn open_handles(storage: &PostgresStorage, timestamp_ms: u64) -> fixture::FixtureHandles {
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(timestamp_ms));
    let runtime = Arc::new(
        storage
            .session_store(fixture::SESSION_ID)
            .with_lease_clock_for_testing(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>),
    );
    let processes = Arc::new(
        storage
            .process_registry()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
            .with_process_id_mint_for_testing(
                lash_core_execution::ProcessIdMint::sequential_for_testing(),
            ),
    );
    let process_envs = Arc::new(storage.process_env_store());
    let triggers = Arc::new(
        storage
            .trigger_store()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
            .with_incarnation_for_testing("durable-read-trigger-incarnation"),
    );
    let session_factory = Arc::new(
        storage
            .session_store_factory()
            .with_lease_clock_for_testing(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>),
    );
    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
        runtime: runtime as Arc<dyn RuntimePersistence>,
        session_factory: session_factory as Arc<dyn SessionStoreFactory>,
        processes: Arc::clone(&processes)
            as Arc<dyn lash_core_execution::ConformanceProcessRegistry>,
        continuations: processes as Arc<dyn ProcessContinuationStore>,
        process_envs: process_envs as Arc<dyn ProcessExecutionEnvStore>,
        triggers: triggers as Arc<dyn TriggerStore>,
        // PostgreSQL is storage only: its effects journal on Restate (ADR 0104).
    }
}

async fn recreate_fixture_schema(database_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("connect for Postgres durable-fixture reset");
    sqlx::raw_sql(&format!(
        "DROP SCHEMA IF EXISTS {FIXTURE_SCHEMA} CASCADE; CREATE SCHEMA {FIXTURE_SCHEMA};"
    ))
    .execute(&pool)
    .await
    .expect("recreate dedicated Postgres durable-fixture schema");
    // Worker open never provisions (FIG-3797): the fixture schema gets its
    // tables from the committed artifact, applied the way `lash migrate` does.
    sqlx::raw_sql(&format!("SET search_path TO {FIXTURE_SCHEMA};"))
        .execute(&pool)
        .await
        .expect("point the fixture pool at the recreated schema");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&pool)
        .await
        .expect("provision the durable-fixture schema from schema.sql");
    pool.close().await;
}

async fn drop_fixture_schema(database_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("connect for Postgres durable-fixture teardown");
    sqlx::raw_sql(&format!("DROP SCHEMA IF EXISTS {FIXTURE_SCHEMA} CASCADE;"))
        .execute(&pool)
        .await
        .expect("drop dedicated Postgres durable-fixture schema");
    pool.close().await;
}

/// The committed dump's catalog identity: the seed draws a random one per
/// install, so a regeneration would otherwise differ on every run.
const FIXTURE_CATALOG_ID: &str = "00000000-0000-4000-8000-000000000887";

async fn install_fixed_catalog_identity(storage: &PostgresStorage) {
    sqlx::query("UPDATE lash_catalog_identity SET catalog_id = $1 WHERE singleton = TRUE")
        .bind(FIXTURE_CATALOG_ID)
        .execute(storage.pool())
        .await
        .expect("install the fixed fixture catalog identity");
}

async fn normalize_fixture_rows(storage: &PostgresStorage) {
    let pinned = sqlx::query(
        "UPDATE lash_parent_end_plans SET obligation_id = $1 WHERE parent_kind = 'process'",
    )
    .bind(fixture::FIXTURE_PARENT_END_OBLIGATION_ID)
    .execute(storage.pool())
    .await
    .expect("pin the fixture parent-end obligation id");
    assert_eq!(
        pinned.rows_affected(),
        1,
        "the fixture seeds one parent-end obligation"
    );
    let pinned = sqlx::query(
        "UPDATE lash_process_events
         SET event_json = jsonb_set(event_json::jsonb, '{occurred_at}', to_jsonb($1::bigint))::text
         WHERE event_type = 'process.first_started'",
    )
    .bind(fixture::FIXTURE_WRITE_MS as i64)
    .execute(storage.pool())
    .await
    .expect("pin the fixture first-started event time");
    assert_eq!(
        pinned.rows_affected(),
        1,
        "the fixture seeds one first-started event"
    );
    // The release stamp records `clock_timestamp()` on the server, so it is
    // server-authoritative in exactly the sense this function exists for:
    // without the rewrite every regeneration would emit a different dump.
    let stamped = sqlx::query("UPDATE lash_release_stamp SET written_at_epoch_ms = $1")
        .bind(fixture::FIXTURE_WRITE_MS as i64)
        .execute(storage.pool())
        .await
        .expect("normalize the server-authoritative fixture release stamp instant");
    assert_eq!(
        stamped.rows_affected(),
        1,
        "opening the fixture database must have stamped exactly one release row"
    );
    pin_attachment_write_token(storage).await;
}

/// Replace the random write token `begin_attachment_write` minted while seeding
/// with the fixture's fixed one.
///
/// The token is minted inside the store, so it cannot be handed in; it is
/// rewritten afterwards instead. The row must exist and must be the only one,
/// or the fixture no longer matches what this generator believes it wrote.
async fn pin_attachment_write_token(storage: &PostgresStorage) {
    let rewritten =
        sqlx::query("UPDATE lash_attachment_manifest SET write_id = $1 WHERE attachment_id = $2")
            .bind(fixture::FIXTURE_ATTACHMENT_WRITE_ID)
            .bind(fixture::FIXTURE_ATTACHMENT_ID)
            .execute(storage.pool())
            .await
            .expect("pin the Postgres fixture attachment write token")
            .rows_affected();
    assert_eq!(
        rewritten, 1,
        "the fixture seeds exactly one attachment manifest row to pin; {rewritten} were rewritten"
    );
}

fn fixture_database_url(database_url: &str) -> String {
    let separator = if database_url.contains('?') { '&' } else { '?' };
    format!("{database_url}{separator}options=-csearch_path%3D{FIXTURE_SCHEMA}")
}

fn pg_dump(database_url: &str) -> Vec<u8> {
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "host",
            "--env",
            "PGCLIENTENCODING=UTF8",
            "postgres:16-alpine",
            "pg_dump",
            "--format=plain",
            "--no-owner",
            "--no-privileges",
            "--no-comments",
            "--inserts",
            "--schema=lash_durable_read_fixture",
            database_url,
        ])
        .output()
        .expect("run postgres:16 pg_dump fixture generator");
    assert!(
        output.status.success(),
        "postgres:16 pg_dump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dump = String::from_utf8(output.stdout).expect("pg_dump output is UTF-8");
    let mut lines = dump
        .lines()
        .filter(|line| {
            !line.starts_with("\\restrict ")
                && !line.starts_with("\\unrestrict ")
                && *line != "SET transaction_timeout = 0;"
        })
        .collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let normalized = lines.join("\n");
    format!("{normalized}\n").into_bytes()
}

fn fixture_dir() -> PathBuf {
    source_manifest_dir().join("../../fixtures/durable-read/v1/postgres")
}

fn source_manifest_dir() -> PathBuf {
    let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY");
    if workspace.is_none()
        && std::env::var(REGENERATE_ENV).as_deref() == Ok("1")
        && Path::new(env!("CARGO_MANIFEST_DIR")).is_relative()
    {
        panic!("Bazel fixture regeneration requires BUILD_WORKSPACE_DIRECTORY");
    }
    source_manifest_dir_for(workspace)
}

fn source_manifest_dir_for(workspace: Option<std::ffi::OsString>) -> PathBuf {
    workspace.map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        |root| PathBuf::from(root).join("crates/lash-postgres-store"),
    )
}

#[test]
fn fixture_source_dir_resolves_bazel_workspace() {
    assert_eq!(
        source_manifest_dir_for(Some("/tmp/lash-fork".into())),
        PathBuf::from("/tmp/lash-fork/crates/lash-postgres-store")
    );
    assert_eq!(
        source_manifest_dir_for(None),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    );
}

fn json_with_newline(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("encode Postgres fixture JSON");
    bytes.push(b'\n');
    bytes
}
