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

use lash_core_execution::{
    EffectHost, ProcessContinuationStore, ProcessExecutionEnvStore, RuntimePersistence,
    SessionStoreFactory, TriggerStore,
};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteProcessRegistry, SqliteSessionStoreFactory, SqliteTriggerStore, Store,
};
use serde::{Deserialize, Serialize};

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

const REBASED_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-97-e327bd63e/sqlite-expected.json",
];
const SETTLEMENT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-96-f9e0aa07d/sqlite-expected.json",
];
const REGENERATE_ENV: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES";
const EFFECT_OUTCOME_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-106-effect-outcome-predecessor/sqlite-expected.json",
];
const OBSERVER_SELECTION_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-98-observer-predecessor/sqlite-expected.json",
];
const CONSTRAINT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-102-379dda204/sqlite-expected.json",
];
const MESSAGE_BODY_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-96-4883e7a46/sqlite-expected.json",
];
const ENVELOPE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-95-e7d07c89b/sqlite-expected.json",
];
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
const PASSING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-84-9a8b048f3/sqlite-expected.json",
];
const CLOSING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-85-9b80fb5b7/sqlite-expected.json",
];
const PARTING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-86-a1cf357c7/sqlite-expected.json",
];
const FADING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-87-4c40db338/sqlite-expected.json",
];
const WANING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-88-9e92bc263/sqlite-expected.json",
];
const EBBING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-89-7e5feb69d/sqlite-expected.json",
];
const DWINDLING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-90-3cdb48643/sqlite-expected.json",
];
const SLIPPING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-91-45dbe5ab2/sqlite-expected.json",
];
const RECEDING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-92-eef96581a/sqlite-expected.json",
];
const SUBSIDING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-93-47ab59286/sqlite-expected.json",
];
const LAPSING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-94-b66a55237/sqlite-expected.json",
];
const DECLINING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-95-a596c2237/sqlite-expected.json",
];
const ABATING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-99-eeeedf38a/sqlite-expected.json",
];
const FLEETING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-100-failure-code-predecessor/sqlite-expected.json",
];
const EXPIRING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-101-f2a4770bd/sqlite-expected.json",
];
const FIG_3484_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-105-fig-3484/sqlite-expected.json",
];
const SETTLING_FROZEN_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-103-constraint-predecessor/sqlite-expected.json",
];
const USAGE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-104-f1d0c1d5f/sqlite-expected.json",
];
const RECEIPT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-107-061de77f7/sqlite-expected.json",
];
const SUBMISSION_DIGEST_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-108-cc0b9eecf/sqlite-expected.json",
];
const TOOL_RESULT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-109-b77f0ff6b/sqlite-expected.json",
];
const ATTACHMENT_BLOB_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-110-27c3a1b77/sqlite-expected.json",
];
const DRIVE_SET_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-111-56e34fddc/sqlite-expected.json",
];
const TURN_BOUND_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-112-18c87504b/sqlite-expected.json",
];
const CARRIER_CUTOVER_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-113-709567432/sqlite-expected.json",
];
const REPLAY_ORDINAL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-114-363e43724/sqlite-expected.json",
];
const GROUP_PROTOCOL_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-115-148f05d42/sqlite-expected.json",
];
const DRAIN_WAIT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-116-99467515b/sqlite-expected.json",
];
const BINDING_SET_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-117-2a5ea5306/sqlite-expected.json",
];
const DIVERGENCE_PARK_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-118-83bda2477/sqlite-expected.json",
];
const SESSION_INGRESS_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-119-5b8e9512f/sqlite-expected.json",
];
const PARK_FEED_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-120-e242928e4/sqlite-expected.json",
];
const NATIVE_CUT_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-121-298e495cd/sqlite-expected.json",
];
const ADMISSION_BASE_PREDECESSOR_EXPECTED_RELATIVE_PATHS: &[&str] = &[
    "../lash-core/tests/fixtures/durable-read-predecessors/schema-122-60e0e86b2/sqlite-expected.json",
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
         LASH_REGENERATE_DURABLE_READ_FIXTURES=1 kiln run //crates/lash-sqlite-store:durable_read_fixture__test -- \
         regenerate_sqlite_durable_fixture --ignored --exact"
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
        message.contains("supports schema version 89"),
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
        message.contains("supports schema version 89"),
        "open refusal must name the current schema boundary: {message}"
    );
    assert!(
        message.contains("reports version 38"),
        "open refusal must name the stale v38 fixture: {message}"
    );
}

