//! Native witnesses for the two turn-cancel modes (FIG-635): `Immediate`
//! keeps today's cooperative abort; `AfterStep` lands at the progress
//! boundary that closes a protocol iteration.

use super::*;
use crate::{TurnCancelMode, TurnCancelOutcome, TurnCancelRequest, TurnCancellationEvidence};

/// A tool that reports whether it observed the cooperative token, and can
/// either hold until released or wait for the token itself.
#[derive(Clone, Default)]
struct TokenWatchingTool {
    executions: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    released: Arc<AtomicBool>,
    observed_cancelled: Arc<AtomicBool>,
    wait_for_token: bool,
}

impl TokenWatchingTool {
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
        self.release.notify_one();
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for TokenWatchingTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        EchoTool.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        EchoTool.resolve_contract(name)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        let token = call
            .context
            .cancellation_token()
            .cloned()
            .expect("attempt context carries the cooperative token");
        if self.wait_for_token {
            token.cancelled().await;
        } else {
            while !self.released.load(Ordering::SeqCst) {
                let released = self.release.notified();
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                released.await;
            }
        }
        self.observed_cancelled
            .store(token.is_cancelled(), Ordering::SeqCst);
        EchoTool.execute(call).await
    }
}

fn tool_call_response(call_id: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: call_id.to_string(),
            tool_name: "echo_tool".to_string(),
            input_json: serde_json::json!({"value": call_id}).to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

/// Provider: call 0 signals `started`, holds until `release`, then answers
/// with one tool call; call 1 answers with text. Counts calls.
fn gated_tool_calling_provider(
    provider_calls: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    released: Arc<AtomicBool>,
) -> TestProvider {
    TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let provider_calls = Arc::clone(&provider_calls);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let released = Arc::clone(&released);
            async move {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                match call {
                    0 => {
                        started.notify_one();
                        while !released.load(Ordering::SeqCst) {
                            let notified = release.notified();
                            if released.load(Ordering::SeqCst) {
                                break;
                            }
                            notified.await;
                        }
                        Ok(tool_call_response("step-0-call"))
                    }
                    _ => Ok(text_response("finished after the stop")),
                }
            }
        })
        .build()
}

struct ModeHarness {
    runtime: LashRuntime,
    driver: crate::TurnWorkDriver,
}

async fn native_harness(
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
) -> ModeHarness {
    let config = super::effect::runtime_host_config_with_native_controller(Arc::new(
        crate::NativeRuntimeEffectController::default(),
    ));
    let driver_store: Arc<dyn crate::RuntimePersistence> = Arc::new(RecordingStore::default());
    crate::testing::store_fixtures::bind_conformance_session(
        &driver_store,
        &crate::SessionId::from("root"),
    )
    .await;
    let driver = crate::TurnWorkDriver::for_session(
        Arc::clone(&config.control.effect_host),
        "root",
        driver_store,
    );
    let host = EmbeddedRuntimeHost::new(config);
    let runtime = runtime_with_plugins_and_tools_and_host(Vec::new(), tools, transport, host).await;
    ModeHarness { runtime, driver }
}

fn request(turn_id: &TurnId, request_id: &str, mode: TurnCancelMode) -> TurnCancelRequest {
    TurnCancelRequest::new(
        crate::TurnAddress::new("root", turn_id),
        request_id,
        Some("test-user".to_string()),
    )
    .with_reason("mode witness")
    .mode(mode)
}

fn cancelled_evidence(turn: &AssembledTurn) -> TurnCancellationEvidence {
    match &turn.outcome {
        TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) => evidence.clone(),
        other => panic!("expected a cancelled outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn after_step_stop_mid_model_call_waits_for_the_response_and_its_tools() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(AtomicBool::new(false));
    let tool = TokenWatchingTool::default();
    tool.release();
    let transport = gated_tool_calling_provider(
        Arc::clone(&provider_calls),
        Arc::clone(&started),
        Arc::clone(&release),
        Arc::clone(&released),
    );
    let ModeHarness {
        mut runtime,
        driver,
    } = native_harness(Arc::new(tool.clone()), transport).await;
    let turn_id = "after-step-mid-model";
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("stop after this step"),
                TurnOptions::new(
                    CancellationToken::new(),
                    named_turn_scope(&SessionId::from("root"), &TurnId::from(turn_id)),
                ),
            )
            .await
    });
    started.notified().await;
    let receipt = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "stop-1",
            TurnCancelMode::AfterStep,
        ))
        .await
        .expect("request after-step stop");
    assert!(matches!(
        receipt.outcome,
        TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "stop-1" && evidence.mode == TurnCancelMode::AfterStep
    ));
    released.store(true, Ordering::SeqCst);
    release.notify_one();

    let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("turn stops at its step boundary")
        .expect("turn task")
        .expect("turn assembles");
    let evidence = cancelled_evidence(&turn);
    assert_eq!(evidence.request_id, "stop-1");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0));
    assert_eq!(evidence.origin.as_deref(), Some("test-user"));
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "the response that was streaming finishes; no further model call starts"
    );
    assert_eq!(
        tool.executions.load(Ordering::SeqCst),
        1,
        "the tool call of the closing step runs to completion"
    );
    assert!(
        !tool.observed_cancelled.load(Ordering::SeqCst),
        "an after-step stop never fires the cooperative token"
    );
    assert_eq!(
        turn.tool_calls.len(),
        1,
        "the completed tool result is part of the stopped turn, not backtracked"
    );
}

