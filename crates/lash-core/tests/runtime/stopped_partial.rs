//! Runtime witnesses for the stopped partial (ADR 0114): what a stopped turn
//! streamed persists as capture before a host sees it, and the stop seals it
//! into a typed partial on the turn's report and publishes it after the
//! commit.

use super::tests::*;
use lash_core::TurnCancelMode;
use lash_core::facade_support::{TurnCancelOutcome, TurnCancelRequest};
use lash_core::testing::TestTurnDrive as _;
use lash_sansio::{
    CaptureCoverage, CutState, PartialItem, StopReason, StoppedPartial, ToolOutputChunk,
};

const SEED: u64 = 0x5_0114;
const STREAMED: &str = "the answer so far";
const PROGRESS: &str = "first line of output\n";

struct StopHarness {
    runtime: LashRuntime,
    driver: lash_core::facade_support::TurnWorkDriver,
}

async fn stop_harness(
    double: &lash_restate_test::RestateTestBackend,
    tools: Arc<dyn lash_core::ToolProvider>,
    transport: TestProvider,
) -> StopHarness {
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
    // The session's durable store: what the turn's capture writes.
    let store = double_unbound_store(double).await;
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        tools,
        transport,
        host,
        store,
    )
    .await;
    StopHarness { runtime, driver }
}

fn block() -> lash_core::llm::types::StreamBlockIdentity {
    lash_core::llm::types::StreamBlockIdentity::new("message:m1", 0)
}

/// Streams the start of an answer, then never finishes it. A provider may
/// stream deltas without announcing their block first.
fn stalling_stream_provider(announce_block: bool) -> TestProvider {
    TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| async move {
            let events = request.stream_events.expect("runtime stream event sender");
            if announce_block {
                events.send(LlmStreamEvent::TextBlockStart { block: block() });
            }
            events.send(LlmStreamEvent::Delta {
                block: block(),
                text: STREAMED.to_string(),
            });
            std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
        })
        .build()
}

/// Answers the first call with one `echo_tool` call.
fn tool_calling_provider() -> TestProvider {
    TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_request| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "call-1".to_string(),
                    tool_name: "echo_tool".to_string(),
                    input_json: serde_json::json!({"value": "call-1"}).to_string(),
                    replay: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build()
}

/// Reports one progress chunk through its attempt context, then runs until
/// the turn is aborted.
#[derive(Clone, Default)]
struct ProgressThenHoldTool {
    refused: Arc<StdMutex<Option<String>>>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ProgressThenHoldTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        EchoTool.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        EchoTool.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if let Err(refused) = call
            .context
            .progress()
            .report(ToolOutputChunk {
                text: PROGRESS.to_string(),
            })
            .await
        {
            *self.refused.lock_recover() = Some(refused.to_string());
        }
        std::future::pending::<lash_core::ToolAttemptOutcome>().await
    }
}

async fn wait_for_activity(
    activities: &RecordingTurnEvents,
    what: &str,
    seen: impl Fn(&TurnEvent) -> bool,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !activities
            .snapshot()
            .iter()
            .any(|activity| seen(&activity.event))
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the host never saw {what}"));
}

/// Drives `turn_id` until `seen` reaches the host, then aborts it.
async fn stop_after(
    double: &lash_restate_test::RestateTestBackend,
    harness: StopHarness,
    turn_id: &'static str,
    what: &str,
    seen: impl Fn(&TurnEvent) -> bool,
) -> (AssembledTurn, Vec<SessionStreamEvent>) {
    let StopHarness {
        mut runtime,
        driver,
    } = harness;
    let activities = RecordingTurnEvents::default();
    let events = RecordingSink::default();
    let turn = lash_core::task::spawn({
        let double = double.clone();
        let activities = activities.clone();
        let events = events.clone();
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
                    TurnInput::text("start, then be stopped"),
                    TurnOptions::new(CancellationToken::new(), handler.scoped())
                        .with_events(&events)
                        .with_turn_events(&activities),
                )
                .await;
            handler.close().await.expect("close the scope's handler");
            assembled
        }
    });
    wait_for_activity(&activities, what, seen).await;
    let receipt = driver
        .request_cancel(
            TurnCancelRequest::new(
                lash_core::facade_support::TurnAddress::new("root", TurnId::from(turn_id)),
                "stop-now",
                Some("test-user".to_string()),
            )
            .with_reason("stopped partial witness")
            .mode(TurnCancelMode::Immediate),
        )
        .await
        .expect("request the stop");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    let turn = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the stop unwinds the turn")
        .expect("turn task")
        .expect("turn assembles");
    (turn, events.snapshot())
}

