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
use lash_postgres_store::{MigrationPhase, PostgresStorage};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;

mod support;

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

const REBASED_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-97-e327bd63e/postgres-expected.json",
];
const SETTLEMENT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-96-f9e0aa07d/postgres-expected.json",
];
const REGENERATE_ENV: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES";
const EFFECT_OUTCOME_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-106-effect-outcome-predecessor/postgres-expected.json",
];
const OBSERVER_SELECTION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-98-observer-predecessor/postgres-expected.json",
];
const CONSTRAINT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-102-379dda204/postgres-expected.json",
];
const MESSAGE_BODY_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-96-4883e7a46/postgres-expected.json",
];
const ENVELOPE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-95-e7d07c89b/postgres-expected.json",
];
const FIXTURE_SCHEMA: &str = "lash_durable_read_fixture";
const BOUNDARY_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-80-fbbeedbb5/postgres-expected.json",
];
const OUTGOING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-81-c938416cc8/postgres-expected.json",
];
const RETIRING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-82-10ae0a31f/postgres-expected.json",
];
const DEPARTING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-83-04b02aef6/postgres-expected.json",
];
const PASSING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-84-9a8b048f3/postgres-expected.json",
];
const CLOSING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-85-9b80fb5b7/postgres-expected.json",
];
const PARTING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-86-a1cf357c7/postgres-expected.json",
];
const FADING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-87-4c40db338/postgres-expected.json",
];
const WANING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-88-9e92bc263/postgres-expected.json",
];
const EBBING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-89-7e5feb69d/postgres-expected.json",
];
const DWINDLING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-90-3cdb48643/postgres-expected.json",
];
const SLIPPING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-91-45dbe5ab2/postgres-expected.json",
];
const RECEDING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-92-eef96581a/postgres-expected.json",
];
const SUBSIDING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-93-47ab59286/postgres-expected.json",
];
const LAPSING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-94-b66a55237/postgres-expected.json",
];
const DECLINING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-95-a596c2237/postgres-expected.json",
];
const ABATING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-99-eeeedf38a/postgres-expected.json",
];
const FLEETING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-100-failure-code-predecessor/postgres-expected.json",
];
const EXPIRING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-101-f2a4770bd/postgres-expected.json",
];
const FIG_3484_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-105-fig-3484/postgres-expected.json",
];
const SETTLING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-103-constraint-predecessor/postgres-expected.json",
];
const USAGE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-104-f1d0c1d5f/postgres-expected.json",
];
const RECEIPT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-107-061de77f7/postgres-expected.json",
];
const SUBMISSION_DIGEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-108-cc0b9eecf/postgres-expected.json",
];
const TOOL_RESULT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-109-b77f0ff6b/postgres-expected.json",
];
const ATTACHMENT_BLOB_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-110-27c3a1b77/postgres-expected.json",
];
const DRIVE_SET_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-111-56e34fddc/postgres-expected.json",
];
const TURN_BOUND_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-112-18c87504b/postgres-expected.json",
];
const CARRIER_CUTOVER_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-113-709567432/postgres-expected.json",
];
const REPLAY_ORDINAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-114-363e43724/postgres-expected.json",
];
const GROUP_PROTOCOL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-115-148f05d42/postgres-expected.json",
];
const DRAIN_WAIT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-116-99467515b/postgres-expected.json",
];
const BINDING_SET_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-117-2a5ea5306/postgres-expected.json",
];
const DIVERGENCE_PARK_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-118-83bda2477/postgres-expected.json",
];
const SESSION_INGRESS_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-119-5b8e9512f/postgres-expected.json",
];
const PARK_FEED_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-120-e242928e4/postgres-expected.json",
];
const NATIVE_CUT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-121-298e495cd/postgres-expected.json",
];
const ADMISSION_BASE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-122-60e0e86b2/postgres-expected.json",
];
const GENERATION_PARK_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-123-6103c2810/postgres-expected.json",
];
const SEQUENCE_IDENTITY_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-124-1d0b41349/postgres-expected.json",
];
const PG_ENGINE_CUT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-125-2ee4a034b/postgres-expected.json",
];
const RETIRED_GENERATION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-126-4f0359684/postgres-expected.json",
];
const CONFIG_REVISION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-127-ebb1defac/postgres-expected.json",
];
const FOLLOW_ON_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-128-8ef0aea502/postgres-expected.json",
];
const PROCESS_IDENTITY_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-129-5f42c383a/postgres-expected.json",
];
const LOGICAL_ROOT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-130-d2906696f/postgres-expected.json",
];
const FRESHEST_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-78-a9506225c8c1/postgres-expected.json",
];
const CURRENT_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-79-171f490d0eb3/postgres-expected.json",
];
const NEWEST_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-77-e102f2b9f861/postgres-expected.json",
];
const LATEST_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-76-023dd85246b4/postgres-expected.json",
];
const LATEST_GENERATION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-75-63f3438c/postgres-expected.json",
];
const CURRENT_GENERATION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-74-6c82924d/postgres-expected.json",
];
const FRESHEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-73-079ae4b4/postgres-expected.json",
];
const NEWEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-72-d2676b45/postgres-expected.json",
];
const LATEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-71-bb852f91/postgres-expected.json",
];
const IMMEDIATE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-70-26d22e05/postgres-expected.json",
];
const CURRENT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-69-330850ee/postgres-expected.json",
];
const PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-68-9680a9bd/postgres-expected.json",
];
const PREVIOUS_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-67-02339d79/postgres-expected.json",
];
const HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-66-847ba3b0/postgres-expected.json",
];
const OLDER_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-65-11f6b0eb/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-65-2bc03f0b/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-65-6a89236a/postgres-expected.json",
];
const ANCIENT_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-64-41ad1609/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-64-dba005a2/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-64-25d7281b/postgres-expected.json",
];
const EARLIER_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-fe2964c7/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-037d9999/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-1b2b8afc/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-75082e3d/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-63-8bbd7b94/postgres-expected.json",
];
const EARLIEST_HISTORICAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-62-7861e438/postgres-expected.json",
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-62-ee717fab/postgres-expected.json",
];

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
    migrate_fixture_forward(&fixture_database_url).await;
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("open restored Postgres durable fixture");
    let handles = open_handles(&storage, fixture::FIXTURE_READ_MS);
    let expected: fixture::ExpectedFixture = serde_json::from_slice(
        &std::fs::read(fixture_dir().join("expected.json"))
            .expect("read committed Postgres durable-fixture expectations"),
    )
    .expect("decode committed Postgres durable-fixture expectations");
    Box::pin(fixture::assert_semantics(&handles, &expected)).await;
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
    let handles = open_handles(&storage, fixture::FIXTURE_WRITE_MS);
    let written_now = Box::pin(fixture::seed(&handles)).await;
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

