//! A turn-capture write that fails, on the Restate server double (ADR 0114
//! §4.1, FIG-4069).
//!
//! Only a transient store fault is retried: the step ends its attempt with
//! the live `TransientCaptureWrite`, and the engine runs it again. A write the
//! store refuses deterministically ends its step with that typed store error
//! within the one attempt, since running the step again would be refused the
//! same way and the turn would never end.
//!
//! Each law runs the turn with `run_in_handler` under a pausing retry policy,
//! so a refusal that was retried would pause the invocation instead of
//! hanging the test.

use super::*;
use lash_core::store::RuntimeStoreDecorator;
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x40_69ca;

/// Attempts before the server pauses the handler's invocation.
const ATTEMPTS: u32 = 3;

fn pausing_after(attempts: u32) -> lash_restate_test::RetryPolicy {
    lash_restate_test::RetryPolicy {
        initial_interval: std::time::Duration::from_millis(1),
        exponentiation_factor: 1.0,
        max_interval: std::time::Duration::from_millis(1),
        max_attempts: Some(attempts),
        on_max_attempts: lash_restate_test::server::OnMaxAttempts::Pause,
    }
}

/// How the store answers capture appends.
#[derive(Clone, Copy)]
enum AppendFault {
    /// The first append fails as the storage substrate; every later one
    /// persists.
    TransientOnce,
    /// Every append is refused: the turn's capture is sealed.
    Refused,
    /// Only the append of a tool's settlement is refused, as corrupt.
    SettlementRefused,
}

/// A store whose capture appends fail as `fault` says.
struct CaptureAppendFaults {
    inner: Arc<RecordingStore>,
    fault: AppendFault,
    appends: AtomicUsize,
}

impl CaptureAppendFaults {
    fn appends(&self) -> usize {
        self.appends.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl RuntimeStoreDecorator for CaptureAppendFaults {
    type Inner = RecordingStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn append_capture_batch(
        &self,
        batch: &lash_core::store::CaptureBatch,
    ) -> Result<lash_core::store::CaptureAck, lash_core::StoreError> {
        let append = self.appends.fetch_add(1, Ordering::SeqCst);
        match self.fault {
            AppendFault::TransientOnce if append == 0 => {
                Err(lash_core::StoreError::StorageFailure {
                    backend: "sqlite",
                    message: "injected transient capture append failure".to_string(),
                })
            }
            AppendFault::Refused => Err(lash_core::StoreError::CaptureSealed {
                session_id: batch.lease.turn.session_id.clone(),
                turn_id: batch.lease.turn.turn_id.clone(),
                sealed_through: 0,
            }),
            AppendFault::SettlementRefused
                if batch.frames.iter().any(|frame| {
                    matches!(frame, lash_core::store::CaptureFrame::ToolSettled { .. })
                }) =>
            {
                Err(lash_core::StoreError::StoredDataCorrupt {
                    record_kind: "turn capture",
                    message: "injected refused tool settlement".to_string(),
                })
            }
            AppendFault::TransientOnce | AppendFault::SettlementRefused => {
                lash_core::store::TurnCaptureStore::append_capture_batch(self.inner.as_ref(), batch)
                    .await
            }
        }
    }
}

/// The last attempt's turn: its outcome and the issues it reported, or the
/// error it failed with.
type AttemptedTurn = Result<(TurnOutcome, String), String>;

/// What the handler's turn ran to.
struct CapturedTurn {
    model_calls: usize,
    appends: usize,
    handler: Result<(), String>,
    outcome: Option<AttemptedTurn>,
    handler_attempts: u32,
}

/// Run one turn in a handler on the double, over a store whose capture
/// appends fail as `fault` says. Its first model call asks for the echo tool
/// when `tool_call` is set; every other call answers text.
async fn run_captured_turn(fault: AppendFault, turn_id: &str, tool_call: bool) -> CapturedTurn {
    let double = kernel_double(
        SEED,
        lash_restate_test::ServerConfig::default().retry(pausing_after(ATTEMPTS)),
    )
    .await;
    let backend = double.lash_backend();
    let store = Arc::new(CaptureAppendFaults {
        inner: double_unbound_recording_store(&double).await,
        fault,
        appends: AtomicUsize::new(0),
    });
    let model_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_request| {
                let call = model_calls.fetch_add(1, Ordering::SeqCst);
                let part = if tool_call && call == 0 {
                    LlmOutputPart::ToolCall {
                        call_id: "captured-call".to_string(),
                        tool_name: "echo_tool".to_string(),
                        input_json: r#"{"value":"echoed"}"#.to_string(),
                        replay: None,
                    }
                } else {
                    LlmOutputPart::Text {
                        text: "captured before it publishes".to_string(),
                        response_meta: None,
                    }
                };
                std::future::ready(Ok(LlmResponse {
                    parts: vec![part],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                }))
            }
        })
        .build();
    let runtime = TestRuntime::new(&backend, transport)
        .tools(Arc::new(EchoTool))
        .host(test_host_config(&backend))
        .store(Arc::clone(&store) as Arc<dyn lash_core::RuntimeStore>)
        .build()
        .await;
    let session_id = runtime.session_id().to_string();
    let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
    let outcome: Arc<std::sync::Mutex<Option<AttemptedTurn>>> = Arc::default();
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
                        TurnInput::text("answer over a failing capture write"),
                        TurnOptions::new(CancellationToken::new(), scoped),
                    )
                    .await
                    .map(|turn| (turn.outcome, format!("{:?}", turn.errors)))
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
    let outcome = outcome.lock_recover().take();
    CapturedTurn {
        model_calls: model_calls.load(Ordering::SeqCst),
        appends: store.appends(),
        handler,
        outcome,
        handler_attempts,
    }
}

