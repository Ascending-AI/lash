#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::SessionId;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use lash_core::{
    EffectHost, ProcessContinuationStore, ProcessExecutionEnvStore, RuntimePersistence,
    SessionStoreFactory, TriggerStore,
};
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};
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
const OBSERVER_SELECTION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-98-observer-predecessor/postgres-expected.json",
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
async fn postgres_prior_component_encoding_fixture_is_refused_at_hydration_when_configured() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping Postgres component-version refusal fixture: not configured");
        return;
    };
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    restore_dump_from(&database_url, &prior_component_fixture_dir()).await;
    // The fixture's catalog tracks the current component by design -- its
    // regeneration refreshes the catalog and preserves only the component-v1
    // checkpoint payload -- so this pins that the two were moved together. It
    // is the tripwire FIG-3414 tripped: the constant went 105 -> 106 without
    // this literal following, so the assertion failed before the payload-level
    // refusal below was ever reached.
    assert_eq!(PostgresStorage::schema_version(), 114);
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
    let expected = Box::pin(fixture::seed(&handles)).await;
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
        "DROP TABLE IF EXISTS lash_turn_cancel_closure_authorizations;
         DROP TABLE IF EXISTS lash_turn_cancellation_bindings;
         DROP TABLE IF EXISTS lash_turn_cancel_retired_scopes;",
    )
    .execute(&pool)
    .await
    .expect("discard pre-cutover turn cancellation closure tables");
    for table in [
        "lash_turn_cancellation_bindings",
        "lash_turn_cancel_closure_authorizations",
        "lash_turn_cancel_retired_scopes",
    ] {
        sqlx::raw_sql(schema_table_ddl(table))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("create {table} from authoritative DDL: {error}"));
    }
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
    sqlx::query("DROP TABLE IF EXISTS lash_effect_scope_retirements")
        .execute(&pool)
        .await
        .expect("discard the pre-cleanup-evidence effect-scope retirement table");
    sqlx::raw_sql(schema_table_ddl("lash_effect_scope_retirements"))
        .execute(&pool)
        .await
        .expect("recreate the effect-scope retirement fence from the authoritative DDL");
    sqlx::query("DROP TABLE lash_attachment_condemnations")
        .execute(&pool)
        .await
        .expect("discard the pre-write-token attachment condemnation table");
    sqlx::raw_sql(schema_table_ddl("lash_attachment_condemnations"))
        .execute(&pool)
        .await
        .expect("recreate the attachment condemnation table from the authoritative DDL");
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS lash_process_artifact_cleanup;
         DROP TABLE lash_parent_end_plans;
         DROP TABLE lash_process_segment_handovers;
         DROP TABLE lash_process_leases;
         DROP TABLE lash_process_observers;
         DROP TABLE lash_process_wake_deliveries;
         DROP TABLE lash_process_events;
         DROP TABLE lash_process_tombstones;
         DROP TABLE lash_processes;
         DROP TABLE lash_process_change_clock;
         DROP TABLE lash_wake_allocation_floors;",
    )
    .execute(&pool)
    .await
    .expect("discard pre-incarnation process registry rows");
    sqlx::raw_sql(schema_process_registry_ddl())
        .execute(&pool)
        .await
        .expect("recreate the process registry from the authoritative DDL");
    sqlx::raw_sql(schema_artifact_owner_ddl())
        .execute(&pool)
        .await
        .expect("create exact artifact ownership from the authoritative DDL");
    sqlx::raw_sql(
        "ALTER TABLE lash_pending_turn_inputs
             DROP CONSTRAINT IF EXISTS ck_pending_turn_inputs_state,
             DROP CONSTRAINT IF EXISTS ck_pending_turn_inputs_state_ingress,
             DROP CONSTRAINT IF EXISTS ck_pending_turn_inputs_claim_id_token_all_or_none,
             DROP CONSTRAINT IF EXISTS ck_pending_turn_inputs_claim_identity_all_or_none,
             ADD CONSTRAINT ck_pending_turn_inputs_state
                 CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted',
                                  'cancelled', 'completed')),
             ADD CONSTRAINT ck_pending_turn_inputs_state_ingress
                 CHECK (((ingress_json::jsonb ->> 'scope') = 'active_turn'
                         AND state IN ('pending_active', 'accepted', 'cancelled', 'completed'))
                     OR ((ingress_json::jsonb ->> 'scope') = 'next_turn'
                         AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
             ADD CONSTRAINT ck_pending_turn_inputs_claim_identity_all_or_none
                 CHECK ((claim_id IS NULL AND claim_owner_id IS NULL
                         AND claim_owner_incarnation_id IS NULL AND claim_token IS NULL)
                     OR (claim_id IS NOT NULL AND claim_owner_id IS NOT NULL
                         AND claim_owner_incarnation_id IS NOT NULL AND claim_token IS NOT NULL));
         ALTER TABLE lash_runtime_turn_commits
             DROP CONSTRAINT IF EXISTS lash_runtime_turn_commits_append_identity_all_or_none,
             ADD CONSTRAINT lash_runtime_turn_commits_append_identity_all_or_none
                 CHECK ((request_identity_hash IS NULL) = (identity_encoding_version IS NULL)
                     AND (requested_node_count IS NULL OR request_identity_hash IS NOT NULL));
         ALTER TABLE lash_attachment_manifest
             ADD COLUMN IF NOT EXISTS write_id TEXT,
             ADD COLUMN IF NOT EXISTS written_at_ms BIGINT,
             ADD COLUMN IF NOT EXISTS owner_incarnation BIGINT,
             DROP CONSTRAINT IF EXISTS lash_attachment_manifest_check,
             DROP CONSTRAINT IF EXISTS ck_lash_attachment_manifest_owner_identity,
             ADD CONSTRAINT ck_lash_attachment_manifest_owner_identity
                 CHECK ((owner_kind IS NULL AND owner_id IS NULL AND owner_incarnation IS NULL)
                     OR (owner_kind = 'turn' AND owner_id IS NOT NULL
                         AND owner_incarnation IS NULL)
                     OR (owner_kind = 'process' AND owner_id IS NOT NULL
                         AND owner_incarnation IS NOT NULL));
         DROP INDEX IF EXISTS idx_lash_attachment_manifest_owner;
         CREATE INDEX idx_lash_attachment_manifest_owner
             ON lash_attachment_manifest(
                 session_id, owner_kind, owner_id, owner_incarnation, committed_at_ms
             );
         DROP INDEX IF EXISTS idx_lash_attachment_manifest_written;
         CREATE INDEX idx_lash_attachment_manifest_written
             ON lash_attachment_manifest(attachment_id, written_at_ms);
         ALTER TABLE lash_attachment_condemnations
             DROP CONSTRAINT IF EXISTS lash_attachment_condemnations_phase_check,
             ADD CONSTRAINT lash_attachment_condemnations_phase_check
                 CHECK (phase IN ('condemned', 'deleting'));
         ALTER TABLE lash_turn_cancel_requests
             ADD COLUMN IF NOT EXISTS mode TEXT NOT NULL DEFAULT 'immediate',
             ADD COLUMN IF NOT EXISTS intent_revision BIGINT NOT NULL DEFAULT 1;
         ALTER TABLE lash_turn_cancel_requests
             ALTER COLUMN intent_revision DROP DEFAULT;",
    )
    .execute(&pool)
    .await
    .expect("refresh refusal fixture pending-input and process-lease catalog");
    sqlx::raw_sql(
        "ALTER TABLE lash_turn_cancel_requests
             DROP COLUMN IF EXISTS affected_input_ids,
             DROP COLUMN IF EXISTS affected_dispositions;",
    )
    .execute(&pool)
    .await
    .expect("retire the parallel affected-input arrays");
    sqlx::raw_sql(schema_table_ddl("lash_turn_cancel_affected_inputs"))
        .execute(&pool)
        .await
        .expect("create the affected-input child table from the authoritative DDL");
    // The effect-group arbitration columns and guards land on the catalog as a
    // delta rather than a rebuild: the refusal fixture's membership rows are
    // evidence the refresh must keep.
    sqlx::raw_sql(
        "ALTER TABLE lash_runtime_effect_group
             ADD COLUMN IF NOT EXISTS next_commit_seq BIGINT NOT NULL DEFAULT 0,
             ADD COLUMN IF NOT EXISTS lifecycle JSONB NOT NULL DEFAULT '{\"type\":\"live\"}';
         ALTER TABLE lash_runtime_effect_group
             DROP CONSTRAINT IF EXISTS ck_runtime_effect_group_lifecycle;
         ALTER TABLE lash_runtime_effect_group
             ADD CONSTRAINT ck_runtime_effect_group_lifecycle CHECK (
                 lifecycle->>'type' IN ('live', 'closing', 'settled')
             );
         DO $$
         BEGIN
             IF EXISTS (
                 SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema()
                   AND table_name = 'lash_runtime_effect_group'
                   AND column_name = 'children'
             ) THEN
                 ALTER TABLE lash_runtime_effect_group
                     RENAME COLUMN children TO expected_children;
             END IF;
             IF EXISTS (
                 SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema()
                   AND table_name = 'lash_runtime_effect_group_child'
                   AND column_name = 'request_version'
             ) THEN
                 ALTER TABLE lash_runtime_effect_group_child
                     RENAME COLUMN request_version TO command_version;
             END IF;
         END $$;
         ALTER TABLE lash_runtime_effect_group_child
             DROP COLUMN IF EXISTS decision,
             DROP COLUMN IF EXISTS commit_seq,
             DROP COLUMN IF EXISTS decided_at_ms,
             DROP COLUMN IF EXISTS drained_at_ms;
         DROP INDEX IF EXISTS uq_lash_runtime_effect_group_child_commit_seq;
         ALTER TABLE lash_runtime_effect_replay
             ADD COLUMN IF NOT EXISTS commit_state TEXT NOT NULL DEFAULT 'pending',
             ADD COLUMN IF NOT EXISTS commit_seq BIGINT,
             ADD COLUMN IF NOT EXISTS drain_input TEXT;
         ALTER TABLE lash_runtime_effect_replay
             DROP CONSTRAINT IF EXISTS ck_runtime_effect_replay_commit_state;
         ALTER TABLE lash_runtime_effect_replay
             DROP CONSTRAINT IF EXISTS ck_runtime_effect_replay_commit_seq;
         ALTER TABLE lash_runtime_effect_replay
             DROP CONSTRAINT IF EXISTS ck_runtime_effect_replay_drain_input;
         ALTER TABLE lash_runtime_effect_replay
             ADD CONSTRAINT ck_runtime_effect_replay_commit_state CHECK (
                 commit_state IN ('pending', 'committed', 'drained', 'cancel_decided')
             ),
             ADD CONSTRAINT ck_runtime_effect_replay_commit_seq CHECK (
                 commit_seq IS NULL OR commit_state IN ('committed', 'drained')
             ),
             ADD CONSTRAINT ck_runtime_effect_replay_drain_input CHECK (
                 drain_input IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))
             );
         DROP INDEX IF EXISTS idx_lash_runtime_effect_replay_group_unsettled;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_lash_runtime_effect_replay_commit_seq
             ON lash_runtime_effect_replay(group_key, commit_seq)
             WHERE commit_seq IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_lash_runtime_effect_group_child_replay_key
             ON lash_runtime_effect_group_child(group_key, replay_key);",
    )
    .execute(&pool)
    .await
    .expect("refresh refusal fixture with the component-113 explicit-observer catalog");
    // The trigger subscription table cut over to the lifecycle column shape
    // with no migration, so the refusal fixture discards its pre-cutover rows
    // and takes the current catalog.
    sqlx::query("DROP TABLE IF EXISTS lash_trigger_subscriptions")
        .execute(&pool)
        .await
        .expect("discard pre-cutover trigger subscription rows");
    sqlx::raw_sql(schema_trigger_subscriptions_ddl())
        .execute(&pool)
        .await
        .expect("recreate the trigger subscription catalog from the authoritative DDL");
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS lash_session_meta_fork_inheritance_processes;
         ALTER TABLE lash_session_meta DROP COLUMN IF EXISTS observer_inheritance_kind;
         ALTER TABLE lash_session_meta_pending_observer_intents DROP COLUMN IF EXISTS attribution;",
    )
    .execute(&pool)
    .await
    .expect("remove retired observer selection from the refusal fixture catalog");
    // The enclosing catalog uses the current session-metadata constraints;
    // only the deliberately obsolete checkpoint component remains historical.
    for constraint in lash_core::store_backend_support::required_constraints::EXPECTED_CONSTRAINTS
        .iter()
        .filter_map(|constraint| constraint.postgres)
        .filter(|constraint| constraint.table == "lash_session_meta")
    {
        sqlx::raw_sql(&format!(
            "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}, ADD CONSTRAINT {} CHECK ({})",
            constraint.table, constraint.name, constraint.name, constraint.expression,
        ))
        .execute(&pool)
        .await
        .expect("refresh refusal fixture session-meta contract from the current schema");
    }
    sqlx::query(
        "UPDATE lash_schema_versions
            SET version = $1
          WHERE component = 'lash-postgres-store'",
    )
    .bind(PostgresStorage::schema_version())
    .execute(&pool)
    .await
    .expect("stamp the refusal fixture with the current component generation");
    upgrade_prior_fixture_frame_identity(&pool).await;
    refresh_prior_fixture_node_bodies(&pool).await;
    sqlx::query("UPDATE lash_session_meta SET session_state_version = $1")
        .bind(i32::try_from(lash_core::store::CURRENT_SESSION_STATE_VERSION).unwrap())
        .execute(&pool)
        .await
        .expect(
            "refresh enclosing session-state admission marker for the component refusal witness",
        );
    // Keep the component-v1 payload as the refusal witness while refreshing
    // the enclosing head so hydration reaches that intended boundary.
    sqlx::query(
        "UPDATE lash_sessions
            SET head_json = jsonb_set(
                jsonb_set(head_json::jsonb, '{schema_version}', to_jsonb($1::bigint)),
                '{config,tool_access}',
                '{\"mode\":\"ambient\"}'::jsonb
            )::text",
    )
    .bind(i64::from(
        lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
    ))
    .execute(&pool)
    .await
    .expect("refresh refusal fixture head schema without changing its checkpoint");
    upgrade_prior_fixture_checkpoint_manifests(&pool).await;
    upgrade_prior_fixture_graph_node_bodies(&pool).await;
    pool.close().await;
    let storage = PostgresStorage::connect(&fixture_database_url)
        .await
        .expect("open the refreshed refusal fixture catalog");
    // That open stamped the release with `clock_timestamp()`. Freeze it, or the
    // committed catalog changes bytes on every refresh for a reason that has
    // nothing to do with the refusal it witnesses.
    let stamped = sqlx::query("UPDATE lash_release_stamp SET written_at_epoch_ms = $1")
        .bind(fixture::FIXTURE_WRITE_MS as i64)
        .execute(storage.pool())
        .await
        .expect("freeze the refusal fixture release stamp instant");
    assert_eq!(
        stamped.rows_affected(),
        1,
        "opening the refusal fixture must have stamped exactly one release row"
    );
    storage.pool().close().await;
    std::fs::write(
        prior_component_fixture_dir().join("fixture.sql"),
        pg_dump(&database_url),
    )
    .expect("write refreshed Postgres refusal fixture catalog");
    drop_fixture_schema(&database_url).await;
}