fn assert_fixture_version() {
    let recorded: PostgresVersion = serde_json::from_slice(
        &std::fs::read(fixture_dir().join("version.json"))
            .expect("read committed Postgres durable-fixture version"),
    )
    .expect("decode committed Postgres durable-fixture version");
    let current = PostgresVersion {
        schema: PostgresStorage::schema_version(),
    };
    // The dump records the component it was captured at, and a version at or
    // behind this build's is honest durable data — the test advances it with
    // `lash migrate` (FIG-3816), the same path a deployment runs, instead of
    // regenerating. A version *ahead* is impossible and a version the expand
    // catalog can no longer carry fails loudly in `migrate` below; regenerate
    // then with LASH_REGENERATE_DURABLE_READ_FIXTURES=1 kiln run
    // //crates/lash-postgres-store:durable_read_fixture__test --
    // regenerate_postgres_durable_fixture --ignored --exact
    assert!(
        recorded.schema <= current.schema,
        "committed Postgres durable fixture declares schema {}, ahead of this build's component {}",
        recorded.schema,
        current.schema
    );
}

/// Advances a restored fixture catalog to this build's component through the
/// same runner a deployment invokes: `lash migrate` under the schema advisory
/// lock, recording each step in the ledger the migration itself creates.
async fn migrate_fixture_forward(fixture_database_url: &str) {
    PostgresStorage::migrate(fixture_database_url, MigrationPhase::Expand)
        .await
        .expect("migrate the restored fixture catalog to this build's component");
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
