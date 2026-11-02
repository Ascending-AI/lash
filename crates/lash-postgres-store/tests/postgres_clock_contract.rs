//! Behavioral laws and lexical fences for PostgreSQL clock provenance.

use lash_kernel_value::identity::SessionId;
use std::sync::Arc;

use lash_core_ids::clock::Clock;
use lash_core_ids::test_clock::TestClock;
use lash_core_store::queued_work_vocabulary::DeliveryPolicy;
use lash_core_store::queued_work_vocabulary::QueuedWorkBatchDraft;
use lash_core_store::queued_work_vocabulary::SessionCommand;
use lash_core_store::session_identity::SessionRelation;
use lash_core_store::session_state::RuntimeSessionState;
use lash_core_store::session_store_factory_types::SessionCreationHead;
use lash_core_store::session_store_factory_types::SessionStoreCreateRequest;
use lash_core_store::store::CheckpointAdmissionRequest;
use lash_core_store::store::QueuedWorkStore;
use lash_core_store::store::RunStore;
use lash_core_store::store::RuntimeCommit;
use lash_core_store::store::SessionCatalogStore as _;
use lash_core_store::store::SessionCommitStore;
use lash_core_store::store::TurnInputStore;
use lash_core_store::turn_input_vocabulary::PendingTurnInputCancelOutcome;
use lash_core_store::turn_input_vocabulary::PendingTurnInputCancelTarget;
use lash_core_store::turn_input_vocabulary::PendingTurnInputDraft;
use lash_core_store::turn_input_vocabulary::PendingTurnInputReadStatus;
use lash_core_store::turn_input_vocabulary::TurnInput;
use lash_core_store::turn_input_vocabulary::TurnInputIngress;
use lash_postgres_store::PostgresStorage;
use lash_protocol_types::plugin::CheckpointKind;
use lash_vocabulary::identity::TurnId;

// Keep subsequent lines stable for machine-checked public API evidence anchors.
// Shared test support now lives at the grouped integration-harness run.
use crate::support::{SharedDatabaseLock, database_url};

const CLOCK_SKEW_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1_000;

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
    let storage = lash_postgres_store::testing::connect(&url)
        .await
        .expect("connect PostgreSQL clock-contract storage");
    Some((lock, storage))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_command_and_pending_input_decisions_follow_the_postgres_clock() {
    let Some((_lock, storage)) =
        configured_storage("session-command/pending-input PostgreSQL clock contract").await
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
            config: lash_core_store::session_graph::PersistedSessionConfig::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
            head: SessionCreationHead::Config,
            retention: lash_core_store::store::Retention::UntilGc,
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
        ..RuntimeSessionState::fixture(lash_core_store::session_policy::SessionPolicy::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    let mut command_commit = RuntimeCommit::persisted_state_for_test(&state);
    command_commit.applied_commands = Some(
        lash_core_store::queued_work_vocabulary::QueuedWorkCompletion {
            session_id: session.clone(),
            batch_ids: vec![command.batch_id.clone()],
        },
    );
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

    // A checkpoint finds nothing to admit and leaves the next-turn input
    // open.
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
    assert!(
        checkpoint.is_empty(),
        "a next-turn input is no checkpoint work"
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
            config: lash_core_store::session_graph::PersistedSessionConfig::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
            head: SessionCreationHead::Config,
            retention: lash_core_store::store::Retention::UntilGc,
        })
        .await
        .expect("create final-commit session store");
    let store = factory;
    let state = RuntimeSessionState {
        session_id: SessionId::fixture(session_id.clone()),
        ..RuntimeSessionState::fixture(lash_core_store::session_policy::SessionPolicy::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
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

/// An artifact cleanup's due instant is on the clock its relay claims with:
/// the store's injected clock, as every SQLite arm and the PostgreSQL
/// ledger's own arm stamp it. A session delete and a guarded artifact
/// publish armed theirs at the database's transaction time, so under a
/// clock behind the database (a test clock, or a host behind its server)
/// the cleanup was not due to the relay that claims it (FIG-5193, found by
/// the cross-backend differential).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_cleanups_are_armed_on_the_injected_store_clock() {
    let Some((_lock, storage)) =
        configured_storage("artifact-cleanup injected-clock contract").await
    else {
        return;
    };
    const INJECTED_MS: u64 = 1_234_567_900_000;
    assert!(
        db_now_ms(&storage).await > INJECTED_MS,
        "the injected clock is behind the database"
    );
    let clock = Arc::new(TestClock::new(INJECTED_MS)) as Arc<dyn Clock>;
    let due_at = |kind: &'static str, id: String| {
        let pool = storage.pool().clone();
        async move {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT obligation_due_at_ms FROM lash_artifact_cleanup_obligations
                 WHERE referrer_kind = $1 AND referrer_id = $2",
            )
            .bind(kind)
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read the armed cleanup's due instant")
        }
    };

    // A session delete arms its session's retirement cleanup.
    let session_id = SessionId::fixture(unique_id("clock-contract-cleanup-delete"));
    let factory = storage
        .session_store_factory()
        .with_clock(Arc::clone(&clock));
    factory
        .admit_session(&SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            config: lash_core_store::session_graph::PersistedSessionConfig::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_protocol_types::session_model::NoProgressBudget::bounded(12),
            ),
            head: SessionCreationHead::Config,
            retention: lash_core_store::store::Retention::UntilGc,
        })
        .await
        .expect("create the session to delete");
    factory
        .delete_session(&session_id)
        .await
        .expect("delete the session");
    assert_eq!(
        due_at("session", session_id.as_str().to_owned()).await,
        Some(INJECTED_MS as i64),
        "the deleted session's cleanup is due at the injected clock's now"
    );

    // A guarded publish arms its reader's cleanup.
    let scope = lash_core_execution::ExecutionScope::turn(
        SessionId::fixture(unique_id("clock-contract-cleanup-publish")),
        "turn",
    );
    let journal = scope.journal_identity().expect("a turn journal identity");
    let referrer = lash_core_store::artifact_referrer::ArtifactReferrer::Execution(journal.clone());
    let claim = lash_core_store::artifact_referrer::ReferrerClaim::guarded(
        lash_core_store::artifact_referrer::ReferrerGuard::Journal(journal),
    );
    let artifacts = storage
        .lash_vm_artifact_store()
        .with_clock(Arc::clone(&clock));
    lash_execution_contracts::module_artifacts::ModuleArtifactStore::publish_module_artifact(
        &artifacts,
        &claim,
        &unique_id("clock-contract-module"),
        b"module bytes",
    )
    .await
    .expect("publish under a guarded claim");
    assert_eq!(
        due_at(referrer.kind().as_str(), referrer.canonical_id()).await,
        Some(INJECTED_MS as i64),
        "the guarded reader's cleanup is due at the injected clock's now"
    );
}
