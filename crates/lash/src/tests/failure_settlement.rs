//! FIG-3575: a turn failure settles by its cause, identically on every host.
//!
//! Each law runs the facade over a file-backed SQLite deployment — the
//! journaled `SqliteEffectHost` beside `SqliteSessionStoreFactory` — which is
//! where the tier-keyed rule was live: a deterministic failure before the model
//! call aborted instead of being recorded, a queued run retried it and stayed
//! pending, and an aborted direct turn kept the claim on its own input.
//!
//! * A deterministic failure is an outcome: a direct turn records it as a
//!   failed turn, and a queued run settles once.
//! * A live fault aborts with `Err`. The aborted direct turn's error carries
//!   its acceptance receipt: the host withdraws the input by it, or redrives
//!   the same turn id, which replays the journal and commits once. Until then
//!   the input is bound to the aborted turn (FIG-3589): no later direct turn
//!   and no drain folds it in, while a crashed turn's input is still
//!   reclaimed by the next lease generation. A queued run stays pending and a
//!   retry completes it.
//! * A failure the journal already holds is an outcome on every redrive,
//!   whatever its code, so it never becomes an abort loop.
//! * Cancellation keeps settling `Stopped { Cancelled }`.

use super::*;
use lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint;

const STRANDED_WORDS: &str = "words of the turn that aborted";

/// A protocol whose `before_llm_call` fails every time: a deterministic
/// failure over the turn's journaled inputs.
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
    ) -> std::result::Result<Option<lash_core::ProtocolLlmCallAction>, lash_core::PluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::PluginError::Invoke(
            "the protocol refuses this request".to_string(),
        ))
    }
}

struct SqliteDeployment {
    directory: tempfile::TempDir,
    effect_host: Arc<lash_sqlite_store::SqliteEffectHost>,
}

impl SqliteDeployment {
    async fn open() -> Self {
        let directory = tempfile::tempdir().expect("temporary durable deployment");
        let effect_host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&directory.path().join("effects.sqlite"))
                .await
                .expect("file-backed SQLite effect journal"),
        );
        Self {
            directory,
            effect_host,
        }
    }

    fn core(
        &self,
        provider: ProviderHandle,
        protocol: Option<Arc<dyn lash_core::plugin::ProtocolSessionPlugin>>,
    ) -> LashCore {
        self.core_with_plugins(provider, protocol, Vec::new())
    }

    fn core_with_plugins(
        &self,
        provider: ProviderHandle,
        protocol: Option<Arc<dyn lash_core::plugin::ProtocolSessionPlugin>>,
        plugins: Vec<Arc<dyn PluginFactory>>,
    ) -> LashCore {
        let builder = LashCore::standard_builder(crate::TurnBudget::Unbounded);
        let builder = match protocol {
            Some(protocol) => builder.protocol_plugin(
                lash_core::testing::test_standard_protocol_factory_with_runtime_state(
                    protocol, None,
                ),
            ),
            None => builder,
        };
        let builder = plugins
            .into_iter()
            .fold(builder, |builder, plugin| builder.plugin(plugin));
        explicit_ephemeral_facets(builder)
            .provider(provider)
            .model(mock_model_spec())
            .effect_host(Arc::clone(&self.effect_host) as Arc<dyn EffectHost>)
            .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                self.directory.path().join("sessions"),
            )))
            .build(crate::testing::runtime_lease_owner())
            .expect("file-backed SQLite deployment")
    }
}

fn counting_text_provider(
    calls: Arc<AtomicUsize>,
    requests: Arc<StdMutex<Vec<String>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            let requests = Arc::clone(&requests);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                requests.lock_recover().push(request_text(&request));
                Ok(text_response("answered"))
            }
        })
        .build()
        .into_handle()
}

/// Replay key of the model call of the session's first turn `turn_id`: turn
/// index 1, protocol iteration 0, effect 2 (effect 1 is the execution
/// environment sync).
fn first_llm_call_key(session_id: &str, turn_id: &str) -> String {
    format!("{session_id}:{turn_id}:1:0:llm_call:2")
}

