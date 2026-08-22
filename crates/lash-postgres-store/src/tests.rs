//! Crate-root tests for the PostgreSQL store wiring.
//!
//! Split out of `lib.rs` as a sibling of the `#[path]`-declared test modules
//! beside it, so the crate root stays the wiring surface it describes rather
//! than carrying its own suite inline.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

struct WarningCounter(Arc<AtomicUsize>);

impl<S> Layer<S> for WarningCounter
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[test]
fn unwired_process_registry_factory_warns() {
    let warning_count = Arc::new(AtomicUsize::new(0));
    let subscriber = Registry::default().with(WarningCounter(Arc::clone(&warning_count)));

    tracing::subscriber::with_default(subscriber, warn_postgres_process_registry_not_wired);

    assert_eq!(
        warning_count.load(Ordering::Relaxed),
        1,
        "the legacy factory must warn that process-owner liveness is not wired"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_session_store_defers_missing_identity_validation() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping direct-session-store contract: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect direct-session-store contract storage");
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list Lash tables for direct-constructor test reset");
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(storage.pool())
        .await
        .expect("reset direct-constructor test tables");
    let store = storage.session_store("missing");

    assert!(matches!(store.load_session_head_meta().await, Ok(None)));
    assert!(matches!(store.load_session_meta().await, Ok(None)));
    assert_eq!(
        store
            .admit_and_bind_session(&lash_core::SessionBinding::root("missing"))
            .await
            .expect("admit missing direct-constructor session"),
        lash_core::SessionAdmission::Created
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_unbound_session_meta_refuses_ambiguous_resolution() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping unbound session-meta refusal: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect unbound session-meta storage");
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list Lash tables for unbound session-meta reset");
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(storage.pool())
        .await
        .expect("reset unbound session-meta tables");
    for session_id in ["unbound-session-meta-a", "unbound-session-meta-b"] {
        sqlx::query(
            "INSERT INTO lash_session_meta
             (session_id, relation_kind, observer_intent_depth)
             VALUES ($1, 'root', 0)",
        )
        .bind(session_id)
        .execute(storage.pool())
        .await
        .unwrap_or_else(|error| panic!("seed `{session_id}` metadata: {error}"));
    }

    lash_core::testing::conformance::unbound_session_meta_refuses_ambiguous_resolution(
        "PostgreSQL",
        crate::session_meta::load_session_meta(storage.pool(), None),
    )
    .await;
}

#[tokio::test]
async fn one_id_selected_drain_touches_at_most_four_queue_rows() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping selected-drain plan proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect selected-drain plan storage");
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(storage.pool())
        .await
        .expect("enable pg_stat_statements for selected-drain plan proof");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let session_id = format!("selected-plan-session:{nonce}");
    let batch_prefix = format!("selected-plan-batch:{nonce}:");
    let source_prefix = format!("selected-plan-source:{nonce}:");
    sqlx::query(
        "INSERT INTO lash_queued_work_batches
         (batch_id, session_id, source_key, delivery_policy, work_kind,
          authority_json, merge_key, available_at_ms, enqueued_at_ms)
         SELECT $1 || value::text, $2, $3 || value::text,
                'earliest_safe_boundary', 'turn', '{}', NULL, 0, 1
         FROM generate_series(1, 10000) AS value",
    )
    .bind(&batch_prefix)
    .bind(&session_id)
    .bind(&source_prefix)
    .execute(storage.pool())
    .await
    .expect("seed 10,000 ready selected-drain batches");
    sqlx::query(
        "INSERT INTO lash_queued_work_items (batch_id, item_index, item_id, payload_json)
         SELECT $1 || value::text, 0, $1 || value::text || ':item:0',
                '{\"type\":\"agent_frame_task\",\"frame_id\":\"selected-plan\",\"task\":\"selected plan row\"}'
         FROM generate_series(1, 10000) AS value",
    )
    .bind(&batch_prefix)
    .execute(storage.pool())
    .await
    .expect("seed selected-drain batch payloads");
    sqlx::query("ANALYZE lash_queued_work_batches")
        .execute(storage.pool())
        .await
        .expect("analyze selected-drain plan fixture");

    let store = storage.session_store(&session_id);
    let owner = LeaseOwnerIdentity::opaque(
        "selected-plan-owner",
        format!("selected-plan-owner:{nonce}"),
    );
    let lease = store
        .try_claim_session_execution_lease(
            &session_id,
            &owner,
            "schema-congruence-test-executor",
            60_000,
        )
        .await
        .expect("claim selected-drain plan lease")
        .acquired()
        .expect("selected-drain plan lane is free");
    sqlx::query("SELECT pg_stat_statements_reset()")
        .execute(storage.pool())
        .await
        .expect("reset selected-drain statement statistics");
    let selected_batch_id = format!("{batch_prefix}5000");
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&selected_batch_id),
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim one selected row from 10,000")
        .expect("selected row is claimable");
    let selected_source_key = format!("{source_prefix}5000");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.source_key.as_deref())
            .collect::<Vec<_>>(),
        vec![Some(selected_source_key.as_str())]
    );
    let measured_queue_rows: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(rows), 0)::bigint
         FROM pg_stat_statements
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND query LIKE '%lash_queued_work_batches%'
           AND query NOT LIKE '%pg_stat_statements%'",
    )
    .fetch_one(storage.pool())
    .await
    .expect("measure selected-drain queue rows");
    assert!(
        measured_queue_rows <= 4,
        "one-ID selected drain may return or lock at most 4 queue rows, measured {measured_queue_rows}"
    );

    store
        .abandon_queued_work_claim(&claim)
        .await
        .expect("abandon selected-drain plan claim");
    store
        .release_session_execution_lease(&lease.completion())
        .await
        .expect("release selected-drain plan lease");
    sqlx::query("DELETE FROM lash_queued_work_batches WHERE session_id = $1")
        .bind(&session_id)
        .execute(storage.pool())
        .await
        .expect("remove selected-drain plan fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_commits_return_one_typed_head_revision_conflict() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping concurrent first-commit proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect concurrent first-commit storage");
    let factory = storage.session_store_factory();
    let session_id = format!("postgres-first-commit-race:{}", uuid::Uuid::new_v4());
    let request = SessionStoreCreateRequest {
        session_id: session_id.clone(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let first_store = factory
        .create_store(&request)
        .await
        .expect("create first racing handle");
    let second_store = factory
        .create_store(&request)
        .await
        .expect("create second racing handle");
    let mut first_state = lash_core::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core::RuntimeSessionState::new(request.policy.clone())
    };
    first_state.ensure_agent_frame_initialized();
    let second_state = first_state.clone();
    let (first_commit, _) = lash_core::RuntimeCommit::persisted_state_for_test(&first_state, &[])
        .with_operation(lash_core::OperationId::turn(
            &session_id,
            "first-racer",
            "final",
        ))
        .expect("build first racing commit");
    let (second_commit, _) = lash_core::RuntimeCommit::persisted_state_for_test(&second_state, &[])
        .with_operation(lash_core::OperationId::turn(
            &session_id,
            "second-racer",
            "final",
        ))
        .expect("build second racing commit");
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let first_start = Arc::clone(&start);
    let second_start = Arc::clone(&start);
    let (first, second) = tokio::join!(
        async move {
            first_start.wait().await;
            first_store.commit_runtime_state(first_commit).await
        },
        async move {
            second_start.wait().await;
            second_store.commit_runtime_state(second_commit).await
        }
    );
    let results = [first, second];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(StoreError::HeadRevisionConflict {
                    expected: 0,
                    actual: 1
                })
            ))
            .count(),
        1,
        "the losing first commit must fail with the typed CAS conflict: {results:?}"
    );
}

