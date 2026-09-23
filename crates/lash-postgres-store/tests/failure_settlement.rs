//! FIG-3575 on PostgreSQL: a turn failure settles by its cause, exactly as the
//! file-backed SQLite laws in the facade's `failure_settlement` tests pin.
//!
//! * A deterministic before-LLM failure is recorded as a failed turn on a
//!   direct turn, and a queued run settles it after one attempt.
//! * A live journal fault aborts a direct turn with `Err` that carries its
//!   acceptance receipt, so the host withdraws the input by name or redrives
//!   the turn id. Until then the input is bound to the aborted turn and no
//!   later turn folds it in (FIG-3589); a crashed turn's input is still
//!   reclaimed by the next lease generation. On a queued run a live fault
//!   keeps the run pending and the retry completes it.
//! * Cancellation keeps settling `Stopped { Cancelled }`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint;
use lash_core::{LlmOutputPart, TurnInput};
use lash_postgres_store::{PostgresEffectHost, PostgresStorage};
use lash_sansio::sync::MutexExt;

use crate::support::{SharedDatabaseLock, database_url, reset};

const STRANDED_WORDS: &str = "words of the turn that aborted";

#[derive(Default)]
struct RefusingBeforeLlmCall {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for RefusingBeforeLlmCall {
    async fn before_llm_call(
        &self,
        _ctx: lash_core::plugin::ProtocolBeforeLlmCallContext,
        _request: &LlmRequest,
    ) -> Result<Option<lash_core::ProtocolLlmCallAction>, lash_core::PluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::PluginError::Invoke(
            "the protocol refuses this request".to_string(),
        ))
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

fn counting_text_provider(
    calls: Arc<AtomicUsize>,
    requests: Arc<StdMutex<Vec<String>>>,
) -> lash::provider::ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            let requests = Arc::clone(&requests);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                requests
                    .lock_recover()
                    .push(serde_json::to_string(&request.messages).expect("serialize request"));
                Ok(text_response("answered"))
            }
        })
        .build()
        .into_handle()
}

struct PostgresDeployment {
    storage: PostgresStorage,
    effect_host: Arc<PostgresEffectHost>,
    _lock: SharedDatabaseLock,
}

impl PostgresDeployment {
    async fn open() -> Option<Self> {
        let database_url = database_url()?;
        let lock = SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect Postgres");
        reset(storage.pool()).await;
        let effect_host = Arc::new(storage.effect_host());
        Some(Self {
            storage,
            effect_host,
            _lock: lock,
        })
    }

    fn core(
        &self,
        provider: lash::provider::ProviderHandle,
        protocol: Option<Arc<dyn lash_core::plugin::ProtocolSessionPlugin>>,
    ) -> lash::LashCore {
        let builder = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded);
        let builder = match protocol {
            Some(protocol) => builder.protocol_plugin(
                lash_core::testing::test_standard_protocol_factory_with_runtime_state(
                    protocol, None,
                ),
            ),
            None => builder,
        };
        builder
            .provider(provider)
            .model(
                lash::ModelSpec::builder("embed-test")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .effect_host(Arc::clone(&self.effect_host) as Arc<dyn lash::durability::EffectHost>)
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .process_env_store(Arc::new(self.storage.process_env_store()))
            .store_factory(Arc::new(
                self.storage
                    .session_store_factory_with_shared_process_registry(),
            ))
            .process_registry(Arc::new(self.storage.process_registry()))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .without_queued_work()
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "pg-failure-settlement",
                "pg-failure-settlement-boot",
            ))
            .expect("Postgres deployment")
    }
}

/// Replay key of the model call of the session's first turn `turn_id`: turn
/// index 1, protocol iteration 0, effect 2 (effect 1 is the execution
/// environment sync).
fn first_llm_call_key(session_id: &str, turn_id: &str) -> String {
    format!("{session_id}:{turn_id}:1:0:llm_call:2")
}

