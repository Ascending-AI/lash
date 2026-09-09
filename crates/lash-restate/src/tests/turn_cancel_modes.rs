//! FIG-635: a durable wait parked on the Restate turn-cancel gate carries the
//! request mode through its wake.
//!
//! An `AfterStep` request that lands while a Restate-owned turn sits in a
//! durable sleep, await-event, or process await must compose to the step
//! boundary: the wait finishes on its own terms, the iteration completes, and
//! the turn stops at the `turn_cancel.after_step.{n}` peek. Only an
//! `Immediate` request — a fresh one or an escalation of a deferred stop —
//! unwinds the wait at its wake.

use super::*;
use lash_core::facade_support::{
    TurnCancelMode, TurnCancelOutcome, TurnCancelRequest, TurnWorkDriver,
};

const SESSION: &str = "session";
const TURN: &str = "turn";

fn cancel_request(request_id: &str, mode: TurnCancelMode) -> TurnCancelRequest {
    TurnCancelRequest::new(TurnAddress::new(SESSION, TURN), request_id, None).mode(mode)
}

fn driver_for<C>(context: Arc<C>) -> TurnWorkDriver
where
    Arc<C>: RestateControllerContext<'static>,
    C: Send + Sync + 'static,
{
    TurnWorkDriver::new(Arc::new(RestateRuntimeEffectController::new(context)))
}

/// Waits (wall-clock bounded) until the gate holds a registration. The process
/// site registers only after the process workflow call has been recorded, which
/// takes more scheduler passes than a fixed yield budget allows.
async fn await_gate_registration(gate: &TestTurnCancelGate) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while gate.registration_count() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "turn cancellation gate was never registered"
        );
        tokio::task::yield_now().await;
    }
}

async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

fn spawn_parked_sleep(
    context: Arc<ReplayableRecordingContext>,
    cancellation: tokio_util::sync::CancellationToken,
    effect_id: &'static str,
) -> tokio::task::JoinHandle<Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>>
{
    tokio::spawn(async move {
        RestateRuntimeEffectController::new(context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Sleep, effect_id),
                    RuntimeEffectCommand::Sleep {
                        duration_ms: 300_000,
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(cancellation)
                    .with_turn_cancel_scope(durable_turn_scope(SESSION, TURN)),
            )
            .await
    })
}

#[tokio::test]
async fn after_step_during_a_parked_sleep_lets_the_timer_finish() {
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let sleep = spawn_parked_sleep(
        Arc::clone(&context),
        cancellation.clone(),
        "fig635-after-step-sleep",
    );
    context.await_sleep_started().await;

    let receipt = driver_for(Arc::clone(&context))
        .request_cancel(cancel_request("stop-in-sleep", TurnCancelMode::AfterStep))
        .await
        .expect("request an after-step stop during the sleep");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    settle().await;
    assert!(
        !sleep.is_finished(),
        "an after-step stop must not wake the timer early"
    );
    assert!(
        !cancellation.is_cancelled(),
        "an after-step stop never fires the cooperative token"
    );

    context.release_sleep();
    let outcome = tokio::time::timeout(Duration::from_secs(2), sleep)
        .await
        .expect("the released timer finishes")
        .expect("join the sleep task")
        .expect("the sleep completes on its own terms after an after-step stop");
    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
    assert!(!cancellation.is_cancelled());
}