// Author-time envelope refresh only: retain the deliberately obsolete leaf bytes
// and encoding descriptor so the refusal fixture reaches component admission.
async fn upgrade_prior_fixture_checkpoint_manifests(pool: &sqlx::PgPool) {
    use lash_core::store::{BlobRef, SESSION_CHECKPOINT_SCHEMA_VERSION, SessionCheckpoint};
    let blobs: Vec<(String, Vec<u8>)> = sqlx::query_as("SELECT hash, content FROM lash_blobs")
        .fetch_all(pool)
        .await
        .expect("read refusal fixture blobs");
    for (old_hash, bytes) in blobs {
        let Ok(mut value) = rmp_serde::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if value.get("turn_state").is_none()
            || value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                == Some(u64::from(SESSION_CHECKPOINT_SCHEMA_VERSION))
        {
            continue;
        }
        value["schema_version"] = SESSION_CHECKPOINT_SCHEMA_VERSION.into();
        let checkpoint: SessionCheckpoint = serde_json::from_value(value)
            .expect("decode refreshed checkpoint envelope without changing component descriptors");
        let bytes =
            rmp_serde::to_vec_named(&checkpoint).expect("encode refreshed checkpoint envelope");
        let new_hash = BlobRef::for_content(&bytes).0;
        sqlx::query(
            "INSERT INTO lash_blobs (hash, content) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&new_hash)
        .bind(&bytes)
        .execute(pool)
        .await
        .expect("write refreshed checkpoint root");
        for table in [
            "lash_sessions",
            "lash_node_anchors",
            "lash_checkpoint_blob_refs",
        ] {
            sqlx::query(&format!(
                "UPDATE {table} SET checkpoint_ref = $1 WHERE checkpoint_ref = $2"
            ))
            .bind(&new_hash)
            .bind(&old_hash)
            .execute(pool)
            .await
            .expect("retarget fixture checkpoint root");
        }
    }
}

// Author-time body restamp: the refusal fixture's node payloads carry no
// error vocabulary, so each body decodes under the current node-body
// generation once its stamp moves; decoding here is the proof, not a hope.
async fn upgrade_prior_fixture_graph_node_bodies(pool: &sqlx::PgPool) {
    use lash_core::session_graph::{SESSION_NODE_BODY_SCHEMA_VERSION, SessionNodeRecord};
    let rows: Vec<(String, Option<String>, String)> =
        sqlx::query_as("SELECT node_id, parent_node_id, node_json FROM lash_graph_nodes")
            .fetch_all(pool)
            .await
            .expect("read refusal fixture graph nodes");
    for (node_id, parent_node_id, node_json) in rows {
        let mut value: serde_json::Value =
            serde_json::from_str(&node_json).expect("parse refusal fixture node body");
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(SESSION_NODE_BODY_SCHEMA_VERSION))
        {
            continue;
        }
        value["schema_version"] = SESSION_NODE_BODY_SCHEMA_VERSION.into();
        let record = SessionNodeRecord::decode_storage_body(
            node_id.clone(),
            parent_node_id,
            &value.to_string(),
        )
        .expect("refusal fixture node payload must decode under the current generation");
        let restamped = record
            .encode_storage_body()
            .expect("re-encode refusal fixture node body");
        sqlx::query("UPDATE lash_graph_nodes SET node_json = $1 WHERE node_id = $2")
            .bind(&restamped)
            .bind(&node_id)
            .execute(pool)
            .await
            .expect("restamp refusal fixture node body");
    }
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

fn schema_process_registry_ddl() -> &'static str {
    let ddl = PostgresStorage::schema_ddl();
    let start = ddl
        .find("CREATE TABLE IF NOT EXISTS lash_process_change_clock (")
        .expect("schema DDL must declare the process registry");
    let end = ddl[start..]
        .find("CREATE TABLE IF NOT EXISTS lash_tool_intent_submissions (")
        .map(|offset| start + offset)
        .expect("process registry DDL must precede tool-intent submissions");
    &ddl[start..end]
}