fn assert_recorded_before_llm_failure(report: &TurnReport) {
    assert!(
        matches!(
            report.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::RuntimeError)
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
async fn deterministic_before_llm_failure_on_a_direct_turn_is_a_recorded_failed_turn() -> Result<()>
{
    let deployment = SqliteDeployment::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(RefusingBeforeLlmCall::default());
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::default()),
        Some(protocol.clone()),
    );
    let session = core.session("direct-before-llm").open().await?;

    let output = session
        .turn(TurnInput::text("refused before the model call"))
        .run()
        .await
        .expect("a deterministic failure is an outcome, not an aborted invocation");

    assert_recorded_before_llm_failure(&output.result);
    assert_eq!(protocol.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let acceptance = output
        .result
        .acceptance
        .clone()
        .expect("the recorded turn carries its acceptance");
    assert!(
        session.durable().pending_turn_inputs().await?.is_empty(),
        "the recorded failure settles the turn's own input"
    );
    assert!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .iter()
            .any(|application| application.input_id == acceptance.input_id),
        "the failed turn committed its input"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deterministic_before_llm_failure_on_a_queued_run_settles_after_one_attempt() -> Result<()>
{
    let deployment = SqliteDeployment::open().await;
    let protocol = Arc::new(RefusingBeforeLlmCall::default());
    let core = deployment.core(
        counting_text_provider(Arc::default(), Arc::default()),
        Some(protocol.clone()),
    );
    let session = core.session("queued-before-llm").open().await?;
    session
        .durable()
        .enqueue(TurnInput::text("queued and refused"))
        .id("queued-refused")
        .send()
        .await?;

    let drained = session
        .queued_turn()
        .run()
        .await
        .expect("a deterministic failure settles the queued run instead of retaining it");

    let drained = format!("{drained:?}");
    assert!(
        drained.contains("RuntimeError") && drained.contains("ProtocolBeforeLlmCall"),
        "the queued run's turn is the recorded failure: {drained}"
    );
    assert_eq!(protocol.calls.load(Ordering::SeqCst), 1);
    assert!(
        session.durable().pending_queued_run().await?.is_none(),
        "the run settled after one attempt; nothing is retained for retry"
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    let again = format!("{:?}", session.queued_turn().run().await?);
    assert_eq!(
        protocol.calls.load(Ordering::SeqCst),
        1,
        "a settled deterministic failure is never re-attempted: {again}"
    );
    Ok(())
}

/// Aborts `turn_id`'s first model call with a live journal fault, and returns
/// the error with the id of the one input the aborted turn accepted.
async fn abort_direct_turn_with_live_fault(
    deployment: &SqliteDeployment,
    session: &crate::LashSession,
    session_id: &str,
    turn_id: &str,
) -> (EmbedError, lash_core::InputId) {
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
    let EmbedError::Runtime(runtime_error) = &error else {
        panic!("the abort is the typed runtime error: {error:?}");
    };
    assert_eq!(
        runtime_error.code,
        lash_core::RuntimeErrorCode::SqliteEffectReplayStore,
        "the abort carries the journal's own store fault"
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
async fn live_fault_on_a_direct_turn_returns_its_receipt_to_withdraw_the_input() -> Result<()> {
    const SESSION: &str = "direct-live-fault";
    let deployment = SqliteDeployment::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::clone(&requests)),
        None,
    );
    let session = core.session(SESSION).open().await?;

    let (error, input_id) =
        abort_direct_turn_with_live_fault(&deployment, &session, SESSION, "faulted-turn").await;
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let receipt = error
        .turn_input_acceptance()
        .expect("an aborted direct turn returns its acceptance receipt");
    assert_eq!(
        receipt.input_id, input_id,
        "the receipt names the accepted input"
    );
    assert_eq!(receipt.session_id.as_str(), SESSION);

    // The host withdraws the input by the name the receipt gives it.
    let cancelled = session
        .durable()
        .cancel_pending_turn_input(&receipt.input_id)
        .await?;
    assert!(
        cancelled.is_cancelled(),
        "the aborted turn's input is withdrawable by its receipt: {cancelled:?}"
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());

    let next = session.turn(TurnInput::text("the next turn")).run().await?;
    assert!(next.is_success(), "{:?}", next.result.outcome);
    let seen = requests.lock_recover().clone();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].contains(STRANDED_WORDS),
        "the next turn does not contain the withdrawn input: {}",
        seen[0]
    );
    Ok(())
}

/// The aborted turn's journal is the recovery path: redriving the same turn
/// id replays the recorded acceptance and drive and commits once, with the
/// receipt's acceptance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_fault_on_a_direct_turn_is_redriven_by_its_turn_id() -> Result<()> {
    const SESSION: &str = "direct-live-fault-redrive";
    let deployment = SqliteDeployment::open().await;
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
        },
        "until its redrive, the input is bound to the aborted turn"
    );

    let redriven = session
        .turn(TurnInput::text(STRANDED_WORDS))
        .turn_id("redriven-turn")
        .run()
        .await?;
    assert!(redriven.is_success(), "{:?}", redriven.result.outcome);
    assert_eq!(
        redriven.result.acceptance.as_ref(),
        Some(&receipt),
        "the redrive re-derives the aborted turn's own acceptance"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        requests.lock_recover()[0].matches(STRANDED_WORDS).count(),
        1,
        "the redriven turn carries its input exactly once"
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// FIG-3589: the aborted turn's input is bound to that turn. A later direct
/// turn under a new lease generation drives only its own input and never
/// folds the aborted one in. The aborted turn's journal was recorded against
/// the session head it ran on, so once a later turn commits its redrive can no
/// longer replay; the input stays bound until the host cancels it by the
/// receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_direct_turn_never_folds_in_an_aborted_turns_input() -> Result<()> {
    const SESSION: &str = "direct-live-fault-next-turn";
    let deployment = SqliteDeployment::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::clone(&requests)),
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
    {
        let seen = requests.lock_recover();
        assert_eq!(seen.len(), 1);
        assert!(
            !seen[0].contains(STRANDED_WORDS),
            "a later direct turn must not fold in the aborted turn's input: {}",
            seen[0]
        );
    }
    let bound = vec![(
        input_id.clone(),
        lash_core::PendingTurnInputReadStatus::TurnBound {
            turn_id: lash_core::TurnId::from("bound-turn"),
        },
    )];
    let open = |reads: Vec<crate::PendingTurnInputRead>| {
        reads
            .into_iter()
            .map(|read| (read.input.input_id, read.status))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        open(session.durable().pending_turn_inputs().await?),
        bound,
        "the aborted turn's input is still open, bound to its turn"
    );

    let late_redrive = session
        .turn(TurnInput::text(STRANDED_WORDS))
        .turn_id("bound-turn")
        .run()
        .await
        .expect_err("a redrive after a later turn committed cannot replay the aborted journal");
    let EmbedError::Runtime(late_redrive) = late_redrive else {
        panic!("the refused redrive is a runtime error: {late_redrive:?}");
    };
    assert_eq!(
        late_redrive.code,
        lash_core::RuntimeErrorCode::SqliteEffectReplayHashConflict,
        "{late_redrive:?}"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        open(session.durable().pending_turn_inputs().await?),
        bound,
        "a refused redrive leaves the input bound"
    );

    let cancelled = session
        .durable()
        .cancel_pending_turn_input(&receipt.input_id)
        .await?;
    assert!(cancelled.is_cancelled(), "{cancelled:?}");
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// FIG-3589: a cancel by the receipt settles the bound input, and the earlier
/// admissions the aborted turn had absorbed into its drive go back to the
/// queue for the next drain instead of staying bound to a turn that can no
/// longer settle them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_bound_input_returns_the_rest_of_its_drive_to_the_queue() -> Result<()> {
    const SESSION: &str = "direct-live-fault-cancel-absorbed";
    const EARLIER_WORDS: &str = "an earlier queued admission";
    let deployment = SqliteDeployment::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::clone(&requests)),
        None,
    );
    let session = core.session(SESSION).open().await?;
    let earlier = session
        .durable()
        .enqueue(TurnInput::text(EARLIER_WORDS))
        .id("earlier-admission")
        .send()
        .await?;
    let faults = deployment.effect_host.effect_journal_faults();
    faults.fail_next(
        EffectJournalFaultPoint::Claim,
        &first_llm_call_key(SESSION, "absorbing-turn"),
    );
    let error = session
        .turn(TurnInput::text(STRANDED_WORDS))
        .turn_id("absorbing-turn")
        .run()
        .await
        .expect_err("a live journal fault aborts the direct turn");
    assert!(faults.fired(), "the armed model-call claim fault fired");
    let receipt = error
        .turn_input_acceptance()
        .cloned()
        .expect("an aborted direct turn returns its acceptance receipt");
    let bound = session.durable().pending_turn_inputs().await?;
    assert_eq!(bound.len(), 2, "both rows of the aborted drive stay open");
    assert!(
        bound.iter().all(|read| read.status
            == lash_core::PendingTurnInputReadStatus::TurnBound {
                turn_id: lash_core::TurnId::from("absorbing-turn"),
            }),
        "the aborted turn's whole drive is bound to it: {bound:?}"
    );

    let cancelled = session
        .durable()
        .cancel_pending_turn_input(&receipt.input_id)
        .await?;
    assert!(cancelled.is_cancelled(), "{cancelled:?}");
    let released = session.durable().pending_turn_inputs().await?;
    assert_eq!(
        released
            .iter()
            .map(|read| (read.input.input_id.clone(), read.status.clone()))
            .collect::<Vec<_>>(),
        vec![(
            earlier.input_id.clone(),
            lash_core::PendingTurnInputReadStatus::Pending
        )],
        "the absorbed admission is back in the queue, unbound"
    );

    let drained = format!("{:?}", session.queued_turn().run().await?);
    assert!(drained.contains("Ran"), "{drained}");
    let seen = requests.lock_recover().clone();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].contains(EARLIER_WORDS), "{}", seen[0]);
    assert!(
        !seen[0].contains(STRANDED_WORDS),
        "the cancelled input is never answered: {}",
        seen[0]
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// FIG-3589: a drain never answers an aborted turn's input either. Only the
/// aborted turn's redrive or a cancel by its receipt consumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drain_never_answers_an_aborted_turns_input() -> Result<()> {
    const SESSION: &str = "direct-live-fault-drain";
    let deployment = SqliteDeployment::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let core = deployment.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::default()),
        None,
    );
    let session = core.session(SESSION).open().await?;

    let (error, input_id) =
        abort_direct_turn_with_live_fault(&deployment, &session, SESSION, "drained-turn").await;
    assert_eq!(
        error
            .turn_input_acceptance()
            .map(|receipt| &receipt.input_id),
        Some(&input_id)
    );

    let drained = format!("{:?}", session.queued_turn().run().await?);
    assert!(
        drained.contains("Empty"),
        "a drain must not answer the aborted turn's input: {drained}"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let pending = session.durable().pending_turn_inputs().await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].status,
        lash_core::PendingTurnInputReadStatus::TurnBound {
            turn_id: lash_core::TurnId::from("drained-turn"),
        }
    );
    Ok(())
}