#[tokio::test]
async fn postgres_graph_generation_uniqueness_is_typed() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping graph-generation error proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect graph-generation error storage");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let session_id = format!("postgres-generation-collision:{nonce}");
    let first_node = format!("generation-node-a:{nonce}");
    let second_node = format!("generation-node-b:{nonce}");
    sqlx::query(
        "INSERT INTO lash_graph_nodes
         (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
         VALUES ($1, $2, NULL, 3, $2, '{}')",
    )
    .bind(&session_id)
    .bind(&first_node)
    .execute(storage.pool())
    .await
    .expect("seed graph-generation uniqueness fixture");
    let raw = sqlx::query(
        "INSERT INTO lash_graph_nodes
         (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
         VALUES ($1, $2, NULL, 3, $2, '{}')",
    )
    .bind(&session_id)
    .bind(&second_node)
    .execute(storage.pool())
    .await
    .expect_err("duplicate generation must violate Postgres uniqueness");
    let error = graph_node_insert_error(raw, &session_id, 3, &second_node);
    assert!(matches!(
        error,
        StoreError::GraphGenerationCollision {
            session_id: ref actual_session_id,
            generation: 3
        } if actual_session_id == &session_id
    ));
    sqlx::query("DELETE FROM lash_graph_nodes WHERE session_id = $1")
        .bind(&session_id)
        .execute(storage.pool())
        .await
        .expect("clean graph-generation uniqueness fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_claim_completion_is_locked_and_zero_rows_roll_back_the_head() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres claim-completion fence: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect claim-completion fence storage");
    let session_id = format!("postgres-claim-fence:{}", uuid::Uuid::new_v4());
    let input_id = format!("input:{}", uuid::Uuid::new_v4());
    let stale = lash_core::TurnInputCompletion {
        session_id: session_id.clone(),
        claim: Some(lash_core::TurnInputSettlementClaim {
            claim_id: "claim-a".to_string(),
            lease_token: "token-a".to_string(),
        }),
        data: lash_core::TurnInputCompletionData {
            input_ids: vec![input_id.clone()],
            applications: Vec::new(),
        },
    };
    sqlx::query(
        "INSERT INTO lash_sessions (session_id, head_revision, head_json)
         VALUES ($1, 7, '{}')",
    )
    .bind(&session_id)
    .execute(storage.pool())
    .await
    .expect("insert claim-fence session head");
    sqlx::query(
        "INSERT INTO lash_pending_turn_inputs (
            input_id, session_id, ingress_json, state, input_json, enqueued_at_ms,
            claim_id, claim_token, claim_fencing_token, claim_session_lease_generation
         )
         VALUES ($1, $2, '{}', $3, '{}', 1, $4, $5, 1, 1)",
    )
    .bind(&input_id)
    .bind(&session_id)
    .bind(lash_core::TurnInputState::DeferredNextTurn.as_str())
    .bind(stale.claim_id())
    .bind(stale.lease_token())
    .execute(storage.pool())
    .await
    .expect("insert claimed turn input");

    // Ownership validation locks the exact claim row. A superseder cannot
    // rewrite it between validation and completion.
    let mut validating = storage.pool().begin().await.expect("begin validating tx");
    ensure_turn_input_completion_tx(&mut validating, &stale)
        .await
        .expect("validate and lock stale claim");
    let mut blocked_superseder = storage.pool().begin().await.expect("begin superseder tx");
    sqlx::query("SET LOCAL lock_timeout = '50ms'")
        .execute(&mut *blocked_superseder)
        .await
        .expect("bound superseder lock wait");
    let blocked = sqlx::query(
        "UPDATE lash_pending_turn_inputs
         SET claim_id = 'claim-b', claim_token = 'token-b',
             claim_fencing_token = 2, claim_session_lease_generation = 2
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(&session_id)
    .bind(&input_id)
    .execute(&mut *blocked_superseder)
    .await
    .expect_err("ownership row lock must block supersession");
    assert_eq!(
        blocked.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("55P03")),
        "supersession must fail specifically on the held row lock: {blocked}"
    );
    validating
        .rollback()
        .await
        .expect("release validation lock");
    blocked_superseder
        .rollback()
        .await
        .expect("roll back timed-out superseder");

    // Reproduce the old TOCTOU transaction shape: A observed ownership
    // without locking, B committed a fresh generation+token, then A moved
    // the head before attempting token-qualified completion. The checked
    // zero-row completion must abort A's whole transaction.
    let mut stale_committer = storage.pool().begin().await.expect("begin stale tx");
    let observed: Option<i64> = sqlx::query_scalar(
        "SELECT 1::BIGINT FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND input_id = $2 AND claim_id = $3 AND claim_token = $4",
    )
    .bind(&session_id)
    .bind(&input_id)
    .bind(stale.claim_id())
    .bind(stale.lease_token())
    .fetch_optional(&mut *stale_committer)
    .await
    .expect("old non-locking ownership validation");
    assert_eq!(observed, Some(1));

    let mut superseder = storage
        .pool()
        .begin()
        .await
        .expect("begin fresh superseder");
    sqlx::query(
        "UPDATE lash_pending_turn_inputs
         SET claim_id = 'claim-b', claim_token = 'token-b',
             claim_fencing_token = 2, claim_session_lease_generation = 2
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(&session_id)
    .bind(&input_id)
    .execute(&mut *superseder)
    .await
    .expect("supersede stale claim");
    superseder
        .commit()
        .await
        .expect("commit fresh supersession");

    sqlx::query("UPDATE lash_sessions SET head_revision = head_revision + 1 WHERE session_id = $1")
        .bind(&session_id)
        .execute(&mut *stale_committer)
        .await
        .expect("tentatively move stale head");
    let error = complete_turn_input_claims_tx(&mut stale_committer, std::slice::from_ref(&stale))
        .await
        .expect_err("zero-row stale completion must trip the atomic fence");
    assert!(matches!(
        error,
        StoreError::TurnInputClaimSuperseded {
            ref session_id,
            ref claim_id,
            ..
        } if session_id == &stale.session_id && claim_id.as_str() == stale.claim_id().unwrap_or_default()
    ));
    stale_committer
        .rollback()
        .await
        .expect("roll back stale head movement");

    let head_revision: i64 =
        sqlx::query_scalar("SELECT head_revision FROM lash_sessions WHERE session_id = $1")
            .bind(&session_id)
            .fetch_one(storage.pool())
            .await
            .expect("read head after rejected stale commit");
    assert_eq!(
        head_revision, 7,
        "rejected stale completion must not move the session head"
    );
    let current_claim: (String, String, i64) = sqlx::query_as(
        "SELECT claim_id, claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs WHERE session_id = $1 AND input_id = $2",
    )
    .bind(&session_id)
    .bind(&input_id)
    .fetch_one(storage.pool())
    .await
    .expect("read winning claim");
    assert_eq!(
        current_claim,
        ("claim-b".to_string(), "token-b".to_string(), 2)
    );

    sqlx::query("DELETE FROM lash_pending_turn_inputs WHERE session_id = $1")
        .bind(&session_id)
        .execute(storage.pool())
        .await
        .expect("clean claim-fence input");
    sqlx::query("DELETE FROM lash_sessions WHERE session_id = $1")
        .bind(&session_id)
        .execute(storage.pool())
        .await
        .expect("clean claim-fence head");
}

#[tokio::test]
async fn postgres_delete_permanently_fences_stale_handles_and_session_id_reuse() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres delete fence proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect delete fence storage");
    let factory = storage.session_store_factory_with_shared_process_registry();
    let session_id = format!("postgres-delete-fence:{}", uuid::Uuid::new_v4());
    let request = SessionStoreCreateRequest {
        session_id: session_id.clone(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let stale_store = factory
        .create_store(&request)
        .await
        .expect("create stale store");
    let mut state = lash_core::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    state.ensure_agent_frame_initialized();

    factory
        .delete_session(&session_id)
        .await
        .expect("delete before first commit");
    let error = stale_store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect_err("stale first commit must not resurrect the session");
    assert!(matches!(
        error,
        StoreError::SessionDeleted {
            ref session_id
        } if session_id == &request.session_id
    ));

    let reuse_error = match factory.create_store(&request).await {
        Ok(_) => panic!("deleted session id must never be reused"),
        Err(error) => error,
    };
    assert!(matches!(
        reuse_error,
        StoreError::SessionDeleted {
            ref session_id
        } if session_id == &request.session_id
    ));
}

#[tokio::test]
async fn checkpoint_probe_skips_writes_for_deferred_head_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres checkpoint counter: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect checkpoint counter storage");
    let session_id = format!("postgres-checkpoint-counter:{}", std::process::id());
    let store = Arc::new(storage.session_store(&session_id));
    lash_core::testing::conformance::checkpoint_claim_probe_transaction_counts(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &session_id,
        || store.checkpoint_claim_counts(),
    )
    .await;
    storage
        .session_store_factory()
        .delete_session(&session_id)
        .await
        .expect("delete checkpoint counter session");
}

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
/// Multi-threaded on purpose: the manifest surface is synchronous, so the
/// writer blocks a thread on a detached runtime while the pool's IO driver
/// and the sweeper half keep running on others.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arming_a_delete_and_a_concurrent_writer_never_both_win() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres attachment fence race proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect attachment fence database");
    let session_id = format!("postgres-attachment-fence-race:{}", std::process::id());
    let store = std::sync::Arc::new(storage.session_store(&session_id));
    let factory = storage.session_store_factory();
    let attachment_id =
        lash_core::AttachmentId::parse(format!("fence-race-{}", std::process::id()))
            .expect("valid attachment id");
    sqlx::query("DELETE FROM lash_attachment_condemnations WHERE attachment_id = $1")
        .bind(attachment_id.as_str())
        .execute(storage.pool())
        .await
        .expect("clear condemnation fixture");
    let intent = {
        let session_id = session_id.clone();
        let attachment_id = attachment_id.clone();
        move || lash_core::AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: session_id.clone(),
            canonical_uri: format!("lash-attachment://sha256/{attachment_id}"),
            intent_at_epoch_ms: 1,
            owner_kind: None,
            owner_id: None,
        }
    };

    // Widen the writer's read-then-revoke window so the interleaving an
    // unfenced `arm` corrupts is reached on every odd round instead of once
    // in a blue moon. The fence does not care how wide the window is: a
    // concurrent `arm` waits on the per-digest advisory key either way.
    crate::attachments::FENCE_WRITER_WINDOW_DELAY_MS
        .store(20, std::sync::atomic::Ordering::Relaxed);

    // Both orderings, every round: the fixed code holds for all of them.
    for round in 0..12 {
        assert_eq!(
            lash_core::AttachmentRootSet::condemn_attachment(&factory, &attachment_id, 0)
                .await
                .expect("condemn"),
            lash_core::AttachmentCondemnation::Condemned,
            "round {round}: the digest must start each round rootless and free"
        );

        let writer = tokio::task::spawn_blocking({
            let store = std::sync::Arc::clone(&store);
            let intent = intent.clone();
            move || lash_core::AttachmentManifest::begin_attachment_write(&*store, intent())
        });
        if round % 2 == 1 {
            // Alternate the stagger so both orderings are exercised: on odd
            // rounds the writer reaches its condemnation read first (the
            // interleaving an unfenced `arm` corrupts), on even rounds the
            // sweeper arms first. The pacing widens a window; it decides no
            // outcome, and the invariant below holds for either ordering.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let armed = lash_core::AttachmentRootSet::arm_attachment_delete(&factory, &attachment_id)
            .await
            .expect("arm");
        let fence = writer.await.expect("join writer").expect("fenced write");

        let holds_ref =
            lash_core::AttachmentManifest::holds_ref(&*store, &session_id, &attachment_id)
                .expect("holds_ref");
        match (armed, fence) {
            // The sweeper won: the delete is armed and the writer parked
            // without recording anything, so no bytes can land inside it.
            (
                lash_core::AttachmentDeleteArming::Armed,
                lash_core::AttachmentWriteFence::ReclamationInFlight,
            ) => {
                assert!(
                    !holds_ref,
                    "round {round}: a parked writer records no intent"
                );
            }
            // The writer won: it took the digest back before the delete was
            // armed, and the sweeper issues no delete at all.
            (
                lash_core::AttachmentDeleteArming::Revoked,
                lash_core::AttachmentWriteFence::Granted,
            ) => {
                assert!(
                    holds_ref,
                    "round {round}: a granted writer records its intent"
                );
            }
            (armed, fence) => panic!(
                "round {round}: arming and the writer must never both win \
                 (arm = {armed:?}, writer = {fence:?}); bytes would land inside an \
                 in-flight delete"
            ),
        }

        lash_core::AttachmentRootSet::release_attachment_condemnation(&factory, &attachment_id)
            .await
            .expect("release");
        if holds_ref {
            lash_core::AttachmentManifest::forget(&*store, &session_id, &attachment_id)
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
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect empty attachment-root database");
    sqlx::query("DELETE FROM lash_attachment_manifest")
        .execute(storage.pool())
        .await
        .expect("make the configured Postgres manifest empty");
    let wrong_factory = storage.session_store_factory();

    let live_factory = lash_core::runtime::InMemorySessionStoreFactory::new();
    let request = SessionStoreCreateRequest {
        session_id: "postgres-wrong-database-live-attachment".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let live_store = live_factory
        .create_store(&request)
        .await
        .expect("create live root authority");
    let backend = lash_core::attachments::InMemoryAttachmentStore::new();
    let attachment = lash_core::AttachmentStore::put(
        &backend,
        b"postgres-live-committed-blob".to_vec(),
        lash_sansio::AttachmentCreateMeta::new(
            lash_sansio::MediaType::parse("application/octet-stream").expect("media type"),
            None,
            Some("live".to_string()),
        ),
    )
    .await
    .expect("put shared backend blob");
    lash_core::AttachmentManifest::record_intent(
        &*live_store,
        lash_core::AttachmentIntent {
            attachment_id: attachment.id.clone(),
            session_id: request.session_id.clone(),
            canonical_uri: format!("lash-attachment://sha256/{}", attachment.id),
            intent_at_epoch_ms: 1,
            owner_kind: None,
            owner_id: None,
        },
    )
    .expect("record live attachment intent");
    lash_core::AttachmentManifest::commit_refs(
        &*live_store,
        &request.session_id,
        std::slice::from_ref(&attachment.id),
    )
    .expect("commit live attachment ref");

    let result = lash_core::attachments::reclaim_unreferenced_attachments(
        &wrong_factory,
        &backend,
        lash_core::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core::EmptyRootSetPolicy::Refuse,
        },
    )
    .await;

    let failure = result.expect_err("an empty Postgres root database must refuse deletion");
    assert_eq!(
        failure.refusal(),
        Some(&lash_core::MaintenanceRefusal::EmptyRootSetUnauthorized),
        "an empty Postgres root database must refuse deletion: {failure:?}"
    );
    assert_eq!(
        failure.partial.scanned_blob_count, 1,
        "the refusal must carry the report accumulated before it: {failure:?}"
    );
    lash_core::AttachmentStore::get(&backend, &attachment.id)
        .await
        .expect("live committed blob survives the refused sweep");
}
