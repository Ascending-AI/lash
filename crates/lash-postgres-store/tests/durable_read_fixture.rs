use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use lash_core::{
    EffectHost, ProcessContinuationStore, ProcessExecutionEnvStore, ProcessRegistry,
    RuntimePersistence, SessionStoreFactory, TriggerStore,
};
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_durable_fixture_reads_with_identical_semantics_when_configured() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping Postgres durable fixture: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    assert_fixture_version();
    restore_dump(&database_url).await;
    let fixture_database_url = fixture_database_url(&database_url);
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("open restored Postgres durable fixture");
    let handles = open_handles(&storage, fixture::FIXTURE_READ_MS);
    let expected: fixture::ExpectedFixture = serde_json::from_slice(
        &std::fs::read(fixture_dir().join("expected.json"))
            .expect("read committed Postgres durable-fixture expectations"),
    )
    .expect("decode committed Postgres durable-fixture expectations");
    fixture::assert_semantics(&handles, &expected).await;
    drop(handles);
    storage.pool().close().await;
    drop_fixture_schema(&database_url).await;
}

/// The read-back test above proves old bytes still mean the same thing. This one
/// proves the write side has not drifted away from them: a payload-shape change
/// that never touches `fixtures/` passes the schema-declaration gate and decodes
/// the old artifact unchanged, so without this law it only surfaces when someone
/// else regenerates (FIG-1433).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_durable_fixture_expectations_match_what_this_build_writes_when_configured() {
    let Some(database_url) = support::database_url() else {
        eprintln!(
            "skipping Postgres durable write-shape law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    recreate_fixture_schema(&database_url).await;
    let fixture_database_url = fixture_database_url(&database_url);
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("provision Postgres write-shape schema");
    install_fixed_await_event_secret(&storage).await;
    storage.pool().close().await;
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("reopen Postgres write-shape schema with fixed await-event secret");
    let handles = open_handles(&storage, fixture::FIXTURE_WRITE_MS);
    let written_now = fixture::seed(&handles).await;
    drop(handles);
    storage.pool().close().await;
    drop_fixture_schema(&database_url).await;
    fixture::assert_committed_expectations_match_current_writes(
        &std::fs::read(fixture_dir().join("expected.json"))
            .expect("read committed Postgres durable-fixture expectations"),
        &json_with_newline(&written_now),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_prior_component_encoding_fixture_is_refused_at_hydration_when_configured() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping Postgres component-version refusal fixture: not configured");
        return;
    };
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    restore_dump_from(&database_url, &prior_component_fixture_dir()).await;
    assert_eq!(PostgresStorage::schema_version(), 73);
    let fixture_database_url = fixture_database_url(&database_url);
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("open Postgres component-version refusal fixture");
    let store = storage.session_store(fixture::SESSION_ID);
    fixture::assert_prior_component_encoding_is_refused(&store).await;
    storage.pool().close().await;
    drop_fixture_schema(&database_url).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "writes the committed golden fixture; set LASH_REGENERATE_DURABLE_READ_FIXTURES=1"]
async fn regenerate_postgres_durable_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge replacing the committed Postgres fixture"
    );
    let database_url = support::database_url()
        .expect("set LASH_POSTGRES_DATABASE_URL to an owned throwaway database");
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    recreate_fixture_schema(&database_url).await;
    let fixture_database_url = fixture_database_url(&database_url);
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("provision Postgres durable-fixture schema");
    install_fixed_await_event_secret(&storage).await;
    storage.pool().close().await;
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("reopen Postgres fixture with fixed await-event secret");
    let handles = open_handles(&storage, fixture::FIXTURE_WRITE_MS);
    let expected = fixture::seed(&handles).await;
    normalize_server_authoritative_fixture_rows(&storage).await;
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