#[tokio::test]
async fn immediate_during_a_parked_sleep_still_aborts_at_wake() {
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let sleep = spawn_parked_sleep(
        Arc::clone(&context),
        cancellation.clone(),
        "fig635-immediate-sleep",
    );
    context.await_sleep_started().await;

    let receipt = driver_for(Arc::clone(&context))
        .request_cancel(cancel_request("abort-in-sleep", TurnCancelMode::Immediate))
        .await
        .expect("request an immediate abort during the sleep");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));

    let error = tokio::time::timeout(Duration::from_secs(2), sleep)
        .await
        .expect("an immediate abort wakes the parked timer")
        .expect("join the sleep task")
        .expect_err("an immediate abort unwinds the sleep");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled
    );
    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn escalating_a_deferred_stop_aborts_the_parked_sleep() {
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let sleep = spawn_parked_sleep(
        Arc::clone(&context),
        cancellation.clone(),
        "fig635-escalated-sleep",
    );
    context.await_sleep_started().await;
    let driver = driver_for(Arc::clone(&context));

    let receipt = driver
        .request_cancel(cancel_request("stop-first", TurnCancelMode::AfterStep))
        .await
        .expect("request the after-step stop");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    settle().await;
    assert!(
        !sleep.is_finished(),
        "the deferred stop leaves the timer parked"
    );
    assert!(!cancellation.is_cancelled());

    let receipt = driver
        .request_cancel(cancel_request("abort-second", TurnCancelMode::Immediate))
        .await
        .expect("escalate the deferred stop");
    assert!(
        matches!(receipt.outcome, TurnCancelOutcome::Escalated(_)),
        "{:?}",
        receipt.outcome
    );

    let error = tokio::time::timeout(Duration::from_secs(2), sleep)
        .await
        .expect("the escalation wakes the parked timer")
        .expect("join the sleep task")
        .expect_err("the escalation unwinds the sleep");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled
    );
    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn after_step_during_a_parked_await_event_keeps_waiting_for_the_event() {
    let context = Arc::new(RecordingContext::default());
    let awaited_key = restate_await_event_key(
        &durable_turn_scope(SESSION, TURN),
        AwaitEventWaitIdentity::Custom {
            key: "fig635-signal".to_string(),
        },
    )
    .expect("await-event key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let wait = {
        let context = Arc::clone(&context);
        let cancellation = cancellation.clone();
        let awaited_key = awaited_key.clone();
        tokio::spawn(async move {
            RestateRuntimeEffectController::new(context)
                .execute_effect(
                    RuntimeEffectEnvelope::new(
                        runtime_invocation(RuntimeEffectKind::AwaitEvent, "fig635-await-event"),
                        RuntimeEffectCommand::AwaitEvent { key: awaited_key },
                    ),
                    RuntimeEffectLocalExecutor::await_event(cancellation, None)
                        .with_turn_cancel_scope(durable_turn_scope(SESSION, TURN)),
                )
                .await
        })
    };
    await_gate_registration(&context.turn_cancel_gate).await;

    let receipt = driver_for(Arc::clone(&context))
        .request_cancel(cancel_request("stop-in-await", TurnCancelMode::AfterStep))
        .await
        .expect("request an after-step stop during the await-event");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    settle().await;
    assert!(
        !wait.is_finished(),
        "an after-step stop must not terminalize the await-event"
    );
    assert!(!cancellation.is_cancelled());

    let signal = Resolution::Ok(serde_json::json!({ "signal": "arrived" }));
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            key: awaited_key,
            resolution: signal.clone(),
        }),
        ResolveOutcome::Accepted
    );
    let outcome = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .expect("the event resolves the wait")
        .expect("join the await task")
        .expect("the await-event completes with the event's own resolution");
    assert!(
        matches!(&outcome, RuntimeEffectOutcome::AwaitEvent { resolution } if *resolution == signal),
        "{outcome:?}"
    );
    assert!(!cancellation.is_cancelled());
}

#[tokio::test]
async fn after_step_during_a_parked_process_await_lets_the_process_finish() {
    let context = Arc::new(RecordingContext::default());
    let registry = process_registry();
    let process_id = "fig635-awaited-process";
    registry
        .register_process(rerunnable_registration(process_id))
        .await
        .expect("register the awaited process");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let wait = {
        let context = Arc::clone(&context);
        let registry = Arc::clone(&registry);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            RestateRuntimeEffectController::new(context)
                .execute_effect(
                    RuntimeEffectEnvelope::new(
                        runtime_invocation(RuntimeEffectKind::Process, "fig635-process-await"),
                        RuntimeEffectCommand::process(ProcessCommand::Await {
                            process_ref: lash_core::ProcessRef::new(
                                process_id,
                                lash_core::ProcessIncarnation::from_registration_sequence(1),
                            ),
                        }),
                    ),
                    registry_local_executor(registry).with_process_turn_cancellation(
                        lash_core::facade_support::ProcessTurnCancellation::new(
                            cancellation,
                            durable_turn_scope(SESSION, TURN),
                        ),
                    ),
                )
                .await
        })
    };
    await_gate_registration(&context.turn_cancel_gate).await;

    let receipt = driver_for(Arc::clone(&context))
        .request_cancel(cancel_request("stop-in-process", TurnCancelMode::AfterStep))
        .await
        .expect("request an after-step stop during the process await");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    settle().await;
    assert!(
        !wait.is_finished(),
        "an after-step stop must not unwind the process await"
    );
    assert!(!cancellation.is_cancelled());
    assert!(
        context.cancelled.lock_recover().is_empty(),
        "an after-step stop never cancels the awaited process"
    );

    let terminal = process_success(serde_json::json!({ "finished": true }));
    context.resolve_process_terminal(process_id, &terminal);
    let outcome = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .expect("the process terminal resolves the wait")
        .expect("join the await task")
        .expect("the process await completes with the process's own terminal");
    let ProcessEffectOutcome::Await { output } = outcome.into_process().expect("process outcome")
    else {
        panic!("process await produced the wrong outcome");
    };
    assert_eq!(*output, terminal);
    assert!(!cancellation.is_cancelled());
    assert!(context.cancelled.lock_recover().is_empty());
}

