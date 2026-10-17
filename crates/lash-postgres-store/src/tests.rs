//! Crate-root tests for the PostgreSQL store wiring.
//!
//! Split out of `lib.rs` as a sibling of the `#[path]`-declared test modules
//! beside it, so the crate root stays the wiring surface it describes rather
//! than carrying its own suite inline.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core_store::store::RunStore as _;
use lash_core_store::store::SessionCatalogStore as _;
use lash_core_store::store::SessionHistoryStore as _;
use lash_core_store::store::TurnInputStore as _;
use lash_protocol_types::TurnId;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

#[tokio::test(flavor = "current_thread")]
async fn runtime_pool_failures_observe_the_injected_store_instruments() {
    let observed = lash_core::operational_metrics::TestMetrics::install();
    let runtime = lash_core::trace::TraceRuntime::new(Arc::new(lash_core_ids::clock::SystemClock));
    let observer = StoreObserver::new(runtime.metrics().clone());
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://fixture:fixture@127.0.0.1:1/fixture")
        .expect("a lazy fixture pool");
    pool.close().await;
    assert!(acquire_runtime_connection(&pool, &observer).await.is_err());
    assert_eq!(
        observed.histogram_count("lash.store.pool.acquire_wait.duration"),
        1
    );
    assert!(
        acquire_runtime_connection(&pool, &StoreObserver::default())
            .await
            .is_err()
    );
    assert_eq!(
        observed.histogram_count("lash.store.pool.acquire_wait.duration"),
        1
    );
}

async fn persisted_record_decode_store(
    storage: &PostgresStorage,
    label: &str,
) -> (SessionId, PostgresStore) {
    let session_id = SessionId::fixture(format!(
        "persisted-record-decode-{label}:{}",
        uuid::Uuid::new_v4()
    ));
    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit persisted-record decode session");
    let state = lash_core_store::session_state::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_store::session_state::RuntimeSessionState::fixture(
            lash_core_store::session_policy::SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
        )
    };
    store
        .commit_runtime_state(
            lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state),
        )
        .await
        .expect("seed persisted-record decode session");
    (session_id, store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_persisted_record_decode_classification_head_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres head decode classification: database URL is not set");
        return;
    };
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(isolated_database.url())
        .await
        .expect("connect persisted-record decode storage");

    let (head_session_id, head_store) = persisted_record_decode_store(&storage, "head").await;
    assert_eq!(
        sqlx::query("UPDATE lash_blobs SET content = '{'::bytea WHERE hash = (SELECT config_ref FROM lash_session_revisions JOIN lash_session_head USING (session_id, head_revision) WHERE session_id = $1)")
            .bind(head_session_id.as_str())
            .execute(storage.pool())
            .await
            .expect("corrupt head JSON")
            .rows_affected(),
        1
    );
    let head_error = head_store
        .load_session_head_meta(&head_session_id)
        .await
        .expect_err("malformed head JSON must refuse");
    assert!(
        matches!(
            head_error,
            StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                ..
            }
        ),
        "malformed Postgres head JSON must be stored-data corruption, got {head_error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_persisted_record_decode_classification_checkpoint_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres checkpoint decode classification: database URL is not set");
        return;
    };
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(isolated_database.url())
        .await
        .expect("connect persisted-record decode storage");

    let (checkpoint_session_id, checkpoint_store) =
        persisted_record_decode_store(&storage, "checkpoint").await;
    let checkpoint_ref: String =
        sqlx::query_scalar("SELECT checkpoint_ref FROM lash_session_head JOIN lash_session_revisions USING (session_id, head_revision) WHERE session_id = $1")
            .bind(checkpoint_session_id.as_str())
            .fetch_one(storage.pool())
            .await
            .expect("read checkpoint ref");
    assert_eq!(
        sqlx::query("UPDATE lash_blobs SET content = $1 WHERE hash = $2")
            .bind(vec![0xc1_u8])
            .bind(checkpoint_ref)
            .execute(storage.pool())
            .await
            .expect("corrupt checkpoint MessagePack")
            .rows_affected(),
        1
    );
    let checkpoint_error = checkpoint_store
        .load_session_window(
            &checkpoint_session_id,
            lash_core_store::store::WindowSelector::Current,
        )
        .await
        .expect_err("malformed checkpoint MessagePack must refuse");
    assert!(
        matches!(
            checkpoint_error,
            StoreError::StoredDataCorrupt {
                record_kind: "SessionCheckpoint",
                ..
            }
        ),
        "malformed Postgres checkpoint MessagePack must be stored-data corruption, got {checkpoint_error:?}"
    );
}

