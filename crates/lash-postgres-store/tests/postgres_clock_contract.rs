//! Live behavioral checks for the PostgreSQL/server-clock boundary.

use std::sync::Arc;

use lash_core::runtime::{QueuedWorkBatchDraft, QueuedWorkClaimBoundary, QueuedWorkPayload};
use lash_core::testing::TestClock;
use lash_core::{
    CheckpointKind, Clock, DeliveryPolicy, LeaseOwnerIdentity, PendingTurnInputCancelOutcome,
    PendingTurnInputCancelTarget, PendingTurnInputDraft, PendingTurnInputSuffixCancelOutcome,
    ProcessAwaitOutput, ProcessCompletionOutcome, ProcessInput, ProcessLeaseClaimOutcome,
    ProcessProvenance, ProcessRegistration, ProcessRegistry, RecoveryDisposition, RuntimeCommit,
    RuntimeSessionState, SessionRelation, SessionStoreCreateRequest, SessionStoreFactory,
    SlotPolicy, TurnInput, TurnInputCheckpointBoundary, TurnInputIngress,
    facade_support::SessionCommand,
};
use lash_postgres_store::PostgresStorage;
use sqlx::Connection as _;

mod support;

use support::{SharedDatabaseLock, database_url};

const CLOCK_SKEW_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1_000;
const RUNTIME_PERSISTENCE_SOURCE: &str = include_str!("../src/postgres/runtime_persistence.rs");
const PROCESS_REGISTRY_SOURCE: &str = include_str!("../src/postgres/process_registry.rs");
const EFFECT_REPLAY_SOURCE: &str = include_str!("../src/postgres/effect_replay.rs");

fn unique_id(prefix: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    format!("{prefix}-{}-{nonce}", std::process::id())
}

async fn db_now_ms(storage: &PostgresStorage) -> u64 {
    let now: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read PostgreSQL transaction clock");
    now.max(0) as u64
}

async fn configured_storage(test_name: &str) -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let Some(url) = database_url() else {
        eprintln!("skipping {test_name}: LASH_POSTGRES_DATABASE_URL is not set");
        return None;
    };
    let lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect PostgreSQL clock-contract storage");
    Some((lock, storage))
}

fn source_region<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start_index = source
        .find(start)
        .unwrap_or_else(|| panic!("missing source marker `{start}`"));
    let region = &source[start_index..];
    let end_index = region
        .find(end)
        .unwrap_or_else(|| panic!("missing source marker `{end}` after `{start}`"));
    &region[..end_index]
}