// --- The full turn: a retry sleep parked on the gate composes to the boundary.

const RETRY_AFTER_MS: u64 = 4321;

#[derive(Clone, Default)]
struct RetryOnceTool {
    attempts: Arc<AtomicUsize>,
}

fn retry_once_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:fig635_retry_once",
        "fig635_retry_once",
        "Fails once with a safe retry, then succeeds.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_retry_policy(lash_core::ToolRetryPolicy::safe(
        2,
        RETRY_AFTER_MS,
        RETRY_AFTER_MS,
    ))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RetryOnceTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![retry_once_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "fig635_retry_once").then(|| Arc::new(retry_once_tool().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return lash_core::ToolOutcome::retryable_failure(
                lash_core::ToolFailureClass::External,
                "transient",
                "transient failure",
                Some(RETRY_AFTER_MS),
            );
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true }))
    }
}

fn cancelled_evidence(
    turn: &lash_core::facade_support::AssembledTurn,
) -> &lash_core::facade_support::TurnCancellationEvidence {
    match &turn.outcome {
        lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::Cancelled { evidence },
        ) => evidence,
        other => panic!("turn did not stop on cancellation: {other:?}"),
    }
}

#[tokio::test]
async fn after_step_during_a_parked_retry_sleep_finishes_the_iteration_and_stops_at_its_boundary() {
    let session_id = "fig635-retry-sleep-session";
    let turn_id = "fig635-retry-sleep-turn";
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let tool = RetryOnceTool::default();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    match provider_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => Ok(lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::ToolCall {
                                call_id: "retry-call-1".to_string(),
                                tool_name: "fig635_retry_once".to_string(),
                                input_json: serde_json::json!({}).to_string(),
                                replay: None,
                            }],
                            ..Default::default()
                        }),
                        _ => Ok(lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text: "finished after the retry".to_string(),
                                response_meta: None,
                            }],
                            ..Default::default()
                        }),
                    }
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    let tool_provider: Arc<dyn lash_core::ToolProvider> = Arc::new(tool.clone());
    let plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fig635-retry",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tool_provider),
        )),
    ];
    let policy = replay_test_policy(session_id);
    let state = replay_test_state(session_id, &policy);
    let dir = tempfile::tempdir().expect("fixture dir");
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open sqlite store"),
    );
    let mut runtime = replay_test_runtime_with_plugins_and_registry(
        session_id, policy, state, host, store, plugins, None,
    )
    .await;

    let turn = {
        let context = Arc::clone(&context);
        tokio::spawn(async move {
            let controller = RestateRuntimeEffectController::new(context);
            let scoped = controller
                .scoped_effect_controller(durable_turn_scope(session_id, turn_id))
                .expect("scoped restate controller");
            runtime
                .stream_turn(
                    replay_test_input(turn_id),
                    lash_core::facade_support::TurnOptions::new(
                        tokio_util::sync::CancellationToken::new(),
                        scoped,
                    ),
                )
                .await
        })
    };
    context.await_sleep_started().await;
    assert_eq!(tool.attempts.load(Ordering::SeqCst), 1);

    let receipt = driver_for(Arc::clone(&context))
        .request_cancel(
            TurnCancelRequest::new(TurnAddress::new(session_id, turn_id), "stop-in-retry", None)
                .mode(TurnCancelMode::AfterStep),
        )
        .await
        .expect("request an after-step stop during the retry sleep");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    settle().await;
    assert!(
        !turn.is_finished(),
        "the turn stays parked in its retry sleep"
    );
    assert_eq!(
        tool.attempts.load(Ordering::SeqCst),
        1,
        "an after-step stop does not wake the retry sleep early"
    );

    context.release_sleep();
    let turn = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the turn stops after the retried step")
        .expect("join the turn task")
        .expect("the turn assembles");
    let evidence = cancelled_evidence(&turn);
    assert_eq!(evidence.request_id, "stop-in-retry");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0));
    assert_eq!(
        tool.attempts.load(Ordering::SeqCst),
        2,
        "the retry runs after wake; the iteration finishes before the stop lands"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

// Drift pin: the wake the index journals is derived from the gate resolution
// the real driver writes. If lash-core renames the mode field or its
// encoding, the deferred branch silently degrades to an abort, and this is
// the test that says so.
#[tokio::test]
async fn gate_resolutions_carry_the_request_mode_into_the_wake() {
    for (mode, expected) in [
        (
            TurnCancelMode::AfterStep,
            RestateTurnCancelWake::TurnCancelDeferred,
        ),
        (
            TurnCancelMode::Immediate,
            RestateTurnCancelWake::TurnCancelled,
        ),
    ] {
        let context = Arc::new(RecordingContext::default());
        driver_for(Arc::clone(&context))
            .request_cancel(cancel_request("pin-wake-mode", mode))
            .await
            .expect("request a stop against an idle turn");
        let resolved = context.resolved_events.lock_recover();
        let gate = resolved
            .iter()
            .find(|request| request.key.wait == AwaitEventWaitIdentity::TurnCancelGate)
            .expect("the request resolves the turn-cancel gate");
        assert_eq!(
            RestateTurnCancelWake::for_gate_resolution(&gate.resolution),
            expected,
            "a {mode:?} request must journal a {expected:?} wake"
        );
    }
}

// Drift pin (prelude 21): `RestateTurnCancelWake` is one declaration feeding
// the index's journaled awakeable payload and the parked waiter's decode, so a
// one-character rename would stay self-consistent while every journal written
// before it silently stopped matching. Spell each wire literal by hand.
#[test]
fn turn_cancel_wake_wire_values_match_the_journaled_awakeable_encoding() {
    for (wake, literal) in [
        (RestateTurnCancelWake::TurnCancelled, "turn_cancelled"),
        (
            RestateTurnCancelWake::TurnCancelDeferred,
            "turn_cancel_deferred",
        ),
        (RestateTurnCancelWake::SessionRevoked, "session_revoked"),
    ] {
        let encoded = serde_json::to_value(wake).expect("serialize a turn-cancel wake");
        assert_eq!(
            encoded,
            serde_json::Value::String(literal.to_string()),
            "{wake:?} must journal the literal `{literal}`"
        );
        assert_eq!(
            serde_json::from_value::<RestateTurnCancelWake>(encoded)
                .expect("decode a journaled turn-cancel wake"),
            wake,
            "a journaled `{literal}` must decode back to {wake:?}"
        );
    }
}

fn deferred_wake_signal() -> serde_json::Value {
    serde_json::to_value(RestateTurnCancelWake::TurnCancelDeferred)
        .expect("serialize a deferred turn-cancel wake")
}

// Journal witness on the FIG-1631 sleep geometry: a deferred wake that lands
// while the timer is parked re-parks the gate on the escalation promise and
// keeps the timer. The handler suspends on both instead of reporting a
// cancelled sleep.
#[tokio::test]
async fn deferred_wake_during_a_parked_sleep_reparks_on_the_escalation_promise() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig635-sleep-gate-deferred-mid-sleep";
    let (_parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
        Some((17, deferred_wake_signal())),
    )
    .expect("splice a deferred wake that fires after the gate registered");
    let deferred = endpoint_protocol::invoke_endpoint_body_with_json_call_responses_then_suspend(
        &endpoint,
        "Fig1631SleepGate",
        "run",
        replay,
        vec![fig1631_registered_gate()],
    )
    .await
    .expect("a deferred wake must keep the parked sleep alive");

    assert_eq!(
        restate_call_frames(&deferred)
            .expect("decode the escalation registration")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable"],
        "the deferred wake registers on the escalation promise and nothing else"
    );
    assert_eq!(
        restate_message_types(&deferred).expect("decode deferred-wake frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ],
        "the timer stays journaled and the handler parks on it and the escalation"
    );
    assert_eq!(
        restate_output_json::<String>(&deferred),
        None,
        "a deferred wake must not settle the sleep"
    );
}

// Journal witness on the FIG-790 process-await geometry: a deferred wake never
// cancels the process. The handler registers the escalation gate and keeps
// awaiting the terminal.
#[tokio::test]
async fn deferred_wake_during_a_parked_process_await_never_cancels_the_process() {
    let process_id = "fig635-process-await-deferred";
    let pre_pr_call = fig790_pre_pr_suspended_process_call(process_id).await;
    let (endpoint, _registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let replay = encode_call_replay(
        "fig635-process-await-deferred",
        &input,
        &[(pre_pr_call, None)],
        Some((17, deferred_wake_signal())),
    )
    .expect("splice a deferred wake against a parked process await");
    let deferred = endpoint_protocol::invoke_endpoint_body_with_json_call_responses_then_suspend(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![fig1631_registered_gate(), fig1631_registered_gate()],
    )
    .await
    .expect("a deferred wake must keep the process await parked");

    assert_eq!(
        restate_call_frames(&deferred)
            .expect("decode the deferred-wake calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable", "register_awakeable"],
        "the gate registers, the deferred wake re-registers on escalation, and no cancel is sent"
    );
    assert_eq!(
        restate_message_types(&deferred)
            .expect("decode deferred-wake frames")
            .last()
            .copied(),
        Some(RESTATE_SUSPENSION_MESSAGE_TYPE),
        "the handler parks on the process terminal and the escalation promise"
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&deferred),
        None,
        "a deferred wake must not settle the process await"
    );
}