fn sealed_partial(turn: &AssembledTurn) -> &StoppedPartial {
    assert!(
        matches!(
            turn.outcome,
            TurnOutcome::Stopped(TurnStop::Cancelled { .. })
        ),
        "expected a cancelled turn, got {:?}",
        turn.outcome
    );
    let partial = turn
        .stopped_partial
        .as_ref()
        .expect("a stopped turn reports its sealed partial");
    partial
        .verify_digest()
        .expect("the partial's digest verifies");
    assert_eq!(partial.reason, StopReason::UserCancel);
    assert!(!partial.recovered_after_process_loss);
    assert_eq!(partial.coverage, CaptureCoverage::Complete);
    partial
}

/// The partial is announced once, after the stopped outcome, naming the
/// partial the report carries.
fn assert_announced_after_outcome(events: &[SessionStreamEvent], partial: &StoppedPartial) {
    let outcome = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionStreamEvent::TurnOutcome {
                    outcome: TurnOutcome::Stopped(_)
                }
            )
        })
        .expect("the host sees the stopped outcome");
    let announced: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            SessionStreamEvent::StoppedPartialAvailable { summary } => Some((index, summary)),
            _ => None,
        })
        .collect();
    let [(index, summary)] = announced.as_slice() else {
        panic!("expected one announcement, got {announced:?}");
    };
    assert!(*index > outcome, "announced after the outcome: {events:?}");
    assert_eq!(summary.id, partial.id);
    assert_eq!(summary.digest, partial.digest);
    assert_eq!(summary.item_count as usize, partial.items.len());
}

async fn stop_mid_stream_seals_the_streamed_text(seed: u64, announce_block: bool) {
    let double = kernel_double(seed, lash_restate_test::ServerConfig::default()).await;
    let harness = Box::pin(stop_harness(
        &double,
        Arc::new(EchoTool),
        stalling_stream_provider(announce_block),
    ))
    .await;
    let (turn, events) = Box::pin(stop_after(
        &double,
        harness,
        "stop-mid-stream",
        "the streamed delta",
        |event| matches!(event, TurnEvent::AssistantProseDelta { text, .. } if &**text == STREAMED),
    ))
    .await;

    let partial = sealed_partial(&turn);
    let [PartialItem::Text { state, text, .. }] = partial.items.as_slice() else {
        panic!("expected one text item, got {:?}", partial.items);
    };
    assert_eq!(text, STREAMED, "everything the host saw is in the partial");
    assert_eq!(*state, CutState::Interrupted);
    assert!(!partial.cut_mid_tool_call());
    assert_announced_after_outcome(&events, partial);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_mid_stream_seals_the_streamed_text_the_host_already_saw() {
    stop_mid_stream_seals_the_streamed_text(SEED, true).await;
}

/// A delta that arrives before any block start is legal provider output: the
/// capture opens its block, so the stop seals a partial and never parks on a
/// malformed capture.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_after_deltas_without_a_block_start_still_seals_them() {
    stop_mid_stream_seals_the_streamed_text(SEED + 2, false).await;
}

/// A tool reports progress through its attempt context, and the host sees
/// each chunk only once it persisted. An immediate stop cancels the running
/// call: the iteration's checkpoint commits the call with its cancelled
/// result, and so advances the capture base past it (ADR 0114 §3.1). The
/// sealed tail is empty, like an after-step stop's.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_mid_tool_commits_the_cancelled_call_and_seals_an_empty_tail() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let tool = ProgressThenHoldTool::default();
    let harness = Box::pin(stop_harness(
        &double,
        Arc::new(tool.clone()),
        tool_calling_provider(),
    ))
    .await;
    let (turn, events) = Box::pin(stop_after(
        &double,
        harness,
        "stop-mid-tool",
        "the tool's progress",
        |event| matches!(event, TurnEvent::ToolOutputProgress { chunk, .. } if chunk.text == PROGRESS),
    ))
    .await;

    assert_eq!(
        *tool.refused.lock_recover(),
        None,
        "the capture accepted the chunk"
    );
    let [record] = turn.tool_calls.as_slice() else {
        panic!("expected the one call, got {:?}", turn.tool_calls);
    };
    assert_eq!(record.call_id.as_deref(), Some("call-1"));
    assert!(matches!(
        record.output.outcome,
        lash_sansio::ToolCallOutcome::Cancelled(_)
    ));
    let partial = sealed_partial(&turn);
    assert_eq!(partial.id.base, lash_sansio::CaptureBase(1));
    assert!(partial.items.is_empty(), "{:?}", partial.items);
    assert_eq!(
        partial.eligibility(),
        lash_sansio::ResubmissionEligibility::Empty
    );
    assert_announced_after_outcome(&events, partial);
}