/// Seed one committed session carrying failure evidence, then splice an extra
/// receipt row into `lash_runtime_turn_commits` under the `bad-evidence-receipt`
/// operation key so a refusal can be asserted against that exact row.
async fn seed_failure_evidence_session(
    session_id: &str,
    bad_result_json: &str,
) -> (PostgresStorage, postgres_test_support::SharedDatabaseLock) {
    let database_url = postgres_test_support::database_url()
        .expect("receipt refusal tests require LASH_POSTGRES_DATABASE_URL");
    let database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = crate::testing::connect(&database_url)
        .await
        .expect("connect receipt-refusal storage");

    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(
                &SessionId::fixture(session_id),
            ),
        )
        .await
        .expect("bind receipt-refusal session");
    let state = lash_core_store::session_state::RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        ..lash_core_store::session_state::RuntimeSessionState::fixture(
            lash_core_store::session_policy::SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
        )
    };
    let mut commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state);
    commit.failure_evidence = vec![
        lash_core_store::turn_failure_evidence::TurnFailureEvidence {
            partial_output: Some(
                lash_core_store::turn_failure_evidence::TurnFailurePartialOutput::Complete {
                    text: "settled partial output".to_string(),
                },
            ),
            billed_usage: lash_protocol_types::session_model::LlmUsage {
                output_tokens: 3,
                ..Default::default()
            },
            refusal: lash_core_store::turn_failure_evidence::ChargeSafetyRefusalEvidence {
                denial_reason:
                    lash_protocol_types::llm::types::ChargeSafetyDenialReason::GuaranteeRequired,
                protocol_position: lash_protocol_types::llm::types::ProtocolPosition::OutputStarted,
                attempt_number: 1,
                attempt_count: 1,
            },
        },
    ];
    store
        .commit_runtime_state(commit)
        .await
        .expect("seed one failure-evidence receipt");

    let mut tx = storage
        .pool()
        .begin()
        .await
        .expect("begin bad receipt splice");
    let sequence = crate::session_factory::next_turn_change_sequence(&mut tx)
        .await
        .expect("allocate bad receipt sequence");
    sqlx::query(
        "INSERT INTO lash_runtime_turn_commits
         (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms, failure_evidence, change_seq, head_revision)
         SELECT $1, 'bad-evidence-receipt', 'bad-evidence-hash', $2, committed_at_ms + 1, TRUE, $3, head_revision + 1
         FROM lash_runtime_turn_commits
         WHERE session_id = $1",
    )
    .bind(session_id)
    .bind(bad_result_json)
    .bind(sequence)
    .execute(&mut *tx)
    .await
    .expect("splice the bad receipt row");
    tx.commit().await.expect("commit bad receipt splice");
    (storage, database_lock)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_failure_reopen_refuses_one_corrupt_evidence_receipt() {
    const SESSION_ID: &str = "failure-evidence-corrupt-receipt";
    let Some(_) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres receipt refusal: database URL is not set");
        return;
    };
    let (storage, _database_lock) =
        seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":"#).await;

    let error = storage
        .store()
        .load_failure_evidence_page(
            &SessionId::from(SESSION_ID),
            None,
            std::num::NonZeroU32::new(100).expect("nonzero page limit"),
        )
        .await
        .expect_err("a corrupt evidence receipt must refuse the page");
    assert!(
        matches!(
            &error,
            StoreError::StoredDataCorrupt {
                record_kind: "RuntimeCommitReceipt",
                message,
            } if message.contains(SESSION_ID) && message.contains("bad-evidence-receipt")
        ),
        "the corrupt receipt refusal must name the session and operation: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_failure_reopen_refuses_a_preversioned_receipt() {
    const SESSION_ID: &str = "failure-evidence-preversioned-receipt";
    let Some(_) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres receipt refusal: database URL is not set");
        return;
    };
    let (storage, _database_lock) =
        seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":[{}]}"#).await;

    let error = storage
        .store()
        .load_failure_evidence_page(
            &SessionId::from(SESSION_ID),
            None,
            std::num::NonZeroU32::new(100).expect("nonzero page limit"),
        )
        .await
        .expect_err("an unversioned receipt must refuse the page");
    assert!(
        matches!(
            &error,
            StoreError::MissingRecordSchemaVersion {
                record_kind: "RuntimeCommitReceipt",
                ..
            }
        ),
        "the pre-versioned receipt refusal must be typed: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_failure_reopen_refuses_a_newer_receipt_version() {
    const SESSION_ID: &str = "failure-evidence-newer-receipt";
    let Some(_) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres receipt refusal: database URL is not set");
        return;
    };
    let newer = lash_core_store::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION + 1;
    let bad_json = format!(r#"{{"schema_version":{newer},"failure_evidence":[{{}}]}}"#);
    let (storage, _database_lock) = seed_failure_evidence_session(SESSION_ID, &bad_json).await;

    let error = storage
        .store()
        .load_failure_evidence_page(
            &SessionId::from(SESSION_ID),
            None,
            std::num::NonZeroU32::new(100).expect("nonzero page limit"),
        )
        .await
        .expect_err("a newer receipt version must refuse the page");
    assert!(
        matches!(
            &error,
            StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "RuntimeCommitReceipt",
                actual,
                expected,
            } if *actual == newer
                && *expected == lash_core_store::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
        ),
        "the newer-version receipt refusal must be the typed version error: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_session_store_defers_missing_identity_validation() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping direct-session-store contract: database URL is not set");
        return;
    };
    let database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("connect direct-session-store contract storage");
    let missing = SessionId::from("missing");
    let store = storage.store();

    assert!(matches!(
        store.load_session_head_meta(&missing).await,
        Ok(None)
    ));
    assert!(matches!(store.load_session_meta(&missing).await, Ok(None)));
    assert_eq!(
        store
            .admit_session(
                &lash_core_execution::testing::store_fixtures::root_session_request(&missing)
            )
            .await
            .expect("admit missing direct-constructor session"),
        lash_core_store::store::SessionAdmission::Created
    );
    // FIG-4951: fixture setup must preserve the seeded feed clock. A missing
    // row refuses allocation just like an exhausted sequence, poisoning every
    // subsequent commit that shares this database.
    let mut tx = storage
        .pool()
        .begin()
        .await
        .expect("begin fixture clock probe");
    assert_eq!(
        crate::session_factory::next_turn_change_sequence(&mut tx)
            .await
            .expect("the direct-constructor fixture retains a usable turn clock"),
        1
    );
    tx.rollback().await.expect("roll back fixture clock probe");
}