/// Author-time catalog refresh for the deliberately stale checkpoint fixture.
///
/// Hard-cutover tables are recreated from this build's authoritative DDL and
/// their pre-cutover rows are discarded. This tooling is not reachable from a
/// store open; the component-v1 checkpoint payload is the only old durable
/// artifact this fixture preserves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "refreshes only the refusal fixture catalog; preserves its component-v1 checkpoint"]
async fn regenerate_postgres_prior_component_fixture_catalog() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge refreshing the refusal fixture catalog"
    );
    let database_url = support::database_url()
        .expect("set LASH_POSTGRES_DATABASE_URL to an owned throwaway database");
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    restore_dump_from(&database_url, &prior_component_fixture_dir()).await;
    let fixture_database_url = fixture_database_url(&database_url);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&fixture_database_url)
        .await
        .expect("connect to refresh refusal fixture catalog");
    sqlx::raw_sql(
        "UPDATE lash_process_events
            SET event_json = jsonb_set(
                event_json::jsonb,
                '{occurred_at}',
                to_jsonb(
                    ((event_json::jsonb #>> '{occurred_at,secs_since_epoch}')::bigint * 1000)
                    + ((event_json::jsonb #>> '{occurred_at,nanos_since_epoch}')::bigint / 1000000)
                )
            )::text
          WHERE jsonb_typeof(event_json::jsonb -> 'occurred_at') = 'object';
         ALTER TABLE lash_process_events DROP COLUMN IF EXISTS occurred_at_ms;",
    )
    .execute(&pool)
    .await
    .expect("cut prior fixture process events over to epoch milliseconds");
    sqlx::query("DROP TABLE lash_usage_deltas")
        .execute(&pool)
        .await
        .expect("discard pre-cutover usage-delta blob rows");
    sqlx::raw_sql(schema_table_ddl("lash_usage_deltas"))
        .execute(&pool)
        .await
        .expect("recreate the usage-delta table from the authoritative DDL");
    sqlx::raw_sql(
        "ALTER TABLE lash_pending_turn_inputs
             DROP CONSTRAINT IF EXISTS ck_pending_turn_inputs_state,
             DROP CONSTRAINT IF EXISTS ck_pending_turn_inputs_state_ingress,
             ADD CONSTRAINT ck_pending_turn_inputs_state
                 CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted',
                                  'cancelled', 'completed')),
             ADD CONSTRAINT ck_pending_turn_inputs_state_ingress
                 CHECK (((ingress_json::jsonb ->> 'scope') = 'active_turn'
                         AND state IN ('pending_active', 'accepted', 'cancelled', 'completed'))
                     OR ((ingress_json::jsonb ->> 'scope') = 'next_turn'
                         AND state IN ('deferred_next_turn', 'cancelled', 'completed')));
         UPDATE lash_schema_versions
            SET version = 73
          WHERE component = 'lash-postgres-store';",
    )
    .execute(&pool)
    .await
    .expect("refresh refusal fixture pending-input and process-lease catalog");
    upgrade_prior_fixture_frame_identity(&pool).await;
    pool.close().await;
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("open the refreshed refusal fixture catalog");
    storage.pool().close().await;
    std::fs::write(
        prior_component_fixture_dir().join("fixture.sql"),
        pg_dump(&database_url),
    )
    .expect("write refreshed Postgres refusal fixture catalog");
    drop_fixture_schema(&database_url).await;
}

fn schema_table_ddl(table: &str) -> &'static str {
    let ddl = PostgresStorage::schema_ddl();
    let marker = format!("CREATE TABLE IF NOT EXISTS {table} (");
    let start = ddl
        .find(&marker)
        .unwrap_or_else(|| panic!("schema DDL must declare {table}"));
    let statement = &ddl[start..];
    let end = statement
        .find(';')
        .unwrap_or_else(|| panic!("{table} DDL must end with a semicolon"));
    &statement[..=end]
}

async fn upgrade_prior_fixture_frame_identity(pool: &sqlx::PgPool) {
    let previous_frame_node_id: String = sqlx::query_scalar(
        "SELECT node_id
           FROM lash_graph_nodes
          WHERE session_id = $1
            AND node_json::jsonb ->> 'kind' = 'frame_open'",
    )
    .bind(fixture::SESSION_ID)
    .fetch_one(pool)
    .await
    .expect("read prior fixture frame identity");
    let frame_key = lash_core::FrameKey::from_caller_material("initial-frame")
        .expect("non-empty initial frame material");
    let frame_node_id =
        lash_core::facade_support::frame_node_id(fixture::SESSION_ID, frame_key.as_str())
            .into_inner();

    let graph_rows = sqlx::query(
        "UPDATE lash_graph_nodes
            SET node_id = CASE WHEN node_id = $2 THEN $3 ELSE node_id END,
                parent_node_id = CASE
                    WHEN parent_node_id = $2 THEN $3
                    ELSE parent_node_id
                END,
                frame_node_id = $3,
                node_json = replace(node_json, $4, $5)
          WHERE session_id = $1",
    )
    .bind(fixture::SESSION_ID)
    .bind(&previous_frame_node_id)
    .bind(&frame_node_id)
    .bind("\"frame_key\":\"initial-frame\"")
    .bind(format!("\"frame_key\":\"{}\"", frame_key.as_str()))
    .execute(pool)
    .await
    .expect("upgrade prior fixture graph frame identity");
    assert_eq!(
        graph_rows.rows_affected(),
        3,
        "the prior fixture carries one three-node frame"
    );

    sqlx::query(
        "UPDATE lash_runtime_turn_commits
            SET result_json = replace(result_json, $2, $3)
          WHERE session_id = $1",
    )
    .bind(fixture::SESSION_ID)
    .bind(&previous_frame_node_id)
    .bind(&frame_node_id)
    .execute(pool)
    .await
    .expect("upgrade prior fixture receipt frame references");
    sqlx::query(
        "UPDATE lash_sessions
            SET head_json = replace(head_json, $2, $3)
          WHERE session_id = $1",
    )
    .bind(fixture::SESSION_ID)
    .bind(&previous_frame_node_id)
    .bind(&frame_node_id)
    .execute(pool)
    .await
    .expect("upgrade prior fixture head frame reference");
}