#[tokio::test]
async fn after_step_stop_mid_tool_call_lets_the_tool_finish_uncancelled() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(AtomicBool::new(true));
    let tool = TokenWatchingTool::default();
    let transport =
        gated_tool_calling_provider(Arc::clone(&provider_calls), started, release, released);
    let ModeHarness {
        mut runtime,
        driver,
    } = native_harness(Arc::new(tool.clone()), transport).await;
    let turn_id = "after-step-mid-tool";
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("stop after this step"),
                TurnOptions::new(
                    CancellationToken::new(),
                    named_turn_scope(&SessionId::from("root"), &TurnId::from(turn_id)),
                ),
            )
            .await
    });
    tool.entered.notified().await;
    let receipt = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "stop-mid-tool",
            TurnCancelMode::AfterStep,
        ))
        .await
        .expect("request after-step stop");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !tool.observed_cancelled.load(Ordering::SeqCst),
        "the tool is still running and the token stays quiet"
    );
    tool.release();

    let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("turn stops at its step boundary")
        .expect("turn task")
        .expect("turn assembles");
    let evidence = cancelled_evidence(&turn);
    assert_eq!(evidence.request_id, "stop-mid-tool");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0));
    assert_eq!(tool.executions.load(Ordering::SeqCst), 1);
    assert!(!tool.observed_cancelled.load(Ordering::SeqCst));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn immediate_after_after_step_escalates_and_aborts_the_running_tool() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let tool = TokenWatchingTool {
        wait_for_token: true,
        ..TokenWatchingTool::default()
    };
    let transport = gated_tool_calling_provider(
        Arc::clone(&provider_calls),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(AtomicBool::new(true)),
    );
    let ModeHarness {
        mut runtime,
        driver,
    } = native_harness(Arc::new(tool.clone()), transport).await;
    let turn_id = "escalate-to-abort";
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("stop, then abort"),
                TurnOptions::new(
                    CancellationToken::new(),
                    named_turn_scope(&SessionId::from("root"), &TurnId::from(turn_id)),
                ),
            )
            .await
    });
    tool.entered.notified().await;
    let stop = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "stop-first",
            TurnCancelMode::AfterStep,
        ))
        .await
        .expect("request after-step stop");
    assert!(matches!(stop.outcome, TurnCancelOutcome::Requested(_)));
    let weaker_again = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "stop-again",
            TurnCancelMode::AfterStep,
        ))
        .await
        .expect("repeat after-step stop");
    assert!(matches!(
        weaker_again.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence) if evidence.request_id == "stop-first"
    ));
    let abort = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "abort-now",
            TurnCancelMode::Immediate,
        ))
        .await
        .expect("escalate to abort");
    assert!(matches!(
        abort.outcome,
        TurnCancelOutcome::Escalated(ref evidence)
            if evidence.request_id == "abort-now" && evidence.mode == TurnCancelMode::Immediate
    ));

    let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("escalated abort unwinds the tool")
        .expect("turn task")
        .expect("turn assembles");
    let evidence = cancelled_evidence(&turn);
    assert_eq!(evidence.request_id, "abort-now");
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    assert_eq!(evidence.honoured_after_step, None);
    assert!(
        tool.observed_cancelled.load(Ordering::SeqCst),
        "the escalated abort fires the cooperative token the tool was waiting on"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    let repeat = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "abort-late",
            TurnCancelMode::Immediate,
        ))
        .await
        .expect("late abort");
    assert!(
        !matches!(repeat.outcome, TurnCancelOutcome::Requested(_)),
        "a finished turn never accepts a fresh request: {:?}",
        repeat.outcome
    );
}