#[tokio::test]
async fn postgres_graph_generation_uniqueness_is_typed() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping graph-generation error proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = crate::testing::connect(&database_url)
        .await
        .expect("connect graph-generation error storage");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let session_id = SessionId::fixture(format!("postgres-generation-collision:{nonce}"));
    let first_node = format!("generation-node-a:{nonce}");
    let second_node = format!("generation-node-b:{nonce}");
    sqlx::query(
        "INSERT INTO lash_graph_nodes
         (session_id, node_id, parent_node_id, generation, frame_node_id, node_json, body_bytes)
         VALUES ($1, $2, NULL, 3, $2, '{}', 2)",
    )
    .bind(session_id.as_str())
    .bind(&first_node)
    .execute(storage.pool())
    .await
    .expect("seed graph-generation uniqueness fixture");
    let inserted: Vec<String> = sqlx::query_scalar(
        crate::session_sql::session_sql()
            .graph_postgres
            .insert_batch
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(vec![second_node.clone()])
    .bind(vec![Option::<String>::None])
    .bind(vec![3_i64])
    .bind(vec![second_node.clone()])
    .bind(vec![2_i64])
    .bind(vec!["{}".to_owned()])
    .fetch_all(storage.pool())
    .await
    .expect("a taken generation leaves the node out of its batch");
    assert!(
        inserted.is_empty(),
        "the node at a taken generation was inserted"
    );
    let mut connection = storage.pool().acquire().await.expect("a connection");
    let error = graph_node_collision(
        &mut connection,
        &session_id,
        3,
        &lash_protocol_types::NodeId::fixture(second_node.as_str()),
    )
    .await;
    assert!(matches!(
        error,
        StoreError::GraphGenerationCollision {
            session_id: ref actual_session_id,
            generation: 3
        } if actual_session_id == session_id
    ));
    sqlx::query("DELETE FROM lash_graph_nodes WHERE session_id = $1")
        .bind(session_id.as_str())
        .execute(storage.pool())
        .await
        .expect("clean graph-generation uniqueness fixture");
}