/// FIG-1949 layer 2: the durable artifact-blob envelope dropped its
/// `descriptor` field under durable-core schema 74. A pre-74 database — one
/// whose blob rows still carry the field — must be refused at the version
/// boundary; new code never decodes the retired shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_v73_envelope_database_is_refused_before_blob_decode() {
    let fixture_dir = fixture_dir();
    let temp = tempfile::tempdir().expect("SQLite fixture tempdir");
    copy_sqlite_fixture(&fixture_dir, temp.path());
    let durable_core = temp.path().join("durable-core.db");
    let connection = rusqlite::Connection::open(&durable_core).expect("open copied fixture");

    // A blob row in the retired pre-74 envelope shape: the payload family was
    // restated inside the envelope before the pointer-table namespace became
    // its sole owner.
    #[derive(serde::Serialize)]
    struct PreBoundaryDescriptor {
        kind: &'static str,
    }
    #[derive(serde::Serialize)]
    struct PreBoundaryEnvelope {
        descriptor: PreBoundaryDescriptor,
        compression: &'static str,
        #[serde(with = "serde_bytes")]
        content: Vec<u8>,
    }
    let legacy_blob = rmp_serde::to_vec_named(&PreBoundaryEnvelope {
        descriptor: PreBoundaryDescriptor {
            kind: "CheckpointManifest",
        },
        compression: "None",
        content: b"legacy payload".to_vec(),
    })
    .expect("encode a pre-74 artifact-blob envelope");
    connection
        .execute(
            "INSERT OR REPLACE INTO blobs (hash, content) VALUES ('pre-74-blob', ?1)",
            rusqlite::params![legacy_blob],
        )
        .expect("insert pre-74 envelope blob");
    connection
        .pragma_update(None, "user_version", 73)
        .expect("stamp the pre-envelope-removal v73 boundary");
    drop(connection);

    let open_error = match Store::open(&durable_core).await {
        Err(error) => error,
        Ok(_) => panic!("a pre-74 durable core must be refused at the schema boundary"),
    };
    let message = open_error.to_string();
    assert!(
        message.contains("supports schema version 89"),
        "open refusal must name the current reject-and-recreate boundary: {message}"
    );
    assert!(
        message.contains("reports version 73"),
        "open refusal must name the pre-74 database: {message}"
    );
}

/// FIG-3544: durable-core schema 77 gives every pending turn input an
/// immutable submitted ingress and submission digest, which source-key replay
/// compares. A pre-77 database holds rows with neither, and the digest is a
/// Rust-computed value no DDL can backfill, so the whole database must be
/// refused at the version boundary rather than replayed against a missing
/// digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_v76_pending_input_database_is_refused_before_replay() {
    let fixture_dir = fixture_dir();
    let temp = tempfile::tempdir().expect("SQLite fixture tempdir");
    copy_sqlite_fixture(&fixture_dir, temp.path());
    let durable_core = temp.path().join("durable-core.db");
    let connection = rusqlite::Connection::open(&durable_core).expect("open copied fixture");
    // The pre-77 row shape: no immutable submission columns at all.
    connection
        .execute_batch(
            "ALTER TABLE pending_turn_inputs DROP COLUMN submitted_ingress_json;
             ALTER TABLE pending_turn_inputs DROP COLUMN submission_digest;
             INSERT INTO pending_turn_inputs (
                 input_id, session_id, source_key, ingress_json, state, input_json,
                 enqueued_at_ms
             )
             VALUES (
                 'ti:pre-77', 'pre-77-session', 'host:pre-77', '{\"scope\":\"next_turn\"}',
                 'deferred_next_turn', '{\"items\":[]}', 0
             );",
        )
        .expect("rewrite the pending-input table to its pre-77 shape");
    connection
        .pragma_update(None, "user_version", 76)
        .expect("stamp the pre-submission-digest v76 boundary");
    drop(connection);

    let open_error = match Store::open(&durable_core).await {
        Err(error) => error,
        Ok(_) => panic!("a pre-77 durable core must be refused at the schema boundary"),
    };
    let message = open_error.to_string();
    assert!(
        message.contains("supports schema version 89"),
        "open refusal must name the current reject-and-recreate boundary: {message}"
    );
    assert!(
        message.contains("reports version 76"),
        "open refusal must name the pre-77 database: {message}"
    );
}