fn schema_trigger_subscriptions_ddl() -> &'static str {
    let ddl = PostgresStorage::schema_ddl();
    let start = ddl
        .find("CREATE TABLE IF NOT EXISTS lash_trigger_subscriptions (")
        .expect("schema DDL must declare trigger subscriptions");
    let end = ddl[start..]
        .find("\n\n-- The named process-definition registry")
        .map(|offset| start + offset)
        .expect("trigger subscription DDL must precede the process-definition registry");
    &ddl[start..end]
}

fn schema_artifact_owner_ddl() -> &'static str {
    let ddl = PostgresStorage::schema_ddl();
    let start = ddl
        .find("CREATE TABLE IF NOT EXISTS lash_artifact_owners (")
        .expect("schema DDL must declare exact artifact ownership");
    let end = ddl[start..]
        .find("\n\n-- Seed rows.")
        .map(|offset| start + offset)
        .expect("artifact-owner DDL must precede schema seed rows");
    &ddl[start..end]
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
    let frame_node_id = lash_core::facade_support::frame_node_id(
        &SessionId::from(fixture::SESSION_ID),
        frame_key.as_str(),
    )
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

/// Re-encode the refusal fixture's graph-node bodies at the current
/// generation. Node-body decode is an exact-generation fence, so the bodies the
/// fixture predates would now refuse the session before hydration reaches the
/// component-v1 checkpoint the fixture exists to exercise.
///
/// The restamp goes through the production codec rather than editing the stamp
/// field alone: decoding each re-stamped body proves the retained payload
/// already matches the current shape, and `encode_storage_body` writes back
/// exactly the bytes this build would have produced.
async fn refresh_prior_fixture_node_bodies(pool: &sqlx::PgPool) {
    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT node_id, parent_node_id, node_json
           FROM lash_graph_nodes
          WHERE session_id = $1
          ORDER BY node_id",
    )
    .bind(fixture::SESSION_ID)
    .fetch_all(pool)
    .await
    .expect("read prior fixture graph-node bodies");
    assert_eq!(
        rows.len(),
        3,
        "the prior fixture carries one three-node frame"
    );
    for (node_id, parent_node_id, node_json) in rows {
        let mut body: serde_json::Value =
            serde_json::from_str(&node_json).expect("parse prior fixture node body");
        // This refusal fixture retains obsolete checkpoint component bytes, but
        // its surrounding conversation must use the current part shape so the
        // test reaches component hydration instead of failing at node decode.
        if let Some(parts) = body
            .pointer_mut("/event/Conversation/parts")
            .and_then(serde_json::Value::as_array_mut)
        {
            for part in parts {
                part.as_object_mut()
                    .expect("fixture part object")
                    .remove("prune_state");
            }
        }
        body["schema_version"] =
            serde_json::Value::from(lash_core::SESSION_NODE_BODY_SCHEMA_VERSION);
        let record = lash_core::SessionNodeRecord::decode_storage_body(
            node_id.clone(),
            parent_node_id,
            &body.to_string(),
        )
        .expect("prior fixture node body must match the current payload shape");
        let canonical = record
            .encode_storage_body()
            .expect("re-encode prior fixture node body at the current generation");
        sqlx::query("UPDATE lash_graph_nodes SET node_json = $1 WHERE node_id = $2")
            .bind(&canonical)
            .bind(&node_id)
            .execute(pool)
            .await
            .expect("write refreshed node body");
    }
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
         LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p lash-internal-postgres-store --test \
         durable_read_fixture regenerate_postgres_durable_fixture -- --ignored --exact"
    );
}

fn open_handles(storage: &PostgresStorage, timestamp_ms: u64) -> fixture::FixtureHandles {
    let clock = Arc::new(lash_core::testing::TestClock::new(timestamp_ms));
    let runtime = Arc::new(
        storage
            .session_store(fixture::SESSION_ID)
            .with_lease_clock_for_testing(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
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
            .with_lease_clock_for_testing(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );
    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
        runtime: runtime as Arc<dyn RuntimePersistence>,
        session_factory: session_factory as Arc<dyn SessionStoreFactory>,
        processes: Arc::clone(&processes) as Arc<dyn lash_core::ConformanceProcessRegistry>,
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
        .bind(fixture::FIXTURE_AWAIT_EVENT_SIGNING_SECRET.to_vec())
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
    .bind(lease.process_id.as_str())
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