#[tokio::test]
async fn start_gate_refuses_the_next_turn_for_both_modes() {
    for mode in [TurnCancelMode::Immediate, TurnCancelMode::AfterStep] {
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let tool = TokenWatchingTool::default();
        tool.release();
        let transport = gated_tool_calling_provider(
            Arc::clone(&provider_calls),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(AtomicBool::new(true)),
        );
        let ModeHarness {
            mut runtime,
            driver,
        } = native_harness(Arc::new(tool.clone()), transport).await;
        let turn_id = "refused-before-start";
        let receipt = driver
            .request_cancel(request(&TurnId::from(turn_id), "before-start", mode))
            .await
            .expect("request before the turn starts");
        assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
        let turn = runtime
            .stream_turn(
                TurnInput::text("never runs"),
                TurnOptions::new(
                    CancellationToken::new(),
                    named_turn_scope(&SessionId::from("root"), &TurnId::from(turn_id)),
                ),
            )
            .await
            .expect("refused turn assembles");
        let evidence = cancelled_evidence(&turn);
        assert_eq!(evidence.request_id, "before-start");
        assert_eq!(evidence.mode, mode, "the start gate honours either mode");
        assert_eq!(evidence.honoured_after_step, None);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 0, "{mode:?}");
        assert_eq!(tool.executions.load(Ordering::SeqCst), 0, "{mode:?}");
    }
}

#[tokio::test]
async fn undelivered_disposition_matrix_applies_for_both_modes() {
    for mode in [TurnCancelMode::Immediate, TurnCancelMode::AfterStep] {
        for disposition in [
            crate::TurnCancelDisposition::Defer,
            crate::TurnCancelDisposition::Drop,
        ] {
            let transport = mock_provider(Vec::new());
            let (mut runtime, store) =
                standard_runtime_with_transport_and_queue_store(transport).await;
            let persisted = runtime.export_persistence_state();
            let session_id = persisted.session_id.clone();
            let driver = crate::TurnWorkDriver::for_session(
                Arc::clone(&runtime.host.core.control.effect_host),
                session_id.clone(),
                Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
            );
            let turn_id = format!("matrix-{mode:?}-{disposition:?}").to_ascii_lowercase();
            let undelivered = crate::store::TurnInputStore::enqueue_pending_turn_input(
                store.as_ref(),
                crate::PendingTurnInputDraft::new(
                    &session_id,
                    crate::TurnInputIngress::active_turn(
                        &turn_id,
                        crate::TurnInputCheckpointBoundary::AfterWork,
                    ),
                    crate::TurnInput::text("unsent steer"),
                ),
            )
            .await
            .expect("enqueue active-turn input");
            let receipt = driver
                .request_cancel(
                    TurnCancelRequest::new(
                        crate::TurnAddress::new(&session_id, &turn_id),
                        format!("{turn_id}:request"),
                        Some("test-user".to_string()),
                    )
                    .undelivered(disposition)
                    .mode(mode),
                )
                .await
                .expect("request cancellation");
            assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
            let turn = runtime
                .run_turn_assembled(
                    TurnInput::text("refused"),
                    CancellationToken::new(),
                    native_scope(persisted.turn_scope(&turn_id)),
                )
                .await
                .expect("refused turn assembles");
            let evidence = cancelled_evidence(&turn);
            assert_eq!(evidence.mode, mode);
            assert_eq!(evidence.undelivered, disposition);
            let affected: Vec<_> = turn
                .turn_cancel_input_outcome
                .affected_inputs
                .iter()
                .map(|input| (input.input_id.clone(), input.disposition))
                .collect();
            assert_eq!(
                affected,
                vec![(undelivered.input_id.clone(), disposition)],
                "{mode:?}/{disposition:?}: the disposition applies to the undelivered active-turn input"
            );
            let pending: Vec<_> =
                crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), &session_id)
                    .await
                    .expect("pending inputs")
                    .into_iter()
                    .map(|input| input.input_id)
                    .collect();
            let expected = match disposition {
                crate::TurnCancelDisposition::Defer => vec![undelivered.input_id.clone()],
                crate::TurnCancelDisposition::Drop => Vec::new(),
            };
            assert_eq!(
                pending, expected,
                "{mode:?}/{disposition:?}: Defer keeps the row queued, Drop removes it"
            );
        }
    }
}

