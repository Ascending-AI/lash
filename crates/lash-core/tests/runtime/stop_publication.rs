//! A stopped turn's terminal publishes only after its commit (ADR 0122).
//!
//! The turn records its `Stopped` terminal when it stops and holds its
//! publication on the observer: the host receives the terminal once the
//! commit is accepted, and never when the commit fails. What the turn
//! streamed before the stop reaches the host as it streamed, and lash keeps
//! no durable record of it.

use super::tests::*;
use lash_core::TurnCancelMode;
use lash_core::facade_support::{TurnCancelOutcome, TurnCancelRequest};
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x5_0122;
const STREAMED: &str = "the answer so far";

/// Records each session event with the runtime commits its store had applied
/// when the host received it.
#[derive(Clone)]
struct CommitOrderSink {
    store: Arc<lash_core::testing::runtime_helpers::RecordingStore>,
    events: Arc<StdMutex<Vec<(SessionStreamEvent, usize)>>>,
}

impl CommitOrderSink {
    fn over(store: &Arc<lash_core::testing::runtime_helpers::RecordingStore>) -> Self {
        Self {
            store: Arc::clone(store),
            events: Arc::default(),
        }
    }

    fn snapshot(&self) -> Vec<(SessionStreamEvent, usize)> {
        self.events.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::EventSink for CommitOrderSink {
    async fn emit(&self, event: SessionStreamEvent) {
        let commits = *self.store.runtime_commit_count.lock_recover();
        self.events.lock_recover().push((event, commits));
    }
}

/// The session events that make up a stopped turn's terminal.
fn is_terminal(event: &SessionStreamEvent) -> bool {
    matches!(
        event,
        SessionStreamEvent::Error { .. }
            | SessionStreamEvent::TurnOutcome { .. }
            | SessionStreamEvent::Done
    )
}

async fn runtime_over(
    double: &lash_restate_test::RestateTestBackend,
    transport: TestProvider,
    store: Arc<dyn lash_core::RuntimeStore>,
) -> LashRuntime {
    Box::pin(runtime_and_driver_over(double, transport, store))
        .await
        .0
}

/// The runtime, and a work driver over the same effect host that requests
/// cancels of the runtime's turns.
async fn runtime_and_driver_over(
    double: &lash_restate_test::RestateTestBackend,
    transport: TestProvider,
    store: Arc<dyn lash_core::RuntimeStore>,
) -> (LashRuntime, lash_core::facade_support::TurnWorkDriver) {
    let backend = double.lash_backend();
    let config = test_runtime_host_config(&backend);
    let driver_store = double_unbound_store(double).await;
    lash_core::store::SessionCatalogStore::admit_session(
        driver_store.as_ref(),
        &lash_core::testing::store_fixtures::root_session_request(&SessionId::from("root")),
    )
    .await
    .expect("admit the driver's session");
    let driver = lash_core::facade_support::TurnWorkDriver::for_session(
        Arc::clone(&config.control.effect_host),
        "root",
        driver_store,
    );
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        EmbeddedRuntimeHost::new(config),
        store,
    )
    .await;
    (runtime, driver)
}

/// A provider whose first call fails for good: the machine writes its
/// `Error` right before the stopped outcome.
fn refusing_provider() -> TestProvider {
    mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Err(
            lash_core::llm::transport::LlmTransportError::new("the provider refused")
                .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::Forbidden),
        ),
    }])
}

/// Streams the start of an answer, then never finishes it.
fn stalling_stream_provider() -> TestProvider {
    TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            let events = request.stream_events.expect("runtime stream event sender");
            let block = lash_core::llm::types::StreamBlockIdentity::new("message:m1", 0);
            events.send(LlmStreamEvent::TextBlockStart {
                block: block.clone(),
            });
            events.send(LlmStreamEvent::Delta {
                block,
                text: STREAMED.to_string(),
            });
            std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
        })
        .build()
}

async fn drive(
    double: &lash_restate_test::RestateTestBackend,
    runtime: &mut LashRuntime,
    turn_id: &str,
    sink: &CommitOrderSink,
) -> Result<AssembledTurn, RuntimeError> {
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from(turn_id),
        ))
        .await
        .expect("open the scope's handler");
    let turn = runtime
        .drive_turn(
            TurnInput::text("start"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()).with_events(sink),
        )
        .await;
    handler.close().await.expect("close the scope's handler");
    turn
}

