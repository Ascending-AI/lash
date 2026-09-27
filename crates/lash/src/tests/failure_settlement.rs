//! FIG-3575: a turn failure settles by its cause, identically on every host.
//!
//! Each law runs the facade over the Restate double — the engine's own
//! handler controller beside the SQLite session stores.
//!
//! * A deterministic failure is an outcome: a direct turn records it as a
//!   failed turn.
//! * A replay refusal parks the direct turn until its input is withdrawn.
//! * An accepted input is withdrawable by its send receipt before it drives.
//! * Cancellation keeps settling `Stopped { Cancelled }`.
//!
//! A live fault is the engine's to retry (FIG-3897): the Restate engine
//! reruns the root under its own attempt, so the host-side redrive-by-turn-id
//! laws the store-journal host had are the engine's retry laws in
//! `lash-restate`.

use super::*;

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

/// The Restate double a law's cores run over, held by the test.
struct SqliteBackend {
    backend: lash_core::Backend,
}

impl SqliteBackend {
    async fn open() -> Self {
        Self {
            backend: double_backend().await,
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
        let builder =
            LashCore::standard_builder(self.backend.clone(), crate::TurnBudget::Unbounded);
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
            .build(crate::testing::runtime_lease_owner())
            .expect("a core over the Restate double")
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
    let backend = SqliteBackend::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(RefusingBeforeLlmCall::default());
    let core = backend.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::default()),
        Some(protocol.clone()),
    );
    let session = core.session("direct-before-llm").open().await?;

    let output = session
        .send(TurnInput::text("refused before the model call"))
        .output()
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

/// A protocol whose `before_llm_call` meets a replay refusal every time, as a
/// code cell does when its re-execution diverges from its journal (FIG-3586).
#[derive(Default)]
struct DivergingBeforeLlmCall {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for DivergingBeforeLlmCall {
    async fn before_llm_call(
        &self,
        _ctx: lash_core::plugin::ProtocolBeforeLlmCallContext,
        _request: &LlmRequest,
    ) -> std::result::Result<Option<lash_core::ProtocolLlmCallAction>, lash_core::PluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::PluginError::RuntimeEffectController(
            lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::LashlangCellReplayDivergence,
                "lashlang run diverged from its journal at issue ordinal 0",
            ),
        ))
    }
}

/// FIG-3586, FIG-3600: a replay refusal parks the sent root without a failed
/// turn report. Reattaching observes the same park and withdrawal clears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_refusal_parks_the_direct_turn_until_its_input_is_withdrawn() -> Result<()> {
    const SESSION: &str = "direct-replay-refusal";
    let backend = SqliteBackend::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(DivergingBeforeLlmCall::default());
    let core = backend.core(
        counting_text_provider(Arc::clone(&provider_calls), Arc::default()),
        Some(protocol.clone()),
    );
    let session = core.session(SESSION).open().await?;

    let handle = session
        .send(TurnInput::text(STRANDED_WORDS))
        .id("parked-turn")
        .await?;
    let input_id = handle.input_id().clone();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), handle.outcome())
        .await
        .expect("the first send answers its park")?;
    let reobserved = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        session.root("parked-turn").outcome(),
    )
    .await
    .expect("the root handle observes the same park")?;
    for outcome in [first, reobserved] {
        assert!(matches!(
            outcome.status,
            crate::TurnStatus::Parked(crate::ParkedTurn {
                reason: lash_core::store::ParkReason::ReplayDivergence { .. },
                ..
            })
        ));
        assert!(outcome.output.is_none(), "a park has no terminal report");
        let status = core.drain_status(false).await?;
        assert_eq!(
            (status.parked_turns, status.in_flight_turns),
            (1, 1),
            "the parked turn is counted"
        );
        assert!(!status.drained());
    }
    // The engine retries a parked attempt (it ends retryably, keeping its
    // journal), and each retry meets the same refusal before the model call.
    assert!(protocol.calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "a parked turn never reaches the model"
    );
    let pending = session.durable().pending_turn_inputs().await?;
    assert_eq!(pending.len(), 1);
    // The park blocks the session's admission, so its input stays pending
    // and no other root drives it (FIG-3600).
    assert!(
        matches!(
            &pending[0].status,
            lash_core::PendingTurnInputReadStatus::Pending
        ),
        "the parked turn's input stays pending: {:?}",
        pending[0].status
    );
    let cancelled = session.cancel(crate::CancelTarget::Input(input_id)).await?;
    assert!(
        matches!(cancelled, crate::CancelReceipt::Withdrawn(_)),
        "{cancelled:?}"
    );
    let status = core.drain_status(false).await?;
    assert_eq!((status.parked_turns, status.in_flight_turns), (0, 0));
    assert!(status.drained(), "withdrawing the input settles the park");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_receipt_withdraws_input_before_drive() -> Result<()> {
    const SESSION: &str = "direct-live-fault";
    let backend = SqliteBackend::open().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(counting_text_provider(
        Arc::clone(&provider_calls),
        Arc::clone(&requests),
    ))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(SESSION).open().await?;
    // The engine drives a send as soon as it is accepted. Hold the session's
    // drive so nothing claims the input before the withdraw below: the
    // withdraw is never a claim race.
    let double = held_double(&core).expect("the core runs on its held double");
    let hold = double.hold_session_drive(&SessionId::from(SESSION)).await;

    let handle = session
        .send(TurnInput::text(STRANDED_WORDS))
        .id("withdrawn-turn")
        .await?;
    let receipt = handle.receipt().clone();
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(receipt.session_id.as_str(), SESSION);

    let cancelled = session
        .cancel(crate::CancelTarget::Input(receipt.input_id.clone()))
        .await?;
    assert!(
        matches!(cancelled, crate::CancelReceipt::Withdrawn(_)),
        "the input is withdrawable by its receipt: {cancelled:?}"
    );
    let outcome = handle.outcome().await?;
    assert_eq!(outcome.status, crate::TurnStatus::Cancelled);
    assert!(outcome.output.is_none());
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    drop(hold);

    let next = session
        .send(TurnInput::text("the next turn"))
        .output()
        .await?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_still_settles_stopped_cancelled() -> Result<()> {
    let backend = SqliteBackend::open().await;
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
    let core = backend.core(provider, None);
    let session = core.session("direct-cancelled").open().await?;
    let cancel = CancellationToken::new();
    let running = tokio::spawn({
        let session = session.clone();
        let cancel = cancel.clone();
        async move {
            output_into_cancelled_by(
                session.send(TurnInput::text("cancel me")),
                &lash_core::facade_support::NoopTurnActivitySink,
                cancel,
                None,
            )
            .await
        }
    });
    entered_rx.await.expect("the provider call started");
    cancel.cancel();

    let report = running.await.expect("turn task")?;
    assert!(
        matches!(
            report.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
        ),
        "{:?}",
        report.outcome
    );
    Ok(())
}