/// FIG-3589 keeps crash recovery: a direct turn whose worker dies mid-turn
/// never reaches its abort path, so its claim is not bound, and the next lease
/// generation reclaims the input under the ADR 0029 fence and answers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_direct_turns_input_is_reclaimed_by_the_next_generation() -> Result<()> {
    const SESSION: &str = "direct-crash-reclaim";
    let deployment = SqliteDeployment::open().await;
    let (entered_tx, entered_rx) = oneshot::channel::<()>();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
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
                    requests.lock_recover().push(request_text(&request));
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

    // The dropped lease guard releases its lease in the background; the next
    // turn waits for the lane to turn over and then claims under a new
    // generation.
    let mut attempts = 0;
    let next = loop {
        match session
            .turn(TurnInput::text("the next turn"))
            .turn_id("next-generation-turn")
            .run()
            .await
        {
            Err(EmbedError::Runtime(error))
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

fn plugin(id: &'static str, spec: lash_core::facade_support::PluginSpec) -> Arc<dyn PluginFactory> {
    Arc::new(crate::plugins::StaticPluginFactory::new(id, spec))
}

/// An `after_turn` hook that fails with an opaque plugin-session error — a
/// store blip behind a plugin service — the first `failures` times.
fn failing_after_turn(calls: Arc<AtomicUsize>, failures: usize) -> Arc<dyn PluginFactory> {
    plugin(
        "failure-settlement-after-turn",
        lash_core::facade_support::PluginSpec::new().with_after_turn(Arc::new(move |_| {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) < failures {
                    return Err(lash_core::PluginError::Session(
                        "plugin session store unavailable".to_string(),
                    ));
                }
                Ok(Vec::new())
            })
        })),
    )
}

