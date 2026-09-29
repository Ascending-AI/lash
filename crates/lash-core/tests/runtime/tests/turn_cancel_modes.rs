//! Native witnesses for the two turn-cancel modes (FIG-635): `Immediate`
//! keeps today's cooperative abort; `AfterStep` lands at the progress
//! boundary that closes a protocol iteration.

use super::*;
use lash_core::TurnCancelMode;
use lash_core::facade_support::{TurnCancelOutcome, TurnCancelRequest, TurnCancellationEvidence};
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x5_c100;

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
impl lash_core::ToolProvider for TokenWatchingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        EchoTool.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        EchoTool.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
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

/// Provider: call 0 signals `started`, holds until `release`, then answers with one tool call;
/// call 1 answers with text.
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
    driver: lash_core::facade_support::TurnWorkDriver,
}

async fn native_harness(
    double: &lash_restate_test::RestateTestBackend,
    tools: Arc<dyn lash_core::ToolProvider>,
    transport: TestProvider,
) -> ModeHarness {
    let backend = double.lash_backend();
    let config = test_runtime_host_config(&backend);
    let driver_store = double_unbound_store(double).await;
    lash_core::testing::store_fixtures::bind_conformance_session(
        &driver_store,
        &lash_core::SessionId::from("root"),
    )
    .await;
    let driver = lash_core::facade_support::TurnWorkDriver::for_session(
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
        lash_core::facade_support::TurnAddress::new("root", turn_id),
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

#[derive(Clone, Default)]
struct OrderedStopSink(Arc<Mutex<Vec<&'static str>>>);

#[async_trait::async_trait]
impl lash_core::runtime::EventSink for OrderedStopSink {
    async fn emit(&self, event: SessionStreamEvent) {
        if matches!(
            event,
            SessionStreamEvent::TurnOutcome {
                outcome: TurnOutcome::Stopped(_)
            }
        ) {
            self.0.lock_recover().push("stopped");
        }
    }
}

#[async_trait::async_trait]
impl lash_core::runtime::TurnActivitySink for OrderedStopSink {
    async fn emit(&self, activity: TurnActivity) {
        if matches!(activity.event, TurnEvent::CheckpointRecorded { .. }) {
            self.0.lock_recover().push("checkpoint");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn after_step_stop_mid_model_call_waits_for_the_response_and_its_tools() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
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
    } = Box::pin(native_harness(&double, Arc::new(tool.clone()), transport)).await;
    let turn_id = "after-step-mid-model";
    let observed = OrderedStopSink::default();
    let turn = lash_core::task::spawn({
        let double = double.clone();
        let observed = observed.clone();
        async move {
            let handler = double
                .open_handler(AdmittedScope::turn(
                    SessionId::from("root"),
                    TurnId::from(turn_id),
                ))
                .await
                .expect("open the scope's handler");
            let assembled = runtime
                .drive_turn(
                    TurnInput::text("stop after this step"),
                    TurnOptions::new(CancellationToken::new(), handler.scoped())
                        .with_events(&observed)
                        .with_turn_events(&observed),
                )
                .await;
            handler.close().await.expect("close the scope's handler");
            assembled
        }
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
        *observed.0.lock_recover(),
        ["checkpoint", "stopped"],
        "the accepted checkpoint reaches the host before the after-step stop"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn after_step_stop_mid_tool_call_lets_the_tool_finish_uncancelled() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
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
    } = Box::pin(native_harness(&double, Arc::new(tool.clone()), transport)).await;
    let turn_id = "after-step-mid-tool";
    let turn = lash_core::task::spawn({
        let double = double.clone();
        async move {
            let handler = double
                .open_handler(AdmittedScope::turn(
                    SessionId::from("root"),
                    TurnId::from(turn_id),
                ))
                .await
                .expect("open the scope's handler");
            let assembled = runtime
                .drive_turn(
                    TurnInput::text("stop after this step"),
                    TurnOptions::new(CancellationToken::new(), handler.scoped()),
                )
                .await;
            handler.close().await.expect("close the scope's handler");
            assembled
        }
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

#[tokio::test(flavor = "multi_thread")]
async fn immediate_stop_tail_after_checkpoint_is_absent_from_next_turn_context() {
    const CHECKPOINTED: &str = "committed before the tool";
    const RETRACTED: &str = "discarded transient attempt";
    const TAIL: &str = "uncommitted streamed tail";
    let double = kernel_double(SEED + 20, lash_restate_test::ServerConfig::default()).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let transport = TestProvider::builder()
        .kind("checkpoint-tail")
        .requires_streaming(true)
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete({
            let calls = Arc::clone(&calls);
            let requests = Arc::clone(&requests);
            move |request| {
                requests.lock_recover().push(request.messages.clone());
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    match call {
                        0 => Ok(LlmResponse {
                            parts: vec![
                                LlmOutputPart::Text {
                                    text: CHECKPOINTED.to_string(),
                                    response_meta: None,
                                },
                                LlmOutputPart::ToolCall {
                                    call_id: "checkpoint-tool".to_string(),
                                    tool_name: "echo_tool".to_string(),
                                    input_json: serde_json::json!({"value": "done"}).to_string(),
                                    replay: None,
                                },
                            ],
                            ..LlmResponse::default()
                        }),
                        1 => {
                            request
                                .stream_events
                                .expect("stream sender")
                                .send(LlmStreamEvent::Delta {
                                    block: lash_core::llm::types::StreamBlockIdentity::new(
                                        "failed-block", 0,
                                    ),
                                    text: RETRACTED.to_string(),
                                });
                            Err(LlmTransportError::new("transient provider failure")
                                .with_kind(lash_core::ProviderFailureKind::Stream)
                                .with_retry_verdict(
                                    lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                                ))
                        }
                        2 => {
                            let stream = request.stream_events.expect("stream sender");
                            stream.send(LlmStreamEvent::Delta {
                                block: lash_core::llm::types::StreamBlockIdentity::new(
                                    "tail-block", 0,
                                ),
                                text: TAIL.to_string(),
                            });
                            std::future::pending().await
                        }
                        3 => Ok(text_response("next turn answered")),
                        other => panic!("unexpected model call {other}"),
                    }
                }
            }
        })
        .build();
    let ModeHarness {
        mut runtime,
        driver,
    } = Box::pin(native_harness(&double, Arc::new(EchoTool), transport)).await;
    let activities = RecordingTurnEvents::default();
    let first_turn = lash_core::task::spawn({
        let double = double.clone();
        let activities = activities.clone();
        async move {
            let handler = double
                .open_handler(AdmittedScope::turn(
                    SessionId::from("root"),
                    TurnId::from("checkpoint-tail-first"),
                ))
                .await
                .expect("open first turn");
            let turn = runtime
                .drive_turn(
                    TurnInput::text("first turn"),
                    TurnOptions::new(CancellationToken::new(), handler.scoped())
                        .with_turn_events(&activities),
                )
                .await
                .expect("stopped turn assembles");
            handler.close().await.expect("close first turn");
            (runtime, turn)
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if activities.snapshot().iter().any(|activity| {
                matches!(&activity.event, TurnEvent::AssistantProseDelta { text, .. } if &**text == TAIL)
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("host sees streamed tail");
    let receipt = driver
        .request_cancel(request(
            &TurnId::from("checkpoint-tail-first"),
            "stop-after-checkpoint",
            TurnCancelMode::Immediate,
        ))
        .await
        .expect("request immediate stop");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    let (mut runtime, first) = first_turn.await.expect("first turn task");
    assert_eq!(cancelled_evidence(&first).mode, TurnCancelMode::Immediate);

    let host = activities.snapshot();
    let checkpoint = host
        .iter()
        .rposition(|activity| matches!(activity.event, TurnEvent::CheckpointRecorded { .. }))
        .expect("first iteration records a checkpoint");
    assert!(host[..checkpoint].iter().any(|activity| {
        matches!(&activity.event, TurnEvent::AssistantProseDelta { text, .. } if &**text == CHECKPOINTED)
    }));
    let retracted = host[checkpoint + 1..]
        .iter()
        .find(|activity| {
            matches!(&activity.event, TurnEvent::AssistantProseDelta { text, .. } if &**text == RETRACTED)
        })
        .expect("failed attempt delta reached the host");
    let reset = host[checkpoint + 1..]
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                ..
            } => Some(assistant_prose_correlation_ids),
            _ => None,
        })
        .expect("retry retracts the failed attempt");
    assert!(reset.contains(&retracted.correlation_id));
    let tail: String = host[checkpoint + 1..]
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text, .. }
                if !reset.contains(&activity.correlation_id) =>
            {
                Some(text.as_ref())
            }
            _ => None,
        })
        .collect();
    assert_eq!(tail, TAIL);

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("checkpoint-tail-next"),
        ))
        .await
        .expect("open next turn");
    let next = runtime
        .drive_turn(
            TurnInput::text("next turn"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("next turn assembles");
    handler.close().await.expect("close next turn");
    assert!(matches!(next.outcome, TurnOutcome::Finished(_)));
    let requests = requests.lock_recover();
    let next_request = requests.get(3).expect("next turn reaches the model");
    let contains = |text: &str| {
        next_request.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, lash_core::llm::types::LlmContentBlock::Text { text: value, .. } if value.contains(text))
            })
        })
    };
    assert!(
        contains(CHECKPOINTED),
        "checkpointed prose is in the next prompt"
    );
    assert!(
        !contains(TAIL),
        "the streamed tail is absent from the next prompt"
    );
    assert!(
        !contains(RETRACTED),
        "the retracted attempt is absent from the next prompt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn immediate_after_after_step_escalates_and_aborts_the_running_tool() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
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
    } = Box::pin(native_harness(&double, Arc::new(tool.clone()), transport)).await;
    let turn_id = "escalate-to-abort";
    let turn = lash_core::task::spawn({
        let double = double.clone();
        async move {
            let handler = double
                .open_handler(AdmittedScope::turn(
                    SessionId::from("root"),
                    TurnId::from(turn_id),
                ))
                .await
                .expect("open the scope's handler");
            let assembled = runtime
                .drive_turn(
                    TurnInput::text("stop, then abort"),
                    TurnOptions::new(CancellationToken::new(), handler.scoped()),
                )
                .await;
            handler.close().await.expect("close the scope's handler");
            assembled
        }
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
    // On the double the escalated abort unwinds the in-handler turn task,
    // dropping the tool future before its watch can observe the cooperative
    // token; the recorded `Stopped(Cancelled)` evidence above carries the
    // escalation.
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

#[tokio::test(flavor = "multi_thread")]
async fn start_gate_refuses_the_next_turn_for_both_modes() {
    for mode in [TurnCancelMode::Immediate, TurnCancelMode::AfterStep] {
        let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
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
        } = Box::pin(native_harness(&double, Arc::new(tool.clone()), transport)).await;
        let turn_id = "refused-before-start";
        let receipt = driver
            .request_cancel(request(&TurnId::from(turn_id), "before-start", mode))
            .await
            .expect("request before the turn starts");
        assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
        let handler = double
            .open_handler(AdmittedScope::turn(
                SessionId::from("root").clone(),
                TurnId::from(turn_id).clone(),
            ))
            .await
            .expect("open the scope's handler");
        let turn = runtime
            .drive_turn(
                TurnInput::text("never runs"),
                TurnOptions::new(CancellationToken::new(), handler.scoped()),
            )
            .await
            .expect("refused turn assembles");
        handler.close().await.expect("close the scope's handler");
        let evidence = cancelled_evidence(&turn);
        assert_eq!(evidence.request_id, "before-start");
        assert_eq!(evidence.mode, mode, "the start gate honours either mode");
        assert_eq!(evidence.honoured_after_step, None);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 0, "{mode:?}");
        assert_eq!(tool.executions.load(Ordering::SeqCst), 0, "{mode:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn undelivered_disposition_matrix_applies_for_both_modes() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    for mode in [TurnCancelMode::Immediate, TurnCancelMode::AfterStep] {
        for disposition in [
            lash_core::TurnCancelDisposition::Defer,
            lash_core::TurnCancelDisposition::Drop,
        ] {
            let transport = mock_provider(Vec::new());
            let session_id = SessionId::from(
                format!("cancel-matrix-{mode:?}-{disposition:?}").to_ascii_lowercase(),
            );
            let (mut runtime, store) =
                standard_runtime_with_transport_and_double_queue_store_for_session(
                    &double,
                    transport,
                    &session_id,
                )
                .await;
            let persisted = runtime.export_persistence_state();
            let session_id = persisted.session_id.clone();
            let driver = lash_core::facade_support::TurnWorkDriver::for_session(
                Arc::clone(&runtime.host.core.control.effect_host),
                session_id.clone(),
                Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
            );
            let turn_id = format!("matrix-{mode:?}-{disposition:?}").to_ascii_lowercase();
            let undelivered = lash_core::store::IngressStore::enqueue_pending_turn_input(
                store.as_ref(),
                lash_core::PendingTurnInputDraft::new(
                    &session_id,
                    lash_core::TurnInputIngress::active_turn(
                        &turn_id,
                        lash_core::TurnInputCheckpointBoundary::AfterWork,
                    ),
                    lash_core::TurnInput::text("unsent steer"),
                ),
            )
            .await
            .expect("enqueue active-turn input");
            let receipt = driver
                .request_cancel(
                    TurnCancelRequest::new(
                        lash_core::facade_support::TurnAddress::new(&session_id, &turn_id),
                        format!("{turn_id}:request"),
                        Some("test-user".to_string()),
                    )
                    .undelivered(disposition)
                    .mode(mode),
                )
                .await
                .expect("request cancellation");
            assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
            let handler = double
                .open_handler(lash_core::AdmittedScope::new(
                    persisted.turn_scope(&turn_id),
                ))
                .await
                .expect("open the scope's handler");
            let turn = runtime
                .drive_turn(
                    TurnInput::text("refused"),
                    lash_core::facade_support::TurnOptions::new(
                        CancellationToken::new(),
                        handler.scoped(),
                    ),
                )
                .await
                .expect("refused turn assembles");
            handler.close().await.expect("close the scope's handler");
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
            let pending: Vec<_> = lash_core::store::IngressStore::list_pending_turn_inputs(
                store.as_ref(),
                &session_id,
            )
            .await
            .expect("pending inputs")
            .into_iter()
            .map(|input| input.input.input_id)
            .collect();
            let expected = match disposition {
                lash_core::TurnCancelDisposition::Defer => vec![undelivered.input_id.clone()],
                lash_core::TurnCancelDisposition::Drop => Vec::new(),
            };
            assert_eq!(
                pending, expected,
                "{mode:?}/{disposition:?}: Defer keeps the row queued, Drop removes it"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_in_either_mode_never_drains_next_turn_work_queued_behind_it() {
    let double = kernel_double(SEED + 5, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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
        let store = double_unbound_recording_store(&double).await;
        let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
        let config = test_runtime_host_config(&backend);
        let mut runtime = TestRuntime::new(&backend, transport)
            .plugins(Vec::new())
            .tools(Arc::new(tool.clone()))
            .host(EmbeddedRuntimeHost::new(config))
            .store(runtime_store)
            .with_session_id(format!("cancel-no-drain-{mode:?}").to_ascii_lowercase())
            .build()
            .await;
        let persisted = runtime.export_persistence_state();
        let session_id = persisted.session_id.clone();
        let driver = lash_core::facade_support::TurnWorkDriver::for_session(
            Arc::clone(&runtime.host.core.control.effect_host),
            session_id.clone(),
            Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
        );
        let turn_id = format!("no-drain-{mode:?}").to_ascii_lowercase();
        let turn = lash_core::task::spawn({
            let turn_scope = persisted.turn_scope(&turn_id);
            let double = double.clone();
            async move {
                let handler = double
                    .open_handler(lash_core::AdmittedScope::new(turn_scope))
                    .await
                    .expect("open the scope's handler");
                let assembled = runtime
                    .drive_turn(
                        TurnInput::text("stop while queued work waits"),
                        lash_core::facade_support::TurnOptions::new(
                            CancellationToken::new(),
                            handler.scoped(),
                        ),
                    )
                    .await;
                handler.close().await.expect("close the scope's handler");
                assembled
            }
        });
        tool.entered.notified().await;
        let queued = enqueue_idle_turn_input(store.as_ref(), &session_id, "queued behind").await;
        let receipt = driver
            .request_cancel(
                TurnCancelRequest::new(
                    lash_core::facade_support::TurnAddress::new(&session_id, &turn_id),
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
        // On the double an immediate abort unwinds the in-handler turn task,
        // dropping the tool future before its watch can observe the
        // cooperative token; the mode distinction is carried by the recorded
        // evidence above (`honoured_after_step`). An after-step stop still
        // proves it never signals the tool's cooperative token.
        if !mode.is_immediate() {
            assert!(
                !tool.observed_cancelled.load(Ordering::SeqCst),
                "{mode:?}: an after-step stop never signals the tool's cooperative token"
            );
        }
        let pending: Vec<_> =
            lash_core::store::IngressStore::list_pending_turn_inputs(store.as_ref(), &session_id)
                .await
                .expect("pending inputs")
                .into_iter()
                .map(|input| input.input.input_id)
                .collect();
        assert_eq!(
            pending,
            vec![queued.input_id.clone()],
            "{mode:?}: a stop never drains the next-turn work queued behind it"
        );
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1, "{mode:?}");
    }
}

const RETRY_AFTER_MS: u64 = 1234;

#[derive(Clone, Default)]
struct RetryOnceTool {
    attempts: Arc<AtomicUsize>,
}

fn retry_once_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
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
    .with_retry_policy(lash_core::ToolRetryPolicy::safe(
        2,
        RETRY_AFTER_MS,
        RETRY_AFTER_MS,
    ))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RetryOnceTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![retry_once_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "retry_once").then(|| Arc::new(retry_once_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return lash_core::ToolOutcome::retryable_failure(
                    lash_core::ToolFailureClass::External,
                    "transient",
                    "transient failure",
                    Some(RETRY_AFTER_MS),
                );
            }
            lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true }))
        })
        .await
        .into()
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
    double: &lash_restate_test::RestateTestBackend,
    tool: RetryOnceTool,
    provider_calls: Arc<AtomicUsize>,
) -> ModeHarness {
    let backend = double.lash_backend();
    let host_clock: Arc<dyn lash_core::Clock> = double.test_clock();
    let config = test_runtime_host_config(&backend).with_clock(host_clock);
    let driver_store = double_unbound_store(double).await;
    lash_core::testing::store_fixtures::bind_conformance_session(
        &driver_store,
        &lash_core::SessionId::from("root"),
    )
    .await;
    let driver = lash_core::facade_support::TurnWorkDriver::for_session(
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

#[tokio::test(flavor = "multi_thread")]
async fn after_step_stop_during_retry_sleep_lands_at_wake_and_stops_at_the_boundary() {
    // The retry sleep is the engine's durable timer: under the manual clock
    // it holds until the test moves the server's time.
    let double = kernel_double(
        SEED + 6,
        lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual),
    )
    .await;
    let tool = RetryOnceTool::default();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let ModeHarness {
        mut runtime,
        driver,
    } = Box::pin(sleeping_retry_harness(
        &double,
        tool.clone(),
        Arc::clone(&provider_calls),
    ))
    .await;
    let turn_id = "after-step-during-sleep";
    let turn = lash_core::task::spawn({
        let double = double.clone();
        async move {
            let handler = double
                .open_handler(AdmittedScope::turn(
                    SessionId::from("root"),
                    TurnId::from(turn_id),
                ))
                .await
                .expect("open the scope's handler");
            let assembled = runtime
                .drive_turn(
                    TurnInput::text("retry then stop"),
                    TurnOptions::new(CancellationToken::new(), handler.scoped()),
                )
                .await;
            handler.close().await.expect("close the scope's handler");
            assembled
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while double.server().timers().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the retry backoff is a live server timer");
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
    double
        .server()
        .advance(std::time::Duration::from_millis(RETRY_AFTER_MS + 1));

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
}

#[tokio::test(flavor = "multi_thread")]
async fn immediate_abort_during_retry_sleep_unwinds_without_the_retry() {
    let double = kernel_double(
        SEED + 7,
        lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual),
    )
    .await;
    let tool = RetryOnceTool::default();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let ModeHarness {
        mut runtime,
        driver,
    } = Box::pin(sleeping_retry_harness(
        &double,
        tool.clone(),
        Arc::clone(&provider_calls),
    ))
    .await;
    let turn_id = "abort-during-sleep";
    let turn = lash_core::task::spawn({
        let double = double.clone();
        async move {
            let handler = double
                .open_handler(AdmittedScope::turn(
                    SessionId::from("root"),
                    TurnId::from(turn_id),
                ))
                .await
                .expect("open the scope's handler");
            let assembled = runtime
                .drive_turn(
                    TurnInput::text("retry then abort"),
                    TurnOptions::new(CancellationToken::new(), handler.scoped()),
                )
                .await;
            handler.close().await.expect("close the scope's handler");
            assembled
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while double.server().timers().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the retry backoff is a live server timer");
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
}