fn assert_recorded_before_llm_failure(report: &lash::TurnReport) {
    assert!(
        matches!(
            report.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::RuntimeError)
        ),
        "a deterministic before-LLM failure is recorded as a failed turn: {:?}",
        report.outcome
    );
    assert!(
        report
            .errors
            .iter()
            .any(|issue| issue.kind == lash_core::TurnFailureKind::ProtocolBeforeLlmCall),
        "the recorded turn names the protocol failure: {:?}",
        report.errors
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deterministic_before_llm_failure_on_a_direct_turn_is_a_recorded_failed_turn()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(RefusingBeforeLlmCall::default());
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::default()),
        Some(protocol.clone()),
    );
    let session = core.session("pg-direct-before-llm").open().await?;

    let output = session
        .turn(TurnInput::text("refused before the model call"))
        .run()
        .await
        .expect("a deterministic failure is an outcome, not an aborted invocation");

    assert_recorded_before_llm_failure(&output.result);
    assert_eq!(protocol.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deterministic_before_llm_failure_on_a_queued_run_settles_after_one_attempt()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let protocol = Arc::new(RefusingBeforeLlmCall::default());
    let core = deployment.core(
        counting_text_provider(Arc::default(), Arc::default()),
        Some(protocol.clone()),
    );
    let session = core.session("pg-queued-before-llm").open().await?;
    session
        .durable()
        .enqueue(TurnInput::text("queued and refused"))
        .id("queued-refused")
        .send()
        .await?;

    let drained = format!(
        "{:?}",
        session
            .queued_turn()
            .run()
            .await
            .expect("a deterministic failure settles the queued run instead of retaining it")
    );
    assert!(
        drained.contains("RuntimeError") && drained.contains("ProtocolBeforeLlmCall"),
        "the queued run's turn is the recorded failure: {drained}"
    );
    assert_eq!(protocol.calls.load(Ordering::SeqCst), 1);
    assert!(session.durable().pending_queued_run().await?.is_none());
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    let _ = session.queued_turn().run().await?;
    assert_eq!(
        protocol.calls.load(Ordering::SeqCst),
        1,
        "a settled deterministic failure is never re-attempted"
    );
    Ok(())
}

async fn abort_direct_turn_with_live_fault(
    deployment: &PostgresDeployment,
    session: &lash::LashSession,
    session_id: &str,
    turn_id: &str,
) -> (lash::EmbedError, lash_core::InputId) {
    let faults = deployment.effect_host.effect_journal_faults();
    faults.fail_next(
        EffectJournalFaultPoint::Claim,
        &first_llm_call_key(session_id, turn_id),
    );
    let error = session
        .turn(TurnInput::text(STRANDED_WORDS))
        .turn_id(turn_id)
        .run()
        .await
        .expect_err("a live journal fault aborts the direct turn");
    assert!(faults.fired(), "the armed model-call claim fault fired");
    let lash::EmbedError::Runtime(runtime_error) = &error else {
        panic!("the abort is the typed runtime error: {error:?}");
    };
    assert_eq!(
        runtime_error.code,
        lash_core::RuntimeErrorCode::PostgresEffectReplayStore
    );
    let pending = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("read pending inputs");
    assert_eq!(
        pending.len(),
        1,
        "the aborted turn's input is still accepted"
    );
    let input_id = pending[0].input.input_id.clone();
    (error, input_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_fault_on_a_direct_turn_returns_its_receipt_to_withdraw_the_input()
-> Result<(), Box<dyn std::error::Error>> {
    const SESSION: &str = "pg-direct-live-fault";
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = deployment.core(
        counting_text_provider(Arc::default(), Arc::clone(&requests)),
        None,
    );
    let session = core.session(SESSION).open().await?;

    let (error, input_id) =
        abort_direct_turn_with_live_fault(&deployment, &session, SESSION, "faulted-turn").await;
    let cancelled = session
        .durable()
        .cancel_pending_turn_input(&input_id)
        .await?;
    assert!(
        cancelled.is_cancelled(),
        "the aborted turn's input is withdrawable by its receipt: {cancelled:?}"
    );
    assert_eq!(
        error
            .turn_input_acceptance()
            .map(|receipt| &receipt.input_id),
        Some(&input_id),
        "the receipt names the accepted input"
    );

    let next = session.turn(TurnInput::text("the next turn")).run().await?;
    assert!(next.is_success(), "{:?}", next.result.outcome);
    let seen = requests.lock_recover().clone();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].contains(STRANDED_WORDS),
        "the next turn does not contain the withdrawn input"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journal_store_fault_on_a_queued_run_stays_pending_and_completes_on_retry()
-> Result<(), Box<dyn std::error::Error>> {
    const SESSION: &str = "pg-queued-journal-fault";
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::default()),
        None,
    );
    let session = core.session(SESSION).open().await?;
    session
        .durable()
        .enqueue(TurnInput::text("the journal blips once"))
        .id("journal-blip")
        .send()
        .await?;
    let faults = deployment.effect_host.effect_journal_faults();
    faults.fail_next(
        EffectJournalFaultPoint::Claim,
        &first_llm_call_key(SESSION, "queued-turn"),
    );

    match session.queued_turn().turn_id("queued-turn").run().await {
        Err(lash::EmbedError::Runtime(error)) => assert_eq!(
            error.code,
            lash_core::RuntimeErrorCode::QueuedRunPending,
            "a live fault keeps the run for a redrive: {error:?}"
        ),
        other => panic!("a live fault keeps the queued run pending: {other:?}"),
    }
    assert!(faults.fired(), "the armed model-call claim fault fired");
    assert!(session.durable().pending_queued_run().await?.is_some());

    let _ = session.queued_turn().turn_id("queued-turn").run().await?;
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(session.durable().pending_queued_run().await?.is_none());
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_still_settles_stopped_cancelled() -> Result<(), Box<dyn std::error::Error>> {
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let provider = lash::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let entered_tx = Arc::clone(&entered_tx);
            async move {
                if let Some(tx) = entered_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<()>().await;
                unreachable!("the cancelled provider call is dropped")
            }
        })
        .build()
        .into_handle();
    let core = deployment.core(provider, None);
    let session = core.session("pg-direct-cancelled").open().await?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let running = tokio::spawn({
        let session = session.clone();
        let cancel = cancel.clone();
        async move {
            session
                .turn(TurnInput::text("cancel me"))
                .cancel(cancel)
                .run()
                .await
        }
    });
    entered_rx.await.expect("the provider call started");
    cancel.cancel();

    let output = running.await.expect("turn task")?;
    assert!(
        matches!(
            output.result.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { .. })
        ),
        "{:?}",
        output.result.outcome
    );
    Ok(())
}