lash_conformance::checkpoint_admission_probe_tests!({
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres checkpoint counter: database URL is not set");
        return;
    };
    let database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = crate::testing::connect(&database_url)
        .await
        .expect("connect checkpoint counter storage");
    let session_id = SessionId::fixture(format!(
        "postgres-checkpoint-counter:{}",
        std::process::id()
    ));
    let store = Arc::new(storage.store());
    let counting_store = Arc::clone(&store);
    let teardown_session = session_id.clone();
    (
        database_lock,
        store as Arc<dyn lash_core_store::store::RuntimeStore>,
        session_id,
        move || counting_store.checkpoint_admission_counts(),
        async move {
            storage
                .session_store_factory()
                .delete_session(&teardown_session)
                .await
                .expect("delete checkpoint counter session");
        },
    )
});

/// Arming a delete and a writer taking the digest back are the two halves of
/// the same CAS: run concurrently against PostgreSQL, at most one of them
/// may win.
///
/// This is the law that catches a transition running bare on the pool
/// instead of inside a transaction under the per-digest advisory key. With
/// `arm_attachment_delete` unfenced, its `UPDATE` can commit *inside* the
/// writer's open transaction — after the writer read `condemned` and before
/// it deleted the row — so the writer erases a `deleting` row, is granted,
/// and puts bytes into a delete that is already in flight. The post-delete
/// probe is no defence against that: it only fires once the bytes are gone.
///
/// Multi-threaded on purpose: the writer and the sweeper half must really
/// interleave, so each runs as its own task while the pool's IO driver keeps
/// running alongside them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arming_a_delete_and_a_concurrent_writer_never_both_win() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres attachment fence race proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = crate::testing::connect(&database_url)
        .await
        .expect("connect attachment fence database");
    let session_id = SessionId::fixture(format!(
        "postgres-attachment-fence-race:{}",
        std::process::id()
    ));
    let store = std::sync::Arc::new(storage.store());
    let factory = storage.session_store_factory();
    let attachment_id = lash_core_store::attachments::content_id(
        format!("fence-race-{}", std::process::id()).as_bytes(),
    );
    sqlx::query("DELETE FROM lash_attachment_condemnations WHERE attachment_id = $1")
        .bind(attachment_id.as_str())
        .execute(storage.pool())
        .await
        .expect("clear condemnation fixture");
    let intent = {
        let session_id = session_id.clone();
        let attachment_id = attachment_id.clone();
        move || lash_core_store::store::AttachmentWrite {
            attachment_id: attachment_id.clone(),
            claim: lash_core_store::artifact_referrer::ReferrerClaim::unguarded(
                lash_core_store::artifact_referrer::ArtifactReferrer::Session(session_id.clone()),
            )
            .expect("claim"),
        }
    };

    crate::attachments::FENCE_WRITER_WINDOW_DELAY_MS
        .store(20, std::sync::atomic::Ordering::Relaxed);

    let pass = lash_core_store::attachments::AttachmentRootSet::begin_attachment_sweep(&factory)
        .await
        .expect("open an attachment sweep pass");
    // Both orderings, every round: the fixed code holds for all of them.
    for round in 0..12 {
        assert_eq!(
            lash_core_store::attachments::AttachmentRootSet::condemn_attachment(
                &factory,
                &attachment_id,
                &pass
            )
            .await
            .expect("condemn"),
            lash_core_store::store::AttachmentCondemnation::Condemned,
            "round {round}: the digest must start each round rootless and free"
        );

        let writer = tokio::spawn({
            let store = std::sync::Arc::clone(&store);
            let intent = intent.clone();
            async move {
                lash_core_store::store::AttachmentReferrers::begin_attachment_write(
                    &*store,
                    &(intent()),
                )
                .await
            }
        });
        if round % 2 == 1 {
            // Alternate the stagger so both orderings are exercised: on odd
            // rounds the writer reaches its condemnation read first (the
            // interleaving an unfenced `arm` corrupts), on even rounds the
            // sweeper arms first. The pacing widens a window; it decides no
            // outcome, and the invariant below holds for either ordering.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let armed = lash_core_store::attachments::AttachmentRootSet::arm_attachment_delete(
            &factory,
            &attachment_id,
            &pass,
        )
        .await
        .expect("arm");
        let fence = writer.await.expect("join writer").expect("fenced write");

        let contains_ref = lash_core_store::store::AttachmentReferrers::attachment_referrers(
            &*store,
            &attachment_id,
        )
        .await
        .map(|refs| !refs.is_empty())
        .expect("contains_ref");
        match (armed, fence) {
            // The sweeper won: the delete is armed and the writer parked
            // without recording anything, so no bytes can land inside it.
            (
                lash_core_store::store::AttachmentDeleteArming::Armed,
                lash_core_store::store::AttachmentWriteFence::ReclamationInFlight,
            ) => {
                assert!(
                    !contains_ref,
                    "round {round}: a parked writer records no intent"
                );
            }
            // The writer won: it took the digest back before the delete was
            // armed, and the sweeper issues no delete at all.
            (
                lash_core_store::store::AttachmentDeleteArming::Revoked,
                lash_core_store::store::AttachmentWriteFence::Granted(permit),
            ) => {
                assert!(
                    contains_ref,
                    "round {round}: a granted writer records its intent"
                );
                let completed_intent = intent();
                lash_core_store::store::AttachmentReferrers::complete_attachment_write(
                    &*store,
                    &completed_intent,
                    permit,
                )
                .await
                .expect("complete the winning writer");
            }
            (armed, fence) => panic!(
                "round {round}: arming and the writer must never both win \
                 (arm = {armed:?}, writer = {fence:?}); bytes would land inside an \
                 in-flight delete"
            ),
        }

        lash_core_store::attachments::AttachmentRootSet::settle_attachment_condemnation(
            &factory,
            &attachment_id,
            &pass,
            lash_core_store::store::AttachmentCondemnationSettlement::Spared,
        )
        .await
        .expect("spare");
        if contains_ref {
            lash_core_store::store::AttachmentReferrers::forget_attachment_ref(
                &*store,
                &intent().claim.referrer(),
                &attachment_id,
            )
            .await
            .expect("forget the ref");
        }
    }

    crate::attachments::FENCE_WRITER_WINDOW_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    factory
        .delete_session(&session_id)
        .await
        .expect("delete fence session");
}