/// A provider failure stops the turn through the machine. The machine's
/// `Error` and the stopped outcome are both the stop's terminal: neither, nor
/// `Done`, reaches the host before the turn's commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failure_publishes_its_error_after_the_commit() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = Box::pin(runtime_over(
        &double,
        refusing_provider(),
        Arc::clone(&store) as Arc<dyn lash_core::RuntimeStore>,
    ))
    .await;
    let sink = CommitOrderSink::over(&store);

    let turn = drive(&double, &mut runtime, "provider-failure", &sink)
        .await
        .expect("the turn assembles");

    assert_eq!(turn.outcome, TurnOutcome::Stopped(TurnStop::ProviderError));
    let events = sink.snapshot();
    let terminal: Vec<_> = events
        .iter()
        .filter(|(event, _)| is_terminal(event))
        .collect();
    assert!(
        matches!(
            terminal.as_slice(),
            [
                (SessionStreamEvent::Error { .. }, _),
                (SessionStreamEvent::TurnOutcome { .. }, _),
                (SessionStreamEvent::Done, _),
            ]
        ),
        "the terminal in order: {terminal:?}"
    );
    for (event, commits) in terminal {
        assert!(
            *commits >= 1,
            "the host received {event:?} before the turn's commit"
        );
    }
}

/// A turn whose commit the store refuses publishes none of its held
/// terminal: no host sees `Stopped` or `Done` for a turn that never
/// committed.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_commit_publishes_none_of_the_stopped_terminal() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = Box::pin(runtime_over(
        &double,
        refusing_provider(),
        Arc::clone(&store) as Arc<dyn lash_core::RuntimeStore>,
    ))
    .await;
    let sink = CommitOrderSink::over(&store);
    store.fail_next_runtime_commit(lash_core::StoreError::RecordEncodingFailed {
        record_kind: "turn commit".to_string(),
        message: "injected commit refusal".to_string(),
    });

    let error = drive(&double, &mut runtime, "refused-commit", &sink)
        .await
        .expect_err("a refused commit fails the turn");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RecordEncodingFailed
    );
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);
    let events = sink.snapshot();
    assert!(
        !events.iter().any(|(event, _)| matches!(
            event,
            SessionStreamEvent::TurnOutcome { .. } | SessionStreamEvent::Done
        )),
        "a turn whose commit failed published its terminal: {events:?}"
    );
    assert!(
        !events.iter().any(|(event, _)| matches!(
            event,
            SessionStreamEvent::Error { message, .. } if message.contains("the provider refused")
        )),
        "a turn whose commit failed published the machine's error: {events:?}"
    );
}

/// An `Immediate` cancel backtracks to the last checkpoint. The host keeps
/// what streamed before the stop, and receives the stop's terminal, from the
/// `Done` the cancel records through the stopped outcome, only after the
/// commit.
#[tokio::test(flavor = "multi_thread")]
async fn an_immediate_cancel_publishes_its_stop_after_the_commit() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let store = double_unbound_recording_store(&double).await;
    let (mut runtime, driver) = Box::pin(runtime_and_driver_over(
        &double,
        stalling_stream_provider(),
        Arc::clone(&store) as Arc<dyn lash_core::RuntimeStore>,
    ))
    .await;
    let sink = CommitOrderSink::over(&store);
    let turn = lash_core::task::spawn({
        let double = double.clone();
        let sink = sink.clone();
        async move { drive(&double, &mut runtime, "immediate-cancel", &sink).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !sink.snapshot().iter().any(|(event, _)| {
            matches!(event, SessionStreamEvent::TextDelta { content, .. } if content == STREAMED)
        }) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the host sees the streamed text before the stop");
    let receipt = driver
        .request_cancel(
            TurnCancelRequest::new(
                lash_core::facade_support::TurnAddress::new(
                    "root",
                    TurnId::from("immediate-cancel"),
                ),
                "stop-now",
                Some("test-user".to_string()),
            )
            .with_reason("stop publication witness")
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

    assert!(
        matches!(
            turn.outcome,
            TurnOutcome::Stopped(TurnStop::Cancelled { .. })
        ),
        "{:?}",
        turn.outcome
    );
    let events = sink.snapshot();
    let streamed = events
        .iter()
        .position(|(event, _)| {
            matches!(event, SessionStreamEvent::TextDelta { content, .. } if content == STREAMED)
        })
        .expect("the streamed text reached the host");
    let terminal: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, (event, _))| is_terminal(event))
        .collect();
    assert!(
        matches!(
            terminal.as_slice(),
            [
                (_, (SessionStreamEvent::Done, _)),
                (_, (SessionStreamEvent::TurnOutcome { .. }, _)),
                (_, (SessionStreamEvent::Done, _)),
            ]
        ),
        "the terminal in order: {terminal:?} of {events:?}"
    );
    for (index, (event, commits)) in terminal {
        assert!(index > streamed, "{event:?} overtook the streamed text");
        assert!(
            *commits >= 1,
            "the host received {event:?} before the turn's commit"
        );
    }
}