/// FIG-3589 on PostgreSQL: the aborted turn's input is bound to that turn. A
/// later direct turn under a new lease generation drives only its own input,
/// and the host consumes the bound input with a cancel by the receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_direct_turn_never_folds_in_an_aborted_turns_input()
-> Result<(), Box<dyn std::error::Error>> {
    const SESSION: &str = "pg-direct-live-fault-next-turn";
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = deployment.core(
        counting_text_provider(Arc::default(), Arc::clone(&requests)),
        None,
    );
    let session = core.session(SESSION).open().await?;

    let (error, input_id) =
        abort_direct_turn_with_live_fault(&deployment, &session, SESSION, "bound-turn").await;
    let receipt = error
        .turn_input_acceptance()
        .cloned()
        .expect("an aborted direct turn returns its acceptance receipt");
    let next = session
        .turn(TurnInput::text("the next turn"))
        .turn_id("next-turn")
        .run()
        .await?;
    assert!(next.is_success(), "{:?}", next.result.outcome);
    let seen = requests.lock_recover().clone();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].contains(STRANDED_WORDS),
        "a later direct turn must not fold in the aborted turn's input: {}",
        seen[0]
    );
    let pending = session.durable().pending_turn_inputs().await?;
    assert_eq!(
        pending
            .iter()
            .map(|read| (read.input.input_id.clone(), read.status.clone()))
            .collect::<Vec<_>>(),
        vec![(
            input_id,
            lash_core::PendingTurnInputReadStatus::TurnBound {
                turn_id: lash_core::TurnId::from("bound-turn"),
                receipt_input_id: receipt.input_id.clone(),
            }
        )],
        "the aborted turn's input is still open, bound to its turn"
    );

    // The aborted turn's journal was recorded against the head before the
    // later turn committed, so its redrive can no longer replay it.
    let late_redrive = session
        .turn(TurnInput::text(STRANDED_WORDS))
        .turn_id("bound-turn")
        .run()
        .await
        .expect_err("a redrive after a later turn committed cannot replay the aborted journal");
    let lash::EmbedError::Runtime(late_redrive) = late_redrive else {
        panic!("the refused redrive is a runtime error: {late_redrive:?}");
    };
    assert_eq!(
        late_redrive.code,
        lash_core::RuntimeErrorCode::PostgresEffectReplayHashConflict,
        "{late_redrive:?}"
    );

    let cancelled = session
        .durable()
        .cancel_pending_turn_input(&receipt.input_id)
        .await?;
    assert!(cancelled.is_cancelled(), "{cancelled:?}");
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// FIG-3589 on PostgreSQL: redriving the aborted turn id replays its journal
/// and settles the bound input once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_fault_on_a_direct_turn_is_redriven_by_its_turn_id()
-> Result<(), Box<dyn std::error::Error>> {
    const SESSION: &str = "pg-direct-live-fault-redrive";
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::clone(&requests)),
        None,
    );
    let session = core.session(SESSION).open().await?;

    let (error, _) =
        abort_direct_turn_with_live_fault(&deployment, &session, SESSION, "redriven-turn").await;
    let receipt = error
        .turn_input_acceptance()
        .cloned()
        .expect("an aborted direct turn returns its acceptance receipt");
    assert_eq!(
        session.durable().pending_turn_inputs().await?[0].status,
        lash_core::PendingTurnInputReadStatus::TurnBound {
            turn_id: lash_core::TurnId::from("redriven-turn"),
            receipt_input_id: receipt.input_id.clone(),
        }
    );

    let redriven = session
        .turn(TurnInput::text(STRANDED_WORDS))
        .turn_id("redriven-turn")
        .run()
        .await?;
    assert!(redriven.is_success(), "{:?}", redriven.result.outcome);
    assert_eq!(redriven.result.acceptance.as_ref(), Some(&receipt));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        requests.lock_recover()[0].matches(STRANDED_WORDS).count(),
        1,
        "the redriven turn carries its input exactly once"
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// FIG-3589 keeps crash recovery on PostgreSQL: a direct turn whose worker
/// dies mid-turn never binds its claim, so the next lease generation reclaims
/// the input and answers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_direct_turns_input_is_reclaimed_by_the_next_generation()
-> Result<(), Box<dyn std::error::Error>> {
    const SESSION: &str = "pg-direct-crash-reclaim";
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = lash::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let requests = Arc::clone(&requests);
            move |request| {
                let entered_tx = Arc::clone(&entered_tx);
                let requests = Arc::clone(&requests);
                async move {
                    let first_call = entered_tx.lock_recover().take();
                    if let Some(tx) = first_call {
                        let _ = tx.send(());
                        // The worker dies here: the call never returns and the
                        // turn's future is dropped, so no abort path runs.
                        std::future::pending::<()>().await;
                    }
                    requests
                        .lock_recover()
                        .push(serde_json::to_string(&request.messages).expect("serialize request"));
                    Ok(text_response("answered"))
                }
            }
        })
        .build()
        .into_handle();
    let core = deployment.core(provider, None);
    let session = core.session(SESSION).open().await?;
    let crashed = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .turn(TurnInput::text(STRANDED_WORDS))
                .turn_id("crashed-turn")
                .run()
                .await
        }
    });
    entered_rx
        .await
        .expect("the crashed turn reached its model call");
    crashed.abort();
    assert!(crashed.await.is_err_and(|error| error.is_cancelled()));

    let mut attempts = 0;
    let next = loop {
        match session
            .turn(TurnInput::text("the next turn"))
            .turn_id("next-generation-turn")
            .run()
            .await
        {
            Err(lash::EmbedError::Runtime(error))
                if error.code == lash_core::RuntimeErrorCode::SessionExecutionLaneBusy
                    && attempts < 200 =>
            {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            other => break other?,
        }
    };
    assert!(next.is_success(), "{:?}", next.result.outcome);
    let seen = requests.lock_recover().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].matches(STRANDED_WORDS).count(),
        1,
        "the next generation reclaims the crashed turn's input and answers it once: {}",
        seen[0]
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// FIG-3589 on PostgreSQL: the drive's journal fails to finalize after the
/// drive's body claimed the input. The abort binds the claim the accepted row
/// carries under this lease generation, so a later direct turn never folds the
/// input in; the host cancels it by the receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_whose_outcome_was_lost_still_binds_its_input()
-> Result<(), Box<dyn std::error::Error>> {
    const SESSION: &str = "pg-direct-drive-finalize-fault";
    let Some(deployment) = PostgresDeployment::open().await else {
        return Ok(());
    };
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = deployment.core(
        counting_text_provider(Arc::default(), Arc::clone(&requests)),
        None,
    );
    let session = core.session(SESSION).open().await?;
    let faults = deployment.effect_host.effect_journal_faults();
    faults.fail_next(
        EffectJournalFaultPoint::Finalize,
        &format!("{SESSION}:drive-lost-turn:accept_turn_input:claim_accepted_turn_input"),
    );
    let error = session
        .turn(TurnInput::text(STRANDED_WORDS))
        .turn_id("drive-lost-turn")
        .run()
        .await
        .expect_err("a drive whose journal cannot finalize aborts the turn");
    assert!(faults.fired(), "the armed drive finalize fault fired");
    let receipt = error
        .turn_input_acceptance()
        .cloned()
        .expect("the aborted turn returns its acceptance receipt");
    assert_eq!(
        session.durable().pending_turn_inputs().await?[0].status,
        lash_core::PendingTurnInputReadStatus::TurnBound {
            turn_id: lash_core::TurnId::from("drive-lost-turn"),
            receipt_input_id: receipt.input_id.clone(),
        },
        "the rows the lost drive claimed are bound to the aborted turn"
    );

    let next = session
        .turn(TurnInput::text("the next turn"))
        .turn_id("next-turn")
        .run()
        .await?;
    assert!(next.is_success(), "{:?}", next.result.outcome);
    let seen = requests.lock_recover().clone();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].contains(STRANDED_WORDS),
        "a later direct turn must not fold in the lost drive's input: {}",
        seen[0]
    );
    let cancelled = session
        .durable()
        .cancel_pending_turn_input(&receipt.input_id)
        .await?;
    assert!(cancelled.is_cancelled(), "{cancelled:?}");
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}