fn assert_fixture_version() {
    let recorded: PostgresVersion = serde_json::from_slice(
        &std::fs::read(fixture_dir().join("version.json"))
            .expect("read committed Postgres durable-fixture version"),
    )
    .expect("decode committed Postgres durable-fixture version");
    let current = PostgresVersion {
        schema: PostgresStorage::schema_version(),
    };
    assert_eq!(
        recorded, current,
        "declared Postgres durable schema version changed without fixture regeneration; run \
         LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p lash-postgres-store --test \
         durable_read_fixture regenerate_postgres_durable_fixture -- --ignored --exact"
    );
}

fn open_handles(storage: &PostgresStorage, timestamp_ms: u64) -> fixture::FixtureHandles {
    let clock = Arc::new(lash_core::testing::TestClock::new(timestamp_ms));
    let runtime = Arc::new(
        storage
            .session_store(fixture::SESSION_ID)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );
    let processes = Arc::new(
        storage
            .process_registry()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );
    let process_envs = Arc::new(storage.process_env_store());
    let triggers = Arc::new(
        storage
            .trigger_store()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .with_incarnation_for_testing("durable-read-trigger-incarnation"),
    );
    let effects = Arc::new(PostgresEffectHost::with_options_and_clock(
        storage,
        PostgresEffectReplayOptions::default(),
        Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
    ));
    let session_factory = Arc::new(
        storage
            .session_store_factory()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );
    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
        runtime: runtime as Arc<dyn RuntimePersistence>,
        session_factory: session_factory as Arc<dyn SessionStoreFactory>,
        processes: Arc::clone(&processes) as Arc<dyn ProcessRegistry>,
        continuations: processes as Arc<dyn ProcessContinuationStore>,
        process_envs: process_envs as Arc<dyn ProcessExecutionEnvStore>,
        triggers: triggers as Arc<dyn TriggerStore>,
        effects: effects as Arc<dyn EffectHost>,
    }
}

async fn restore_dump(database_url: &str) {
    restore_dump_from(database_url, &fixture_dir()).await;
}

async fn restore_dump_from(database_url: &str, source: &Path) {
    drop_fixture_schema(database_url).await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("connect for Postgres durable-fixture restore");
    let dump = std::fs::read_to_string(source.join("fixture.sql"))
        .expect("read committed Postgres durable fixture dump");
    sqlx::raw_sql(&dump)
        .execute(&pool)
        .await
        .expect("restore committed Postgres durable fixture dump");
    pool.close().await;
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

async fn install_fixed_await_event_secret(storage: &PostgresStorage) {
    sqlx::query("UPDATE lash_await_event_meta SET signing_secret = $1 WHERE singleton = TRUE")
        .bind(vec![0x88_u8; 32])
        .execute(storage.pool())
        .await
        .expect("install deterministic Postgres await-event signing secret");
}

async fn normalize_server_authoritative_fixture_rows(storage: &PostgresStorage) {
    let lease = fixture::expected_process_lease();
    sqlx::query(
        "UPDATE lash_process_leases
         SET lease_token = $2, lease_claimed_at_ms = $3, lease_expires_at_ms = $4
         WHERE process_id = $1",
    )
    .bind(&lease.process_id)
    .bind(&lease.lease_token)
    .bind(lease.claimed_at_epoch_ms as i64)
    .bind(lease.expires_at_epoch_ms as i64)
    .execute(storage.pool())
    .await
    .expect("normalize server-authoritative fixture process lease");
    sqlx::query(
        "UPDATE lash_session_execution_leases
         SET lease_claimed_at_ms = $2, lease_expires_at_ms = $3, lease_term_ms = $4
         WHERE session_id = $1",
    )
    .bind(fixture::SESSION_ID)
    .bind(fixture::FIXTURE_WRITE_MS as i64)
    .bind((fixture::FIXTURE_WRITE_MS + 100) as i64)
    .bind(100_i64)
    .execute(storage.pool())
    .await
    .expect("normalize server-authoritative fixture session lease");
    sqlx::query(
        "UPDATE lash_runtime_effect_replay
         SET created_at_ms = $1, updated_at_ms = $1",
    )
    .bind(fixture::FIXTURE_WRITE_MS as i64)
    .execute(storage.pool())
    .await
    .expect("normalize server-authoritative fixture effect timestamps");
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/durable-read/v1/postgres")
}

fn prior_component_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/checkpoint-component-v1-refusal/postgres")
}

fn json_with_newline(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("encode Postgres fixture JSON");
    bytes.push(b'\n');
    bytes
}