#[tokio::test]
async fn a_stop_in_either_mode_never_drains_next_turn_work_queued_behind_it() {
    for mode in [TurnCancelMode::AfterStep, TurnCancelMode::Immediate] {
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let tool = TokenWatchingTool {
            wait_for_token: mode.is_immediate(),
            ..TokenWatchingTool::default()
        };
        let transport = gated_tool_calling_provider(
            Arc::clone(&provider_calls),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(AtomicBool::new(true)),
        );
        let store = Arc::new(RecordingStore::default());
        let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
        let config = super::effect::runtime_host_config_with_native_controller(Arc::new(
            crate::NativeRuntimeEffectController::default(),
        ));
        let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
            Vec::new(),
            Arc::new(tool.clone()),
            transport,
            EmbeddedRuntimeHost::new(config),
            runtime_store,
        )
        .await;
        let persisted = runtime.export_persistence_state();
        let session_id = persisted.session_id.clone();
        let driver = crate::TurnWorkDriver::for_session(
            Arc::clone(&runtime.host.core.control.effect_host),
            session_id.clone(),
            Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        );
        let turn_id = format!("no-drain-{mode:?}").to_ascii_lowercase();
        let turn_scope = native_scope(persisted.turn_scope(&turn_id));
        let turn = crate::task::spawn(async move {
            runtime
                .run_turn_assembled(
                    TurnInput::text("stop while queued work waits"),
                    CancellationToken::new(),
                    turn_scope,
                )
                .await
        });
        tool.entered.notified().await;
        let queued = enqueue_idle_turn_input(store.as_ref(), &session_id, "queued behind").await;
        let receipt = driver
            .request_cancel(
                TurnCancelRequest::new(
                    crate::TurnAddress::new(&session_id, &turn_id),
                    format!("{turn_id}:request"),
                    Some("test-user".to_string()),
                )
                .mode(mode),
            )
            .await
            .expect("request cancellation");
        assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
        tool.release();
        let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
            .await
            .expect("turn stops")
            .expect("turn task")
            .expect("turn assembles");
        let evidence = cancelled_evidence(&turn);
        assert_eq!(evidence.mode, mode);
        assert_eq!(
            evidence.honoured_after_step,
            (!mode.is_immediate()).then_some(0)
        );
        assert_eq!(
            tool.observed_cancelled.load(Ordering::SeqCst),
            mode.is_immediate(),
            "{mode:?}: only an immediate abort reaches the tool"
        );
        let pending: Vec<_> =
            crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), &session_id)
                .await
                .expect("pending inputs")
                .into_iter()
                .map(|input| input.input_id)
                .collect();
        assert_eq!(
            pending,
            vec![queued.input_id.clone()],
            "{mode:?}: a stop never drains the next-turn work queued behind it"
        );
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1, "{mode:?}");
    }
}

/// A clock whose retry-backoff sleep holds until the test releases it, so a
/// cancel request can land while the turn is inside a durable sleep.
#[derive(Debug)]
struct HeldRetrySleepClock {
    inner: crate::testing::TestClock,
    held_ms: u64,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    released: AtomicBool,
    held_sleeps: AtomicUsize,
}

impl HeldRetrySleepClock {
    fn new(held_ms: u64) -> Self {
        Self {
            inner: crate::testing::TestClock::new(0),
            held_ms,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            released: AtomicBool::new(false),
            held_sleeps: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl crate::Clock for HeldRetrySleepClock {
    fn now(&self) -> std::time::Instant {
        self.inner.now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        self.inner.timestamp_datetime()
    }

    async fn sleep(&self, duration: std::time::Duration) {
        if duration.as_millis() as u64 != self.held_ms {
            tokio::task::yield_now().await;
            return;
        }
        self.held_sleeps.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        while !self.released.load(Ordering::SeqCst) {
            let released = self.release.notified();
            if self.released.load(Ordering::SeqCst) {
                break;
            }
            released.await;
        }
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.inner.sleep_until(deadline).await;
    }
}

const RETRY_AFTER_MS: u64 = 1234;

#[derive(Clone, Default)]
struct RetryOnceTool {
    attempts: Arc<AtomicUsize>,
}

fn retry_once_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:retry_once",
        "retry_once",
        "Fails once with a safe retry.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_retry_policy(crate::ToolRetryPolicy::safe(
        2,
        RETRY_AFTER_MS,
        RETRY_AFTER_MS,
    ))
}

#[async_trait::async_trait]
impl crate::ToolProvider for RetryOnceTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![retry_once_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "retry_once").then(|| Arc::new(retry_once_tool_definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return crate::ToolOutcome::retryable_failure(
                crate::ToolFailureClass::External,
                "transient",
                "transient failure",
                Some(RETRY_AFTER_MS),
            );
        }
        crate::ToolOutcome::ok(serde_json::json!({ "ok": true }))
    }
}