fn assert_queued_run_pending(result: Result<crate::QueuedTurnDrain<crate::TurnOutput>>) {
    match result {
        Err(EmbedError::Runtime(error)) => assert_eq!(
            error.code,
            lash_core::RuntimeErrorCode::QueuedRunPending,
            "a live fault keeps the run for a redrive: {error:?}"
        ),
        other => panic!("a live fault keeps the queued run pending: {other:?}"),
    }
}

/// F1: an opaque plugin-session failure in a queued run's finalize hook is a
/// live fault. The run stays pending for its retry budget and completes on
/// the retry instead of settling failed for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_session_fault_in_a_queued_finalize_hook_is_retried_to_completion() -> Result<()> {
    let deployment = SqliteDeployment::open().await;
    let finalize_calls = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let core = deployment.core_with_plugins(
        counting_text_provider(Arc::clone(&provider_calls), Arc::default()),
        None,
        vec![failing_after_turn(Arc::clone(&finalize_calls), 1)],
    );
    let session = core.session("queued-finalize-fault").open().await?;
    session
        .durable()
        .enqueue(TurnInput::text("finalize blips once"))
        .id("finalize-blip")
        .send()
        .await?;

    assert_queued_run_pending(session.queued_turn().run().await);
    assert!(session.durable().pending_queued_run().await?.is_some());

    let completed = format!("{:?}", session.queued_turn().run().await?);
    assert!(
        completed.contains("Ran") || completed.contains("Replayed"),
        "{completed}"
    );
    assert_eq!(finalize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "the retry replays the journaled model call"
    );
    assert!(session.durable().pending_queued_run().await?.is_none());
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// F3: a journal store fault is live and not terminal. On a queued run it
/// keeps the run pending, and the retry completes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journal_store_fault_on_a_queued_run_stays_pending_and_completes_on_retry() -> Result<()>
{
    const SESSION: &str = "queued-journal-fault";
    let deployment = SqliteDeployment::open().await;
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

    assert_queued_run_pending(session.queued_turn().turn_id("queued-turn").run().await);
    assert!(faults.fired(), "the armed model-call claim fault fired");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(session.durable().pending_queued_run().await?.is_some());

    let completed = format!(
        "{:?}",
        session.queued_turn().turn_id("queued-turn").run().await?
    );
    assert!(
        completed.contains("Ran") || completed.contains("Replayed"),
        "{completed}"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(session.durable().pending_queued_run().await?.is_none());
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// F4: a failure the journal already holds is an outcome on every redrive,
/// whatever its code. The checkpoint hook fails with a live-coded plugin
/// session fault, which the checkpoint's journaled outcome records; the first
/// attempt is then interrupted by a live finalize fault. The redrive replays
/// the recorded checkpoint failure and settles it as a failed turn, instead of
/// aborting on the replayed live code forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journaled_live_coded_failure_replays_as_a_recorded_failed_turn() -> Result<()> {
    let deployment = SqliteDeployment::open().await;
    let checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let finalize_calls = Arc::new(AtomicUsize::new(0));
    let failing_checkpoint = plugin(
        "failure-settlement-checkpoint",
        lash_core::facade_support::PluginSpec::new().with_checkpoint(Arc::new({
            let checkpoint_calls = Arc::clone(&checkpoint_calls);
            move |_| {
                let checkpoint_calls = Arc::clone(&checkpoint_calls);
                Box::pin(async move {
                    checkpoint_calls.fetch_add(1, Ordering::SeqCst);
                    Err(lash_core::PluginError::Session(
                        "checkpoint store unavailable".to_string(),
                    ))
                })
            }
        })),
    );
    let core = deployment.core_with_plugins(
        counting_text_provider(Arc::default(), Arc::default()),
        None,
        vec![
            failing_checkpoint,
            failing_after_turn(Arc::clone(&finalize_calls), 1),
        ],
    );
    let session = core.session("queued-journaled-failure").open().await?;
    session
        .durable()
        .enqueue(TurnInput::text("checkpoint fails and is journaled"))
        .id("journaled-failure")
        .send()
        .await?;

    assert_queued_run_pending(session.queued_turn().run().await);
    let recorded_checkpoints = checkpoint_calls.load(Ordering::SeqCst);
    assert!(recorded_checkpoints > 0, "the checkpoint ran and failed");

    let settled = format!("{:?}", session.queued_turn().run().await?);
    assert!(
        settled.contains("RuntimeError") && settled.contains("plugin_session_manager"),
        "the redrive records the journaled failure as a failed turn: {settled}"
    );
    assert_eq!(
        checkpoint_calls.load(Ordering::SeqCst),
        recorded_checkpoints,
        "the redrive replays the recorded checkpoint instead of re-running it"
    );
    assert!(session.durable().pending_queued_run().await?.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_still_settles_stopped_cancelled() -> Result<()> {
    let deployment = SqliteDeployment::open().await;
    let (entered_tx, entered_rx) = oneshot::channel::<()>();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let provider = crate::testing::TestProvider::builder()
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
    let session = core.session("direct-cancelled").open().await?;
    let cancel = CancellationToken::new();
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
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
        ),
        "{:?}",
        output.result.outcome
    );
    Ok(())
}
