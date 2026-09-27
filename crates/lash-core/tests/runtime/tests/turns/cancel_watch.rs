//! A model call's watch on the turn's cancellation gate, on the Restate server
//! double (FIG-3672 P9, FIG-3668).
//!
//! The watch rides a retry ladder of [`GATE_RETRY_ATTEMPTS`] tries. A fault it
//! rides out never touches the call. A watch that exhausts the ladder ends the
//! **attempt**, not the step and never the turn: the fault is live and
//! unrecorded, so the engine runs the step again under its invocation retry
//! policy (ADR 0104 O3). When that policy is spent — a deployment's turn
//! handler pauses after `lash_restate::TURN_HANDLER_MAX_ATTEMPTS` — the
//! invocation pauses and the turn is stalled with the fault as its last
//! failure: never `Cancelled`, never provider-cancelled evidence, never a
//! terminal failure. Nothing surfaces the fault to the caller as an error;
//! that was the SQLite engine's contract, which has no invocation to retry.
//!
//! Each law runs the turn with `run_in_handler`, whose body the server re-runs
//! from the top on every retry, as a deployment re-runs a turn's handler.

use super::*;
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x36_68ca;

/// How many watches one ladder makes before it gives up.
const GATE_RETRY_ATTEMPTS: usize = 8;

/// The server's invocation retry policy for the pause law: few attempts, no
/// backoff to wait out, then pause.
fn pausing_after(attempts: u32) -> lash_restate_test::RetryPolicy {
    lash_restate_test::RetryPolicy {
        initial_interval: std::time::Duration::from_millis(1),
        exponentiation_factor: 1.0,
        max_interval: std::time::Duration::from_millis(1),
        max_attempts: Some(attempts),
        on_max_attempts: lash_restate_test::server::OnMaxAttempts::Pause,
    }
}

fn completed_call() -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: "completed through the watch faults".to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

/// What the handler's turn ran to: how many times the model was called and
/// what the last attempt's turn returned.
struct WatchedTurn {
    model_calls: usize,
    handler: Result<(), String>,
    outcome: Option<Result<TurnOutcome, String>>,
    handler_attempts: u32,
}

/// Run one turn in a handler on the double, its model call over `recorder`'s
/// cancellation-gate watch. The `n`th model call (0-based) answers what
/// `complete(n)` resolves to; `None` never answers, so only the watch can end
/// it.
async fn run_watched_turn<F, Fut>(
    config: lash_restate_test::ServerConfig,
    recorder: super::effect::RecordingEffectController,
    turn_id: &str,
    complete: F,
) -> WatchedTurn
where
    F: Fn(usize) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<LlmResponse>> + Send + 'static,
{
    let double = kernel_double(SEED, config).await;
    let backend = double.lash_backend();
    let model_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            let complete = Arc::new(complete);
            move |_request| {
                let call = model_calls.fetch_add(1, Ordering::SeqCst);
                let answer = complete(call);
                async move {
                    match answer.await {
                        Some(response) => Ok(response),
                        None => std::future::pending().await,
                    }
                }
            }
        })
        .build();
    let clock = Arc::new(CancelWatchTestClock(lash_core::testing::TestClock::new(0)));
    let host_clock: Arc<dyn lash_core::Clock> = clock;
    let layer: Arc<dyn lash_core::testing::EffectLayer> = Arc::new(recorder);
    let config = super::effect::runtime_host_config_with_effect_layer(&backend, layer)
        .with_clock(host_clock);
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(CountingEchoTool {
            executions: Arc::new(AtomicUsize::new(0)),
        }),
        transport,
        EmbeddedRuntimeHost::new(config),
    )
    .await;
    let session_id = runtime.session_id().to_string();
    let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
    let outcome: Arc<std::sync::Mutex<Option<Result<TurnOutcome, String>>>> = Arc::default();
    let attempt: lash_restate_test::HandlerAttempt = {
        let runtime = Arc::clone(&runtime);
        let outcome = Arc::clone(&outcome);
        Arc::new(move |scoped| {
            let runtime = Arc::clone(&runtime);
            let outcome = Arc::clone(&outcome);
            Box::pin(async move {
                let turn = runtime
                    .lock()
                    .await
                    .drive_turn(
                        TurnInput::text("run a model call over a failing cancellation watch"),
                        TurnOptions::new(CancellationToken::new(), scoped),
                    )
                    .await
                    .map(|turn| turn.outcome)
                    .map_err(|error| format!("{}: {error}", error.code));
                *outcome.lock_recover() = Some(turn);
            })
        })
    };
    let handler = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        double.run_in_handler(
            AdmittedScope::turn(session_id.as_str(), TurnId::from(turn_id)),
            attempt,
        ),
    )
    .await
    .expect("the turn's handler completes or pauses");
    let handler_attempts = double
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTestHandlerHost/"))
        .expect("the turn's handler invocation ran")
        .attempts;
    let model_calls = model_calls.load(Ordering::SeqCst);
    let outcome = outcome.lock_recover().take();
    WatchedTurn {
        model_calls,
        handler,
        outcome,
        handler_attempts,
    }
}