#[tokio::test]
async fn attachment_gc_refuses_an_empty_postgres_root_database() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping empty Postgres attachment-root proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = crate::testing::connect(&database_url)
        .await
        .expect("connect empty attachment-root database");
    sqlx::query("DELETE FROM lash_attachment_referrer_edges")
        .execute(storage.pool())
        .await
        .expect("make the configured Postgres manifest empty");
    let wrong_factory = storage.session_store_factory();

    let live_backend = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory store set");
    let live_store = live_backend.open_store().await.expect("open live catalog");
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("postgres-wrong-database-live-attachment"),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    };
    live_store
        .admit_session(&request)
        .await
        .expect("admit live root authority");
    let blobs = tempfile::tempdir().expect("attachment directory");
    let backend = lash_sqlite_store::SqliteStoreSet::open(
        (blobs.path()).join("attachments.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("SQLite attachment store")
    .attachment_store();
    let attachment = lash_core_store::attachments::AttachmentStore::put(
        backend.as_ref(),
        b"postgres-live-committed-blob".to_vec(),
        lash_protocol_types::AttachmentCreateMeta::new(
            lash_protocol_types::MediaType::parse("application/octet-stream").expect("media type"),
            None,
            Some("live".to_string()),
        ),
    )
    .await
    .expect("put shared backend blob");
    let live_intent = lash_core_store::store::AttachmentWrite {
        attachment_id: attachment.id.clone(),
        claim: lash_core_store::artifact_referrer::ReferrerClaim::unguarded(
            lash_core_store::artifact_referrer::ArtifactReferrer::Session(
                request.session_id.clone(),
            ),
        )
        .expect("claim"),
    };
    let lash_core_store::store::AttachmentWriteFence::Granted(live_permit) =
        lash_core_store::store::AttachmentReferrers::begin_attachment_write(
            &*live_store,
            &(live_intent.clone()),
        )
        .await
        .expect("begin live attachment write")
    else {
        panic!("a free digest must grant its writer");
    };
    lash_core_store::store::AttachmentReferrers::complete_attachment_write(
        &*live_store,
        &live_intent,
        live_permit,
    )
    .await
    .expect("stamp live attachment upload");
    lash_core_store::store::AttachmentReferrers::acquire_attachment_refs(
        &*live_store,
        &lash_core_store::artifact_referrer::ReferrerClaim::unguarded(
            lash_core_store::artifact_referrer::ArtifactReferrer::Session(
                request.session_id.clone(),
            ),
        )
        .expect("claim"),
        std::slice::from_ref(&attachment.id),
    )
    .await
    .expect("commit live attachment ref");

    let result = lash_core_store::attachments::reclaim_unreferenced_attachments(
        &wrong_factory,
        backend.as_ref(),
        lash_core_store::attachments::AttachmentReclamationPolicy::new(
            0,
            lash_core_store::attachments::EmptyRootSetPolicy::Refuse,
        ),
    )
    .await;

    let failure = result.expect_err("an empty Postgres root database must refuse deletion");
    assert_eq!(
        failure.refusal(),
        Some(&lash_core_store::store::MaintenanceRefusal::EmptyRootSetUnauthorized),
        "an empty Postgres root database must refuse deletion: {failure:?}"
    );
    assert_eq!(
        failure.partial.scanned_blob_count, 1,
        "the refusal must carry the report accumulated before it: {failure:?}"
    );
    lash_core_store::attachments::AttachmentStore::get(
        backend.as_ref(),
        &attachment.id,
        32 * 1024 * 1024,
    )
    .await
    .expect("live committed blob survives the refused sweep");
}

/// A session over `storage` with a committed head and one next-turn input
/// bound to `run`: the fixture the settlement laws below start from.
async fn admitted_input_fixture(
    storage: &PostgresStorage,
    label: &str,
    run: &TurnId,
) -> (
    PostgresStore,
    lash_core_store::session_state::RuntimeSessionState,
    lash_protocol_types::InputId,
) {
    let session_id = SessionId::fixture(format!("{label}:{}", uuid::Uuid::new_v4()));
    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit the fixture session");
    let mut state = lash_core_store::session_state::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_store::session_state::RuntimeSessionState::fixture(
            lash_core_store::session_policy::SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
        )
    };
    let seeded = store
        .commit_runtime_state(
            lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state),
        )
        .await
        .expect("seed the fixture head");
    state.head_revision = seeded.head_revision;
    let input = store
        .enqueue_pending_turn_input(
            lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
                &session_id,
                lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
                lash_core_store::turn_input_vocabulary::TurnInput::text("the admitted input"),
            ),
        )
        .await
        .expect("enqueue the fixture input");
    store
        .bind_run_inputs(&session_id, run, std::slice::from_ref(&input.input_id))
        .await
        .expect("bind the fixture input to its run");
    // The row as the session mail drain's admission leaves it: bound to the
    // run that admitted it.
    sqlx::query(
        "UPDATE lash_pending_turn_inputs SET admitted_run = $3, admitted_by = $3
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(session_id.as_str())
    .bind(input.input_id.as_str())
    .bind(run.as_str())
    .execute(storage.pool())
    .await
    .expect("admit the fixture input");
    (store, state, input.input_id)
}