/// A capture append the store refuses deterministically ends the model call's
/// step with that typed refusal, in the one attempt: it is never retried as a
/// transient fault.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_refused_capture_write_ends_the_turn_typed_within_one_attempt() {
    let turn = Box::pin(run_captured_turn(
        AppendFault::Refused,
        "refused-capture",
        false,
    ))
    .await;
    turn.handler
        .expect("a refused capture write ends its turn, not pauses its invocation");
    assert_eq!(turn.handler_attempts, 1, "the refusal was never retried");
    assert_eq!(turn.model_calls, 1, "the model ran once");
    assert_eq!(turn.appends, 1, "the refused append was made once");
    let outcome = turn.outcome.expect("the handler ran the turn");
    let failure = match outcome {
        Err(error) => error,
        Ok((outcome @ TurnOutcome::Stopped(_), errors)) => format!("{outcome:?}: {errors}"),
        Ok((outcome, _)) => panic!("a refused capture write must end the turn: {outcome:?}"),
    };
    assert!(
        failure.contains("runtime_store")
            && failure.contains("is sealed through sequence 0")
            && !failure.contains("transient_capture_write"),
        "the turn ends with the store's typed refusal, not a transient fault: {failure}"
    );
}

/// A transient capture append fault ends the attempt with the live
/// `TransientCaptureWrite`; the engine runs the step again, and the turn
/// finishes on that retry.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_transient_capture_write_fault_is_retried_and_then_succeeds() {
    let turn = Box::pin(run_captured_turn(
        AppendFault::TransientOnce,
        "transient-capture",
        false,
    ))
    .await;
    turn.handler.expect("the retried handler completes");
    assert_eq!(
        turn.handler_attempts, 2,
        "the transient fault ended exactly one attempt"
    );
    assert_eq!(
        turn.model_calls, 2,
        "the failed step was never recorded, so the retry ran the model call again"
    );
    assert!(
        turn.appends >= 2,
        "the retry appended again: {}",
        turn.appends
    );
    let (outcome, _) = turn
        .outcome
        .expect("the retry ran the turn")
        .expect("the retried turn assembles");
    assert!(
        matches!(outcome, TurnOutcome::Finished(_)),
        "the retried turn finishes: {outcome:?}"
    );
}

/// A tool attempt whose settlement the store refuses ends its step with that
/// typed refusal in the one attempt, as a refused model-call capture does.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_refused_tool_settlement_capture_ends_the_turn_typed_within_one_attempt() {
    let turn = Box::pin(run_captured_turn(
        AppendFault::SettlementRefused,
        "refused-settlement",
        true,
    ))
    .await;
    turn.handler
        .expect("a refused settlement ends its turn, not pauses its invocation");
    assert_eq!(turn.handler_attempts, 1, "the refusal was never retried");
    assert_eq!(turn.model_calls, 1, "the turn ended at the tool step");
    let outcome = turn.outcome.expect("the handler ran the turn");
    let failure = match outcome {
        Err(error) => error,
        Ok((outcome @ TurnOutcome::Stopped(_), errors)) => format!("{outcome:?}: {errors}"),
        Ok((outcome, _)) => panic!("a refused settlement must end the turn: {outcome:?}"),
    };
    assert!(
        failure.contains("runtime_store_corrupt")
            && failure.contains("injected refused tool settlement")
            && !failure.contains("transient_capture_write"),
        "the turn ends with the store's typed refusal, not a transient fault: {failure}"
    );
}
