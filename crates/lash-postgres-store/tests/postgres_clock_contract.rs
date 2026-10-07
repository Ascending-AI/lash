//! Behavioral laws and lexical fences for PostgreSQL clock provenance.

use lash_sansio::{ProcessId, SessionId};
use std::sync::Arc;

use lash_core_execution::runtime::QueuedWorkBatchDraft;
use lash_core_execution::store::{CheckpointAdmissionRequest, RunStore};
use lash_core_execution::testing::TestClock;
use lash_core_execution::{
    CheckpointKind, Clock, DeliveryPolicy, PendingTurnInputCancelOutcome,
    PendingTurnInputCancelTarget, PendingTurnInputDraft, PendingTurnInputReadStatus,
    QueuedWorkStore, RuntimeCommit, RuntimeSessionState, SessionCatalogStore as _,
    SessionCommitStore, SessionCreationHead, SessionRelation, SessionStoreCreateRequest, TurnId,
    TurnInput, TurnInputIngress, TurnInputStore, facade_support::SessionCommand,
};
use lash_postgres_store::PostgresStorage;

// Keep subsequent lines stable for machine-checked public API evidence anchors.
// Shared test support now lives at the grouped integration-harness run.
use crate::support::{SharedDatabaseLock, database_url};

const CLOCK_SKEW_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1_000;
const RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE: &str = concat!(
    include_str!("../src/postgres/runtime_persistence/queued_work.rs"),
    "\nimpl IngressStore for PostgresStore"
);
const RUNTIME_PERSISTENCE_ADMISSION_SOURCE: &str = concat!(
    include_str!("../src/postgres/runtime_persistence/admission.rs"),
    "\n// end of admission.rs"
);
const RUNTIME_PERSISTENCE_INGRESS_SETTLEMENT_SOURCE: &str = concat!(
    include_str!("../src/postgres/runtime_persistence/ingress_settlement.rs"),
    "\n// end of ingress_settlement.rs"
);
const RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE: &str =
    include_str!("../src/postgres/runtime_persistence/turn_input.rs");
const RUNTIME_PERSISTENCE_SESSION_COMMIT_SOURCE: &str =
    include_str!("../src/postgres/runtime_persistence/session_commit.rs");
const PROCESS_HELPERS_SOURCE: &str = include_str!("../src/postgres/process_helpers.rs");
const PROCESS_REGISTRY_SOURCE: &str = include_str!("../src/postgres/process_registry.rs");
const PROCESS_LIFECYCLE_SOURCE: &str =
    include_str!("../src/postgres/process_registry/lifecycle.rs");

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
    let clock_sensitive_regions = [
        // Every admission path — run, checkpoint and the command run — and
        // the free functions that compose and bind their rows, through the
        // end of the file.
        (
            RUNTIME_PERSISTENCE_ADMISSION_SOURCE,
            "async fn follow_on_blocks_admission_tx(",
            "// end of admission.rs",
        ),
        // A commit's settlement of the rows its run admitted runs inside the
        // commit's transaction, on the same server clock.
        (
            RUNTIME_PERSISTENCE_INGRESS_SETTLEMENT_SOURCE,
            "async fn settle_commit_ingress_tx(",
            "// end of ingress_settlement.rs",
        ),
        (
            RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE,
            "async fn cancel_queued_work_batch_pg(",
            "async fn queued_work_batch_completion_pg(",
        ),
        (
            RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE,
            "async fn pending_session_work_ordering_pg(",
            "async fn list_open_queued_work_pg(",
        ),
        (
            RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE,
            "async fn list_open_queued_work_pg(",
            "impl IngressStore for PostgresStore",
        ),
        (
            RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE,
            "async fn list_pending_turn_inputs(",
            "async fn cancel_pending_turn_inputs(",
        ),
        (
            RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE,
            "async fn cancel_pending_turn_inputs(",
            "async fn cancel_pending_turn_input_suffix(",
        ),
        (
            RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE,
            "async fn cancel_pending_turn_input_suffix(",
            "async fn enqueue_queued_work(",
        ),
        (
            RUNTIME_PERSISTENCE_SESSION_COMMIT_SOURCE,
            "async fn commit_runtime_state(",
            "async fn settle_observer_intents(",
        ),
        // The shared process-event append sequence stamps registry events
        // under the caller's store clock.
        (
            PROCESS_HELPERS_SOURCE,
            "async fn apply_process_event_append_tx(",
            "async fn append_process_event_tx(",
        ),
    ];

    // Every way this crate can read a host wall clock. `current_epoch_ms()` is
    // the crate's own helper; the other two are the ways around it.
    const CLIENT_CLOCK_READS: [&str; 3] =
        ["current_epoch_ms()", "SystemTime::now()", "SystemClock"];

    for (source, start, end) in clock_sensitive_regions {
        let region = source_region(source, start, end);
        for read in CLIENT_CLOCK_READS {
            assert!(
                !region.contains(read),
                "lexical clock fence: `{start}` must not use the client wall clock (`{read}`)"
            );
        }
    }
}