/// A settlement locks the row it settles: the verdict reads the row under
/// `FOR UPDATE`, so no concurrent rebind can move it between that read and
/// the settling write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_settlement_locks_the_admitted_row() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres settlement row lock: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = crate::testing::connect(&database_url)
        .await
        .expect("connect settlement-lock storage");
    let run = TurnId::from("settlement-lock-run");
    let (_store, state, input_id) =
        admitted_input_fixture(&storage, "postgres-settlement-lock", &run).await;

    let mut settling = storage.pool().begin().await.expect("begin settling tx");
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .select_by_id_for_update
            .sql(),
    )
    .bind(state.session_id.as_str())
    .bind(input_id.as_str())
    .fetch_one(&mut *settling)
    .await
    .expect("lock the admitted row");
    let mut rebinder = storage.pool().begin().await.expect("begin rebinding tx");
    sqlx::query("SET LOCAL lock_timeout = '50ms'")
        .execute(&mut *rebinder)
        .await
        .expect("bound the rebinder's lock wait");
    let blocked = sqlx::query(
        "UPDATE lash_pending_turn_inputs SET admitted_run = 'another-run', admitted_by = 'another-run'
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(state.session_id.as_str())
    .bind(input_id.as_str())
    .execute(&mut *rebinder)
    .await
    .expect_err("the settlement's row lock must block a rebind");
    assert_eq!(
        blocked.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("55P03")),
        "the rebind must fail specifically on the held row lock: {blocked}"
    );
    settling
        .rollback()
        .await
        .expect("release the settling lock");
    rebinder
        .rollback()
        .await
        .expect("roll back the timed-out rebind");
}