fn retry_tool_provider(provider_calls: Arc<AtomicUsize>) -> TestProvider {
    TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let provider_calls = Arc::clone(&provider_calls);
            async move {
                match provider_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "retry-call-1".to_string(),
                            tool_name: "retry_once".to_string(),
                            input_json: serde_json::json!({}).to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    _ => Ok(text_response("finished after the retry")),
                }
            }
        })
        .build()
}

async fn sleeping_retry_harness(
    clock: Arc<HeldRetrySleepClock>,
    tool: RetryOnceTool,
    provider_calls: Arc<AtomicUsize>,
) -> ModeHarness {
    let host_clock: Arc<dyn crate::Clock> = clock;
    let config = super::effect::runtime_host_config_with_native_controller(Arc::new(
        crate::NativeRuntimeEffectController::default(),
    ))
    .with_clock(host_clock);
    let driver_store: Arc<dyn crate::RuntimePersistence> = Arc::new(RecordingStore::default());
    crate::testing::store_fixtures::bind_conformance_session(
        &driver_store,
        &crate::SessionId::from("root"),
    )
    .await;
    let driver = crate::TurnWorkDriver::for_session(
        Arc::clone(&config.control.effect_host),
        "root",
        driver_store,
    );
    let host = EmbeddedRuntimeHost::new(config);
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(tool),
        retry_tool_provider(provider_calls),
        host,
    )
    .await;
    ModeHarness { runtime, driver }
}

#[tokio::test]
async fn after_step_stop_during_retry_sleep_lands_at_wake_and_stops_at_the_boundary() {
    let clock = Arc::new(HeldRetrySleepClock::new(RETRY_AFTER_MS));
    let tool = RetryOnceTool::default();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let ModeHarness {
        mut runtime,
        driver,
    } = sleeping_retry_harness(
        Arc::clone(&clock),
        tool.clone(),
        Arc::clone(&provider_calls),
    )
    .await;
    let turn_id = "after-step-during-sleep";
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("retry then stop"),
                TurnOptions::new(
                    CancellationToken::new(),
                    named_turn_scope(&SessionId::from("root"), &TurnId::from(turn_id)),
                ),
            )
            .await
    });
    clock.entered.notified().await;
    assert_eq!(tool.attempts.load(Ordering::SeqCst), 1);
    let receipt = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "stop-in-sleep",
            TurnCancelMode::AfterStep,
        ))
        .await
        .expect("request during the sleep");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        tool.attempts.load(Ordering::SeqCst),
        1,
        "an after-step stop does not wake the sleep early"
    );
    clock.released.store(true, Ordering::SeqCst);
    clock.release.notify_one();

    let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("turn stops after the retried step")
        .expect("turn task")
        .expect("turn assembles");
    let evidence = cancelled_evidence(&turn);
    assert_eq!(evidence.request_id, "stop-in-sleep");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0));
    assert_eq!(
        tool.attempts.load(Ordering::SeqCst),
        2,
        "the retry runs after wake; the iteration finishes before the stop lands"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(clock.held_sleeps.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn immediate_abort_during_retry_sleep_unwinds_without_the_retry() {
    let clock = Arc::new(HeldRetrySleepClock::new(RETRY_AFTER_MS));
    let tool = RetryOnceTool::default();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let ModeHarness {
        mut runtime,
        driver,
    } = sleeping_retry_harness(
        Arc::clone(&clock),
        tool.clone(),
        Arc::clone(&provider_calls),
    )
    .await;
    let turn_id = "abort-during-sleep";
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("retry then abort"),
                TurnOptions::new(
                    CancellationToken::new(),
                    named_turn_scope(&SessionId::from("root"), &TurnId::from(turn_id)),
                ),
            )
            .await
    });
    clock.entered.notified().await;
    let receipt = driver
        .request_cancel(request(
            &TurnId::from(turn_id),
            "abort-in-sleep",
            TurnCancelMode::Immediate,
        ))
        .await
        .expect("request during the sleep");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));

    let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("immediate abort unwinds the held sleep")
        .expect("turn task")
        .expect("turn assembles");
    let evidence = cancelled_evidence(&turn);
    assert_eq!(evidence.request_id, "abort-in-sleep");
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    assert_eq!(evidence.honoured_after_step, None);
    assert_eq!(
        tool.attempts.load(Ordering::SeqCst),
        1,
        "the cooperative token cuts the sleep short; no retry runs"
    );
    assert!(
        !clock.released.load(Ordering::SeqCst),
        "the sleep was never released by the test"
    );
}