/// FIG-3578: durable-core schema 78 adds `attachment_blobs`, where a SQLite
/// backend keeps its attachment bytes. A pre-78 catalog has no such table,
/// and an attachment store over it would fail at its first put, so the whole
/// database is refused at the version boundary rather than midwifed the table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_v77_catalog_without_attachment_blobs_is_refused() {
    let fixture_dir = fixture_dir();
    let temp = tempfile::tempdir().expect("SQLite fixture tempdir");
    copy_sqlite_fixture(&fixture_dir, temp.path());
    let durable_core = temp.path().join("durable-core.db");
    let connection = rusqlite::Connection::open(&durable_core).expect("open copied fixture");
    connection
        .execute_batch("DROP TABLE attachment_blobs;")
        .expect("rewrite the catalog to its pre-78 shape");
    connection
        .pragma_update(None, "user_version", 77)
        .expect("stamp the pre-attachment-blob v77 boundary");
    drop(connection);

    let open_error = match Store::open(&durable_core).await {
        Err(error) => error,
        Ok(_) => panic!("a pre-78 durable core must be refused at the schema boundary"),
    };
    let message = open_error.to_string();
    assert!(
        message.contains("supports schema version 89"),
        "open refusal must name the current reject-and-recreate boundary: {message}"
    );
    assert!(
        message.contains("reports version 77"),
        "open refusal must name the pre-78 database: {message}"
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
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(timestamp_ms));
    // Prime the durable core so its release stamp can be pinned before
    // anything is written.
    let core_path = root.join("durable-core.db");
    let priming_runtime = Store::open_with_clock(
        &core_path,
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await
    .expect("prime SQLite durable-core fixture schema");
    drop(priming_runtime);
    pin_release_stamp_instant(&core_path, timestamp_ms);
    let runtime = Arc::new(
        Store::open_with_clock(
            &core_path,
            Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
        )
        .await
        .expect("open SQLite durable-core fixture")
        .with_commit_count_seed_for_testing(0),
    );
    let processes = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &root.join("processes.db"),
            Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
            root.join("process-sessions"),
        )
        .await
        .expect("open SQLite process fixture"),
    );
    let triggers = Arc::new(
        SqliteTriggerStore::open_with_clock(
            &root.join("triggers.db"),
            Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
        )
        .await
        .expect("open SQLite trigger fixture")
        .with_incarnation_for_testing("durable-read-trigger-incarnation"),
    );
    let effect_path = root.join("effects.db");
    let priming_effects = SqliteEffectHost::open_with_clock(
        &effect_path,
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
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
            Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
        )
        .await
        .expect("open SQLite effect fixture"),
    );
    let session_factory = Arc::new(
        SqliteSessionStoreFactory::new(root)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>),
    );
    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
        process_lease_ttl_ms: fixture::PINNED_PROCESS_LEASE_TTL_MS,
        runtime: Arc::clone(&runtime) as Arc<dyn RuntimePersistence>,
        session_factory: session_factory as Arc<dyn SessionStoreFactory>,
        processes: Arc::clone(&processes)
            as Arc<dyn lash_core_execution::ConformanceProcessRegistry>,
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
    source_manifest_dir().join("../../fixtures/durable-read/v1/sqlite")
}

fn prior_component_fixture_dir() -> PathBuf {
    source_manifest_dir().join("../../fixtures/checkpoint-component-v1-refusal/sqlite")
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
        |root| PathBuf::from(root).join("crates/lash-sqlite-store"),
    )
}

#[test]
fn fixture_source_dir_resolves_bazel_workspace() {
    assert_eq!(
        source_manifest_dir_for(Some("/tmp/lash-fork".into())),
        PathBuf::from("/tmp/lash-fork/crates/lash-sqlite-store")
    );
    assert_eq!(
        source_manifest_dir_for(None),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    );
}

fn json_with_newline(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("encode SQLite fixture JSON");
    bytes.push(b'\n');
    bytes
}