/// FIG-3927: the settlement verdict runs first, and it — not the write's
/// rows-affected — is what refuses a row another run holds.
///
/// The two paths are distinguishable in the error itself. The verdict reads
/// the locked row, so its `IngressRowNotAdmitted` names the run that holds
/// the row. The write-only backstop has no row to read, so its refusal
/// carries `None`. Asserting the populated field is therefore proof of
/// ordering, not just of refusal. The refused commit moves no head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_settlement_verdict_decides_before_the_settlement_write() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres settlement-order law: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = crate::testing::connect(&database_url)
        .await
        .expect("connect settlement-order storage");
    let run = TurnId::from("settlement-order-run");
    let (store, state, input_id) =
        admitted_input_fixture(&storage, "postgres-settle-order", &run).await;
    // A fork rebinds a row to its own run; the commit of the run that
    // admitted it then names a row it no longer holds.
    sqlx::query(
        "UPDATE lash_pending_turn_inputs SET admitted_run = 'rebinding-root', admitted_by = 'rebinding-root'
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(state.session_id.as_str())
    .bind(input_id.as_str())
    .execute(storage.pool())
    .await
    .expect("rebind the admitted row");

    let mut settlement = lash_core_store::store::IngressSettlement::new(run.clone());
    settlement.completed_inputs.push(
        lash_core_store::turn_input_vocabulary::TurnInputCompletion {
            session_id: state.session_id.clone(),
            data: lash_core_store::turn_input_vocabulary::TurnInputCompletionData {
                input_ids: vec![input_id],
                applications: Vec::new(),
            },
        },
    );
    let error = store
        .commit_runtime_state(
            lash_core_execution::testing::store_fixtures::settling_commit_for_test(
                lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state),
                settlement,
            ),
        )
        .await
        .expect_err("the verdict must refuse a row another run holds before any write");
    let StoreError::IngressRowNotAdmitted {
        run: ref refused_run,
        ref admitted_run,
        ..
    } = error
    else {
        panic!("unexpected variant: {error:?}");
    };
    assert_eq!(*refused_run, run);
    assert_eq!(
        admitted_run.as_ref().map(TurnId::as_str),
        Some("rebinding-root"),
        "the refusal must name the run the locked read observed, which only the verdict can see"
    );
    let head_revision = store
        .load_session_head_meta(&state.session_id)
        .await
        .expect("read the head after the refusal")
        .expect("the head exists")
        .head_revision;
    assert_eq!(
        head_revision, state.head_revision,
        "a refused settlement must not move the session head"
    );
}

/// A commit's session-meta preflight never holds a pooled connection while
/// it acquires another (FIG-5237): on a pool of one connection it answers
/// instead of waiting out the acquire timeout on itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_commit_meta_preflight_answers_on_a_pool_of_one_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres pool-of-one preflight: database URL is not set");
        return;
    };
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let mut config = crate::testing::work_pool_of(1);
    config.roles.work.acquire_timeout = Duration::from_secs(5);
    let storage = crate::testing::connect_with(isolated_database.url(), &config)
        .await
        .expect("connect pool-of-one storage");
    let session_id = SessionId::fixture(format!("pool-of-one:{}", uuid::Uuid::new_v4()));
    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit the pool-of-one session");
    let meta = lash_core_store::store::SessionCommitStore::load_session_meta_for_commit(
        &store,
        &session_id,
    )
    .await
    .expect("the preflight answers on a pool of one");
    assert!(meta.is_some(), "the admitted session has its metadata");
}