#[test]
fn lint_postgres_clock_contract_paths_never_use_client_wall_clock() {
    // This is deliberately a lexical fence, not a behavioral test: ADR-0044
    // recognizes that an in-process test cannot skew `SystemTime::now()`.
    let lease_sensitive_regions = [
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn claim_leading_ready_session_command(",
            "async fn claim_ready_queued_work(",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn claim_ready_queued_work(",
            "async fn abandon_queued_work_claim(",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn cancel_queued_work_batch(",
            "async fn list_queued_work(",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn list_pending_queued_work(",
            "impl TurnInputStore for PostgresSessionStore",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn list_pending_turn_inputs(",
            "async fn cancel_pending_turn_inputs(",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn cancel_pending_turn_inputs(",
            "async fn cancel_pending_turn_input_suffix(",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn cancel_pending_turn_input_suffix(",
            "async fn claim_active_turn_inputs(",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn claim_pending_turn_inputs_postgres(",
            "struct SessionExecutionLeaseRow",
        ),
        (
            PROCESS_REGISTRY_SOURCE,
            "async fn complete_process_with_lease(",
            "async fn record_first_started_with_authority(",
        ),
        (
            PROCESS_REGISTRY_SOURCE,
            "async fn claim_process_lease(",
            "async fn reclaim_process_lease(",
        ),
        (
            PROCESS_REGISTRY_SOURCE,
            "async fn reclaim_process_lease(",
            "async fn renew_process_lease(",
        ),
        (
            PROCESS_REGISTRY_SOURCE,
            "async fn renew_process_lease(",
            "async fn get_process_lease(",
        ),
        (
            RUNTIME_PERSISTENCE_SOURCE,
            "async fn commit_runtime_state(",
            "async fn save_session_meta(",
        ),
        // Effect-replay leases fence exactly-once execution across hosts, so
        // the persistence adapter's claim/finalize/renew atoms read the server
        // clock like every other lease path. The shared `EffectReplayDriver`
        // carries a wall clock for sleeps only; if it ever reached these atoms,
        // this fence is what fails.
        (
            EFFECT_REPLAY_SOURCE,
            "async fn claim(",
            "async fn finalize(",
        ),
        (
            EFFECT_REPLAY_SOURCE,
            "async fn finalize(",
            "async fn renew(",
        ),
        (
            EFFECT_REPLAY_SOURCE,
            "async fn renew(",
            "async fn retire_journal(",
        ),
        (
            EFFECT_REPLAY_SOURCE,
            "async fn claim_in_transaction(",
            "async fn select_effect_row_for_update(",
        ),
        // The two atoms that actually persist lease stamps. They bind values the
        // covered `claim` region minted, so covering only the readers would let a
        // client-clock read slip into the write side.
        (
            EFFECT_REPLAY_SOURCE,
            "async fn insert_claimed_row(",
            "async fn take_over_expired_lease(",
        ),
        (
            EFFECT_REPLAY_SOURCE,
            "async fn take_over_expired_lease(",
            "fn effect_store_error(",
        ),
    ];

    // Every way this crate can read a host wall clock. `current_epoch_ms()` is
    // the crate's own helper; the other two are the ways around it, and one of
    // them (`SystemClock`) is now constructed in `effect_replay.rs` to give the
    // shared driver its sleep clock, so it is one identifier away from these
    // regions rather than one module away.
    const CLIENT_CLOCK_READS: [&str; 3] =
        ["current_epoch_ms()", "SystemTime::now()", "SystemClock"];

    for (source, start, end) in lease_sensitive_regions {
        let region = source_region(source, start, end);
        for read in CLIENT_CLOCK_READS {
            assert!(
                !region.contains(read),
                "lexical clock fence: `{start}` must not use the client wall clock (`{read}`)"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_work_and_pending_input_lease_decisions_follow_the_postgres_clock() {
    let Some((_lock, storage)) =
        configured_storage("queued-work/pending-input PostgreSQL clock contract").await
    else {
        return;
    };
    let session_id = unique_id("clock-contract-session");
    let server_before = db_now_ms(&storage).await;
    let clock = Arc::new(TestClock::new(server_before.saturating_add(CLOCK_SKEW_MS)));
    let factory = storage
        .session_store_factory()
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            policy: lash_core::SessionPolicy::default(),
        })
        .await
        .expect("create skewed-clock session store");
    let owner = LeaseOwnerIdentity::opaque("clock-contract-owner", "clock-contract-owner:i");
    let lease = store
        .try_claim_session_execution_lease(&session_id, &owner, 60_000)
        .await
        .expect("claim session lease")
        .acquired()
        .expect("session lease acquired");
    let server_after = db_now_ms(&storage).await;
    assert!(
        (server_before..=server_after).contains(&lease.claimed_at_epoch_ms),
        "session lease claim timestamp must come from PostgreSQL: server={server_before}..={server_after}, lease={lease:?}"
    );

    let command = store
        .enqueue_queued_work(QueuedWorkBatchDraft::new(
            &session_id,
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
            vec![QueuedWorkPayload::session_command(
                SessionCommand::RefreshToolCatalog {
                    reason: "clock-contract command".to_string(),
                },
            )],
        ))
        .await
        .expect("enqueue session command under skewed client clock");
    let batch = store
        .enqueue_queued_work(QueuedWorkBatchDraft::new(
            &session_id,
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
            vec![QueuedWorkPayload::agent_frame_task(
                "clock-contract-frame",
                "clock-contract queued work",
                None,
            )],
        ))
        .await
        .expect("enqueue queued work under skewed client clock");
    let active_input = store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            &session_id,
            TurnInputIngress::active_turn(
                "clock-contract-turn",
                TurnInputCheckpointBoundary::AfterWork,
            ),
            TurnInput::text("clock-contract active input"),
        ))
        .await
        .expect("enqueue active input under skewed client clock");
    let next_input = store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            &session_id,
            TurnInputIngress::NextTurn,
            TurnInput::text("clock-contract pending input"),
        ))
        .await
        .expect("enqueue pending input under skewed client clock");
    let command_claim = store
        .claim_leading_ready_session_command(&session_id, &lease.fence(), &owner)
        .await
        .expect("command claim must validate against PostgreSQL time")
        .expect("session command remains claimable despite future-skewed client clock");
    assert_eq!(command_claim.batches[0].batch_id, command.batch_id);
    assert_eq!(
        store
            .list_pending_queued_work(&session_id)
            .await
            .expect("list pending queue against PostgreSQL time")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![batch.batch_id.as_str()],
        "a live server-clock claim must stay hidden from the pending queue"
    );
    assert!(
        store
            .cancel_queued_work_batch(&session_id, &command.batch_id)
            .await
            .expect("cancel claimed command against PostgreSQL time")
            .is_none(),
        "a future-skewed client clock must not make a live claim cancellable"
    );
    store
        .abandon_queued_work_claim(&command_claim)
        .await
        .expect("release command claim for the turn-work probe");
    assert_eq!(
        store
            .cancel_queued_work_batch(&session_id, &command.batch_id)
            .await
            .expect("cancel released command")
            .expect("released command is cancellable")
            .batch_id,
        command.batch_id
    );
    let queue_claim = store
        .claim_ready_queued_work(
            &session_id,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("queue claim must validate against PostgreSQL time")
        .expect("queued work remains claimable despite future-skewed client clock");
    assert_eq!(queue_claim.batches[0].batch_id, batch.batch_id);

    let active_claim = store
        .claim_active_turn_inputs(
            &session_id,
            &lease.fence(),
            &owner,
            &lash_core::TurnId::from("clock-contract-turn"),
            CheckpointKind::AfterWork,
            1,
        )
        .await
        .expect("active-input claim must validate against PostgreSQL time")
        .expect("active input remains claimable despite future-skewed client clock");
    assert_eq!(active_claim.inputs[0].input_id, active_input.input_id);
    assert_eq!(
        store
            .list_pending_turn_inputs(&session_id)
            .await
            .expect("list pending inputs against PostgreSQL time")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![next_input.input_id.as_str()],
        "a live server-clock input claim must stay hidden from pending inputs"
    );
    let cancel = store
        .cancel_pending_turn_inputs(
            &session_id,
            &[PendingTurnInputCancelTarget::input_id(
                &active_input.input_id,
            )],
        )
        .await
        .expect("cancel claimed input against PostgreSQL time");
    assert!(matches!(
        &cancel[0].outcome,
        PendingTurnInputCancelOutcome::AlreadyClaimed { input, .. }
            if input.input_id == active_input.input_id
    ));
    let suffix = store
        .cancel_pending_turn_input_suffix(
            &session_id,
            &PendingTurnInputCancelTarget::input_id(&active_input.input_id),
        )
        .await
        .expect("cancel input suffix against PostgreSQL time");
    let PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix else {
        panic!("expected suffix cancellation outcomes, got {suffix:?}");
    };
    assert!(matches!(
        &outcomes[0],
        PendingTurnInputCancelOutcome::AlreadyClaimed { input, .. }
            if input.input_id == active_input.input_id
    ));
    assert!(matches!(
        &outcomes[1],
        PendingTurnInputCancelOutcome::Cancelled(input) if input.input_id == next_input.input_id
    ));
    let final_next_input = store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            &session_id,
            TurnInputIngress::NextTurn,
            TurnInput::text("clock-contract final pending input"),
        ))
        .await
        .expect("enqueue final pending input under skewed client clock");
    let input_claim = store
        .claim_next_turn_inputs(&session_id, &lease.fence(), &owner, 1)
        .await
        .expect("input claim must validate against PostgreSQL time")
        .expect("pending input remains claimable despite future-skewed client clock");
    assert_eq!(input_claim.inputs[0].input_id, final_next_input.input_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_lease_decisions_follow_the_postgres_clock() {
    let Some((_lock, storage)) =
        configured_storage("process-lease PostgreSQL clock contract").await
    else {
        return;
    };
    let process_id = unique_id("clock-contract-process");
    let server_now = db_now_ms(&storage).await;
    let clock = Arc::new(TestClock::new(server_now));
    let registry = storage
        .process_registry()
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    registry
        .register_process(ProcessRegistration::new(
            &process_id,
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register process for clock contract");
    let owner_a = LeaseOwnerIdentity::opaque("clock-process-a", "clock-process-a:i");
    let lease = registry
        .claim_process_lease(&process_id, &owner_a, 60_000)
        .await
        .expect("claim process lease")
        .acquired()
        .expect("process lease acquired");

    clock.advance(CLOCK_SKEW_MS);
    let owner_b = LeaseOwnerIdentity::opaque("clock-process-b", "clock-process-b:i");
    let renewed = registry
        .renew_process_lease(&lease, 60_000)
        .await
        .expect("future-skewed client clock must not invalidate process lease renewal");
    assert_eq!(renewed.lease_token, lease.lease_token);
    assert_eq!(renewed.fencing_token, lease.fencing_token);
    assert!(
        renewed.expires_at_epoch_ms >= lease.expires_at_epoch_ms,
        "renewal must not shorten the live process lease"
    );
    assert!(matches!(
        registry
            .reclaim_process_lease(&process_id, &owner_b, &renewed, 60_000)
            .await
            .expect("competing process lease reclaim decision"),
        ProcessLeaseClaimOutcome::Busy { holder }
            if holder.lease_token == renewed.lease_token
                && holder.fencing_token == renewed.fencing_token
    ));
    assert!(matches!(
        registry
            .claim_process_lease(&process_id, &owner_b, 60_000)
            .await
            .expect("competing process lease decision"),
        ProcessLeaseClaimOutcome::Busy { holder }
            if holder.lease_token == lease.lease_token
                && holder.fencing_token == lease.fencing_token
    ));
    let completion = registry
        .complete_process_with_lease(
            &renewed,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"clock": "postgres"}),
                control: None,
            },
        )
        .await
        .expect("future-skewed client clock must not invalidate a live process lease");
    assert!(matches!(completion, ProcessCompletionOutcome::Committed(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_turn_commit_stamps_follow_the_injected_store_clock() {
    let Some((_lock, storage)) = configured_storage("final-turn injected-clock contract").await
    else {
        return;
    };
    const INJECTED_COMMIT_MS: u64 = 1_234_567_890_000;
    let session_id = unique_id("clock-contract-final-commit");
    let clock = Arc::new(TestClock::new(INJECTED_COMMIT_MS));
    let factory = storage
        .session_store_factory()
        .with_clock(clock as Arc<dyn Clock>);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            policy: lash_core::SessionPolicy::default(),
        })
        .await
        .expect("create final-commit session store");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit runtime state with injected clock");
    let committed_at_ms: i64 = sqlx::query_scalar(
        "SELECT committed_at_ms FROM lash_runtime_turn_commits WHERE session_id = $1",
    )
    .bind(&session_id)
    .fetch_one(storage.pool())
    .await
    .expect("read persisted final-turn commit timestamp");
    assert_eq!(committed_at_ms, INJECTED_COMMIT_MS as i64);
}

/// The diagnostic lease read must not take the lease row's lock.
///
/// The mutation paths take `FOR UPDATE` deliberately, because check-then-act on
/// this row is not atomic under READ COMMITTED. If the *diagnostic* read joined
/// them, an operator polling a stuck session would queue behind (and make wait)
/// the very holder or claimant they are trying to observe: watching the lease
/// would delay the lane. This proves both halves: the query plans no `LockRows`,
/// and the read completes promptly while another connection holds the row locked.
#[tokio::test]
async fn diagnostic_lease_read_neither_locks_the_row_nor_waits_for_a_holder() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping PostgreSQL diagnostic-read lock contract: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect PostgreSQL storage");
    let session_id = unique_id("diagnostic-read-lock");
    let factory = storage.session_store_factory_with_shared_process_registry();
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            policy: Default::default(),
        })
        .await
        .expect("create the session store");
    let owner = LeaseOwnerIdentity::opaque("diagnostic-read-holder", "boot-1");
    let held = store
        .try_claim_session_execution_lease(&session_id, &owner, 60_000)
        .await
        .expect("claim the lane")
        .acquired()
        .expect("an unheld lane is acquirable");

    // 1. The plan carries no row-locking node.
    let plan_rows: Vec<String> = sqlx::query_scalar(
        "EXPLAIN (FORMAT TEXT)
         SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id, lease_owner_liveness_json
         FROM lash_session_execution_leases
         WHERE session_id = $1",
    )
    .bind(&session_id)
    .fetch_all(storage.pool())
    .await
    .expect("explain the diagnostic read");
    let plan = plan_rows.join("\n");
    assert!(
        !plan.contains("LockRows"),
        "the diagnostic read must not plan a row lock:\n{plan}"
    );

    // 2. It also does not wait on one. Hold the row under `FOR UPDATE` in an open
    //    transaction on a separate connection, then read diagnostics.
    let mut locker = sqlx::PgConnection::connect(&database_url)
        .await
        .expect("connect the row-lock holder");
    sqlx::query("BEGIN")
        .execute(&mut locker)
        .await
        .expect("begin the locking transaction");
    let locked: Option<String> = sqlx::query_scalar(
        "SELECT lease_owner_id FROM lash_session_execution_leases
         WHERE session_id = $1 FOR UPDATE",
    )
    .bind(&session_id)
    .fetch_optional(&mut locker)
    .await
    .expect("take the row lock");
    assert_eq!(
        locked.as_deref(),
        Some("diagnostic-read-holder"),
        "the locking transaction must actually hold this session's lease row"
    );

    let observed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.get_session_execution_lease(&session_id),
    )
    .await
    .expect("the diagnostic read must not wait for the row lock")
    .expect("diagnostic read succeeds")
    .expect("a held lane is reported");
    assert_eq!(observed.fencing_token, held.fencing_token);
    assert_eq!(observed.owner, held.owner);

    // For contrast: the same read through a locking path would block here. Prove
    // the lock really was contended by showing a `FOR UPDATE NOWAIT` fails.
    let contended = sqlx::query(
        "SELECT 1 FROM lash_session_execution_leases
         WHERE session_id = $1 FOR UPDATE NOWAIT",
    )
    .bind(&session_id)
    .fetch_optional(storage.pool())
    .await;
    assert!(
        contended.is_err(),
        "the row lock must still be held, or this test proved nothing"
    );

    sqlx::query("ROLLBACK")
        .execute(&mut locker)
        .await
        .expect("release the row lock");
}