/// A transient fault watching the turn's cancellation gate during a model
/// call is retried inside the attempt; it never stops the call, so the turn
/// completes on its first attempt as it would have without the fault.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_transient_cancel_watch_fault_never_cancels_the_model_call() {
    let recorder =
        super::effect::RecordingEffectController::default().with_transient_cancel_watch_failures(3);
    let turn = Box::pin(run_watched_turn(
        lash_restate_test::ServerConfig::default(),
        recorder.clone(),
        "transient-watch",
        {
            let watched = recorder.clone();
            move |_| {
                let watched = watched.clone();
                async move {
                    // The call outlives the faults: it ends only after the
                    // watch has failed three times and retried past them.
                    while watched.cancel_watch_attempts() < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    Some(completed_call())
                }
            }
        },
    ))
    .await;
    turn.handler.expect("the turn's handler completes");
    assert_eq!(recorder.cancel_watch_attempts(), 3);
    assert_eq!(
        turn.model_calls, 1,
        "the model ran once, on the first attempt"
    );
    assert_eq!(turn.handler_attempts, 1, "no attempt ended over the fault");
    let outcome = turn
        .outcome
        .expect("the handler ran the turn")
        .expect("the turn assembles");
    assert!(
        matches!(outcome, TurnOutcome::Finished(_)),
        "a watch fault must never become a cancellation: {outcome:?}"
    );
}

/// A watch that exhausts its ladder ends the attempt closed: the model call is
/// dropped unrecorded and the engine retries the invocation, whose replay runs
/// the step again under a fresh watch. The turn finishes on that retry — the
/// lost watch was never a cancellation and never the step's outcome.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn an_exhausted_cancel_watch_fails_the_attempt_closed_not_cancelled() {
    let recorder = super::effect::RecordingEffectController::default()
        .with_transient_cancel_watch_failures(GATE_RETRY_ATTEMPTS);
    let turn = Box::pin(run_watched_turn(
        lash_restate_test::ServerConfig::default(),
        recorder.clone(),
        "exhausted-watch",
        // The first call never answers: only the lost watch ends it.
        |call| std::future::ready((call > 0).then(completed_call)),
    ))
    .await;
    turn.handler.expect("the retried handler completes");
    assert_eq!(
        recorder.cancel_watch_attempts(),
        GATE_RETRY_ATTEMPTS,
        "the watch rode the whole ladder before giving up"
    );
    assert_eq!(
        turn.model_calls, 2,
        "the lost watch was never recorded, so the retry ran the model call again"
    );
    assert_eq!(
        turn.handler_attempts, 2,
        "the exhausted watch ended exactly one attempt"
    );
    let outcome = turn
        .outcome
        .expect("the retry ran the turn")
        .expect("the retried turn assembles");
    assert!(
        matches!(outcome, TurnOutcome::Finished(_)),
        "an exhausted watch must never become a cancellation: {outcome:?}"
    );
}

/// A watch that never recovers spends the engine's retry policy and the
/// invocation pauses: the turn is stalled, with the lost watch as its last
/// failure. No attempt settled it — neither `Cancelled` nor failed.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_watch_lost_on_every_attempt_pauses_the_turn_not_cancels_it() {
    const ATTEMPTS: u32 = 3;
    let recorder =
        super::effect::RecordingEffectController::default().with_always_failing_cancel_watch();
    recorder.release_cancel_watch_failures();
    let turn = Box::pin(run_watched_turn(
        lash_restate_test::ServerConfig::default().retry(pausing_after(ATTEMPTS)),
        recorder.clone(),
        "lost-watch",
        |_| std::future::ready(None),
    ))
    .await;
    let paused = turn
        .handler
        .expect_err("a watch lost on every attempt pauses the invocation");
    assert!(
        paused.contains(&format!("paused after {ATTEMPTS} attempts"))
            && paused.contains("transient_cancel_watch"),
        "the pause names the lost watch as its last failure: {paused}"
    );
    assert_eq!(
        recorder.cancel_watch_attempts(),
        GATE_RETRY_ATTEMPTS * ATTEMPTS as usize,
        "every attempt rode its own whole ladder"
    );
    assert_eq!(turn.model_calls, ATTEMPTS as usize);
    assert!(
        turn.outcome.is_none(),
        "no attempt settled the turn: {:?}",
        turn.outcome
    );
}