#[test]
fn lint_process_event_timestamps_use_the_injected_clock() {
    // Fence the complete entry-point files and shared append helpers so a
    // second timestamp source cannot hide in a new method or helper (ADR 0044).
    for (name, source) in [
        ("process_registry.rs", PROCESS_REGISTRY_SOURCE),
        ("process_registry/lifecycle.rs", PROCESS_LIFECYCLE_SOURCE),
        ("process_helpers.rs", PROCESS_HELPERS_SOURCE),
    ] {
        for read in [
            "process_registry_now_epoch_ms_tx",
            "select_statement_epoch_ms",
            "select_transaction_epoch_ms",
            "clock_timestamp()",
            "transaction_timestamp()",
            "statement_timestamp()",
            "current_epoch_ms()",
            "SystemTime::now()",
            "SystemClock",
        ] {
            assert!(
                !source.contains(read),
                "process-event clock fence: `{name}` must not read `{read}`"
            );
        }
    }
    for source in [PROCESS_REGISTRY_SOURCE, PROCESS_LIFECYCLE_SOURCE] {
        assert!(
            source.contains("self.clock.timestamp_ms()"),
            "process-event entry points must sample the injected registry clock"
        );
    }
}

fn clock_contract_wake(session_id: &SessionId) -> lash_core_execution::ProcessWakeDelivery {
    let process_id = || ProcessId::fixture("clock-contract-wake-process");
    lash_core_execution::ProcessWakeDelivery {
        version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        target_session_id: session_id.clone(),
        process_id: process_id(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        process_caused_by: None,
        authority: lash_core_execution::QueuedWorkAuthority::default(),
        input: "clock-contract queued work".to_string(),
        created_at_ms: 1,
        trace_cause: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_work_and_pending_input_admission_decisions_follow_the_postgres_clock() {
    let Some((_lock, storage)) =
        configured_storage("queued-work/pending-input PostgreSQL clock contract").await
    else {
        return;
    };
    let session_id = unique_id("clock-contract-session");
    let session = SessionId::fixture(session_id.clone());
    let server_before = db_now_ms(&storage).await;
    let clock = Arc::new(TestClock::new(server_before.saturating_add(CLOCK_SKEW_MS)));
    let factory = storage
        .session_store_factory()
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    factory
        .admit_session(&SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session.clone(),
            relation: SessionRelation::Root,
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
                lash_core_execution::MaxToolCalls::new(1024),
            )
            .into(),
            head: SessionCreationHead::Config,
        })
        .await
        .expect("create skewed-clock session store");
    let store = factory;

    let withdrawable = store
        .enqueue_queued_work(QueuedWorkBatchDraft::new(
            SessionId::fixture(&session_id),
            DeliveryPolicy::EarliestSafeBoundary,
            SessionCommand::RefreshToolCatalog {
                reason: "clock-contract withdrawn command".to_string(),
            },
        ))
        .await
        .expect("enqueue a command before admission under skewed client clock");
    assert_eq!(
        store
            .cancel_queued_work_batch(&session, &withdrawable.batch_id)
            .await
            .expect("withdraw an unread command against PostgreSQL time")
            .expect("an unapplied command is withdrawable")
            .batch_id,
        withdrawable.batch_id
    );
    let command = store
        .enqueue_queued_work(QueuedWorkBatchDraft::new(
            SessionId::fixture(&session_id),
            DeliveryPolicy::EarliestSafeBoundary,
            SessionCommand::RefreshToolCatalog {
                reason: "clock-contract command".to_string(),
            },
        ))
        .await
        .expect("enqueue session command under skewed client clock");
    let batch = store
        .enqueue_queued_work(lash_core_execution::runtime::process_wake_batch_draft(
            clock_contract_wake(&session),
        ))
        .await
        .expect("enqueue queued work under skewed client clock");

    // The command lane is bindless: its read takes no binding (FIG-4202).
    let commands = store
        .open_session_command_run(&session)
        .await
        .expect("the command run must validate against PostgreSQL time");
    assert_eq!(
        commands
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()],
        "the session command is readable despite a future-skewed client clock"
    );
    assert!(
        store
            .queued_work_batch_completion(&session, &command.batch_id)
            .await
            .expect("read the unapplied command's completion")
            .is_none(),
        "a read alone does not apply the command"
    );
    let mut state = RuntimeSessionState {
        session_id: session.clone(),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    let mut command_commit = RuntimeCommit::persisted_state_for_test(&state);
    command_commit.applied_commands = Some(lash_core_execution::runtime::QueuedWorkCompletion {
        session_id: session.clone(),
        batch_ids: vec![command.batch_id.clone()],
    });
    let command_receipt = store
        .commit_runtime_state(command_commit)
        .await
        .expect("apply the read command");
    state.head_revision = command_receipt.head_revision;
    assert_eq!(
        store
            .queued_work_batch_completion(&session, &command.batch_id)
            .await
            .expect("read the applied command's completion")
            .expect("the command commit records its receipt")
            .head_revision,
        command_receipt.head_revision,
        "the command and session head settle in the same commit"
    );

    // A running run's checkpoint takes the queued work and leaves the
    // next-turn input open.
    let run = TurnId::from("clock-contract-run");
    let next_input = store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            SessionId::fixture(&session_id),
            TurnInputIngress::NextTurn,
            TurnInput::text("clock-contract pending input"),
        ))
        .await
        .expect("enqueue pending input under skewed client clock");
    let checkpoint = store
        .admit_at_checkpoint(&CheckpointAdmissionRequest {
            session_id: session.clone(),
            run: run.clone(),
            turn_id: run.clone(),
            checkpoint: CheckpointKind::AfterWork,
            step: "clock-contract-checkpoint".to_string(),
            max_inputs: 1,
            policy: lash_core_execution::testing::queued_work_admission_policy(1),
        })
        .await
        .expect("the checkpoint admission must validate against PostgreSQL time");
    assert_eq!(
        checkpoint
            .queued
            .as_ref()
            .map(lash_core_execution::runtime::AdmittedQueuedWork::batch_ids),
        Some(vec![batch.batch_id.clone()]),
        "queued work is admissible despite a future-skewed client clock"
    );
    assert!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open queue against PostgreSQL time")
            .is_empty(),
        "an admitted batch stays hidden from the open queue"
    );
    assert_eq!(
        store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list pending inputs against PostgreSQL time")
            .iter()
            .map(|read| (read.input.input_id.as_str(), read.status.clone()))
            .collect::<Vec<_>>(),
        vec![(
            next_input.input_id.as_str(),
            PendingTurnInputReadStatus::Open
        )],
        "a checkpoint leaves the next-turn input open"
    );
    let cancel = store
        .cancel_pending_turn_inputs(
            &session,
            &[PendingTurnInputCancelTarget::input_id(&next_input.input_id)],
        )
        .await
        .expect("cancel the open input against PostgreSQL time");
    assert!(matches!(
        &cancel[0].outcome,
        PendingTurnInputCancelOutcome::Cancelled(input) if input.input_id == next_input.input_id
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_turn_commit_stamps_follow_the_injected_store_clock() {
    let Some((_lock, storage)) = configured_storage("final-turn injected-clock contract").await
    else {
        return;
    };
    const INJECTED_COMMIT_MS: u64 = 1_234_567_900_000;
    let session_id = unique_id("clock-contract-final-commit");
    let clock = Arc::new(TestClock::new(INJECTED_COMMIT_MS));
    let factory = storage
        .session_store_factory()
        .with_clock(clock as Arc<dyn Clock>);
    factory
        .admit_session(&SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::fixture(session_id.clone()),
            relation: SessionRelation::Root,
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
                lash_core_execution::MaxToolCalls::new(1024),
            )
            .into(),
            head: SessionCreationHead::Config,
        })
        .await
        .expect("create final-commit session store");
    let store = factory;
    let state = RuntimeSessionState {
        session_id: SessionId::fixture(session_id.clone()),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
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