/// Per-label costs include the transaction envelope and delegated domain SQL,
/// returned column bytes, and the actual number of outcome records batched.
#[tokio::test(flavor = "current_thread")]
async fn durable_labels_observe_physical_cost_and_group_members() {
    use lash_durable::domain::{DomainWrite, Ordinal, OwnerKey, RunFactKind, RunFactWrite, RunSeq};
    use lash_durable::{
        ActorKey, CommitLabel, DurableStore, FormatSet, MailTx, NodeId, NodeSpec, Release,
    };
    let url =
        crate::postgres_test_support::database_url().expect("this cost law requires PostgreSQL");
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let runtime = lash_core::trace::TraceRuntime::new(Arc::new(lash_core_ids::clock::SystemClock));
    let storage = PostgresStorage::connect(
        &crate::PostgresEndpoints::from_url(database.url()).expect("parse the database URL"),
        &crate::testing::fixture_config(),
        StoreObserver::new(runtime.metrics().clone()),
    )
    .await
    .expect("open observed store");
    let store = storage.durable_store();
    let formats = FormatSet::new("cost-law");
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("cost-law"),
            decodes: vec![formats.clone()],
            serves: Vec::new(),
            ttl_millis: 15_000,
        })
        .await
        .expect("register");
    let actor = ActorKey::session("cost-law").expect("actor key");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats);
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .expect("create");
    let claimed = store.claim(&node, 1).await.expect("claim");
    let mut tx = store
        .begin(&actor, claimed[0].epoch)
        .await
        .expect("open actor");
    for ordinal in 0..2 {
        // Each fact's record is a node the commit installs with it.
        let mut staging = lash_core_store::checkpoint_graph::GraphStaging::new(
            lash_core_store::checkpoint_graph::EncodeBudget::new(
                lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
            ),
        );
        let mut body = lash_core_store::checkpoint_graph::TokenWriter::new();
        body.text(&format!("outcome {ordinal}"));
        let record = staging
            .encode(
                lash_core_store::checkpoint_graph::NodeKind::ROUND_FACT,
                lash_core_store::checkpoint_graph::TypedBody::logical(&body.into_bytes()),
            )
            .expect("the record stages");
        tx.stage_all(staging);
        tx.write(DomainWrite::RunFact(RunFactWrite::Append {
            owner: OwnerKey::Turn(
                SessionId::from("cost-law"),
                lash_protocol_types::TurnId::from("run"),
            ),
            run: RunSeq(0),
            ordinal: Ordinal(ordinal),
            kind: RunFactKind::XOutcome,
            call: None,
            record,
        }));
    }
    let observed = lash_core::operational_metrics::TestMetrics::install();
    crate::observed_sql::RECEIPTS
        .scope(std::cell::RefCell::new(Vec::new()), async {
            store
                .commit(tx, CommitLabel::ROUND_OUTCOME)
                .await
                .expect("commit grouped outcomes");
            let mut tx = store
                .begin(&actor, claimed[0].epoch)
                .await
                .expect("open turn commit");
            tx.give_up(Release::Idle);
            store
                .commit(tx, CommitLabel::TURN_COMMIT)
                .await
                .expect("commit turn");
            crate::observed_sql::RECEIPTS.with(|receipts| {
                let receipts = receipts.borrow();
                assert_eq!(receipts.len(), 2);
                for (index, (label, outcome, cost)) in receipts.iter().enumerate() {
                    assert_eq!(label, ["round.outcome", "turn.commit"][index]);
                    assert_eq!(*outcome, "success");
                    assert!(cost.acquire_wait > Duration::ZERO);
                    assert!(cost.transaction_duration > cost.lock_statement_elapsed);
                    assert!(cost.lock_statement_elapsed > Duration::ZERO);
                    assert!(cost.returned_bytes > 0);
                    assert_eq!(cost.group_commit_members, [2, 0][index]);
                    // Each commit is one call (FIG-5936): its envelope, the
                    // grouped outcomes' graph nodes and their facts' appends
                    // all run in it.
                    assert_eq!(cost.sql_statements, [1, 1][index]);
                    eprintln!("{label}: {cost:?}");
                }
            });
        })
        .await;
    for name in [
        "lash.durable.commit.acquire_wait.duration",
        "lash.durable.commit.transaction.duration",
        "lash.durable.commit.sql_statements",
        "lash.durable.commit.returned_bytes",
        "lash.durable.commit.lock_statement_elapsed",
        "lash.durable.commit.group_commit.members",
    ] {
        assert_eq!(observed.histogram_count(name), 2, "{name}");
    }
}

/// D-RUNREPORT: terminal windows survive later head writes on PostgreSQL.
#[tokio::test]
async fn a_terminal_window_is_unchanged_by_later_commits() {
    let database_url = crate::testing::required_database_url();
    let database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("connect");
    let (session, store) = persisted_record_decode_store(&storage, "terminal-window").await;
    let run = TurnId::fixture("terminal-window");
    store
        .end_command_run(&session, &run, 1)
        .await
        .expect("terminal");
    let first = store
        .load_session_window(
            &session,
            lash_core_store::store::WindowSelector::Terminal(run.clone()),
        )
        .await
        .expect("terminal window")
        .expect("terminal has a window");
    let mut later = lash_core_store::store::window_state(
        first.clone(),
        lash_core_store::store::FleetFormat::current(),
    )
    .expect("stored state")
    .state;
    later.turn_index += 1;
    store
        .commit_runtime_state(
            lash_core_store::store::RuntimeCommit::persisted_state_for_test(&later),
        )
        .await
        .expect("later commit");
    let again = store
        .load_session_window(
            &session,
            lash_core_store::store::WindowSelector::Terminal(run),
        )
        .await
        .expect("terminal window")
        .expect("terminal has a window");
    assert_eq!(
        serde_json::to_string(&first).expect("serialize"),
        serde_json::to_string(&again).expect("serialize")
    );
}
