//! Worker loss and the stopped partial on the Restate double (ADR 0114 §4.4).
//!
//! A model call streams, and its handler dies before the server stores the
//! call's result. The replay re-runs the call on a successor, whose capture
//! writer inherits the dead worker's acknowledged frames and retracts them
//! before it streams anything new. When the turn is then stopped, its partial
//! holds what the successor streamed, once: never the retracted attempt's
//! text beside it.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::sync::MutexExt as _;
use lash::{CancelTarget, CaptureCoverage, PartialItem, StopReason, TurnInput};
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    LlmOutputPart, LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity,
};
use lash_restate_test::{CrashPoint, RestateTestBackend, ServerConfig, TURN_DRIVER_SERVICE};
use tokio::sync::oneshot;

const SEED: u64 = 0x0433_0114;
const SESSION: &str = "stopped-partial-recovery";
const ROOT: &str = "turn-1";
const CRASHED: &str = "streamed by the worker that died";
const REDRIVEN: &str = "streamed again by its successor";

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

fn stream_text(request: &LlmRequest, text: &str) {
    let stream = request
        .stream_events
        .as_ref()
        .expect("runtime stream event sender");
    let block = StreamBlockIdentity::new("text:0", 0);
    stream.send(LlmStreamEvent::TextBlockStart {
        block: block.clone(),
    });
    stream.send(LlmStreamEvent::Delta {
        block,
        text: text.to_string(),
    });
}

/// The provider's script. `stall_redrive` holds the second call after it
/// streams, until the stop drops it; otherwise every call answers.
fn provider(
    calls: Arc<AtomicUsize>,
    stall_redrive: bool,
    started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("stopped-partial-recovery")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            let started = (call > 0).then(|| started.lock_recover().take()).flatten();
            async move {
                match call {
                    0 => {
                        stream_text(&request, CRASHED);
                        Ok::<_, LlmTransportError>(text_response(CRASHED))
                    }
                    _ if stall_redrive => {
                        stream_text(&request, REDRIVEN);
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        std::future::pending::<()>().await;
                        unreachable!("the stop drops the provider call")
                    }
                    _ => Ok(text_response(REDRIVEN)),
                }
            }
        })
        .build()
        .into_handle()
}

fn core(
    backend: &RestateTestBackend,
    provider: lash_core::facade_support::ProviderHandle,
) -> lash::LashCore {
    lash::LashCore::standard_builder(backend.lash_backend(), lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(
            lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(16_000)
                .build()
                .expect("model spec"),
        )
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "stopped-partial-recovery",
        ))
        .expect("build the lash core")
}

/// The name of the root's model call run, from a clean reference run under
/// the same seed: the journal point the crash run drops.
async fn model_call_run_name() -> String {
    let backend = lash_restate_test::backend(SEED, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let calls = Arc::new(AtomicUsize::new(1));
    let core = core(&backend, provider(calls, false, Arc::new(Mutex::new(None))));
    let session = core
        .session(SESSION)
        .open()
        .await
        .expect("open the session");
    session
        .send(TurnInput::text("answer"))
        .id(ROOT)
        .output()
        .await
        .expect("the reference turn finishes");
    let server = backend.server();
    server.settle().await;
    let turn_driver = backend.service_name(TURN_DRIVER_SERVICE);
    server
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(&format!("{turn_driver}/")))
        .flat_map(|view| server.journal(&view.id).unwrap_or_default())
        .filter_map(|entry| entry.name)
        .find(|name| name.contains(":llm_call:"))
        .expect("the reference root journals its model call")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redriven_model_call_retracts_the_lost_attempts_stream_from_the_partial() {
    let run_name = model_call_run_name().await;
    let backend = lash_restate_test::backend(SEED, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    backend.crash_turn_drive(CrashPoint::BeforeRunResult {
        name: Some(run_name.clone()),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let (started_tx, started) = oneshot::channel();
    let core = core(
        &backend,
        provider(
            Arc::clone(&calls),
            true,
            Arc::new(Mutex::new(Some(started_tx))),
        ),
    );
    let session = core
        .session(SESSION)
        .open()
        .await
        .expect("open the session");
    let handle = session
        .send(TurnInput::text("answer"))
        .id(ROOT)
        .await
        .expect("accept the turn input");
    let input_id = handle.input_id().clone();
    let settled = tokio::spawn(async move { handle.output().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), started)
        .await
        .expect("the redriven call streams")
        .expect("the provider signals");
    assert_eq!(
        backend.server().stats().crashes,
        1,
        "the root crashed before its model call's result ({run_name}) was stored"
    );
    session
        .cancel(CancelTarget::Input(input_id))
        .origin("user")
        .await
        .expect("cancel");
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), settled)
        .await
        .expect("the stop settles the turn")
        .expect("send task")
        .expect("stopped report");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one call per attempt");

    let partial = output
        .result
        .stopped_partial
        .expect("a stopped turn reports its partial");
    assert_eq!(partial.verify_digest(), Ok(()));
    assert_eq!(partial.reason, StopReason::UserCancel);
    assert!(
        partial.recovered_after_process_loss,
        "a successor fenced the dead worker's writer"
    );
    assert_eq!(partial.coverage, CaptureCoverage::AcknowledgedPrefix);
    let texts: Vec<&str> = partial
        .items
        .iter()
        .filter_map(|item| match item {
            PartialItem::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        [REDRIVEN],
        "the retracted attempt's text never joins the re-streamed text: {:?}",
        partial.items
    );
}

const PROGRESS_SESSION: &str = "stopped-partial-rebuilt-child";
const PROGRESS_TOOL: &str = "progress_call";
const DISPATCH: &str = "EffectGroupDispatch";

fn progress_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{PROGRESS_TOOL}"),
        PROGRESS_TOOL,
        "Report progress, then wait to be stopped.",
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        serde_json::json!({"type": "object"}),
    )
}

/// Reports `attempt {n}` through its attempt context, then waits for its
/// stop and answers cancelled.
#[derive(Clone, Default)]
struct ProgressTool {
    executions: Arc<AtomicUsize>,
    reported: Arc<Mutex<Vec<Result<(), String>>>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ProgressTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![progress_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == PROGRESS_TOOL).then(|| Arc::new(progress_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let attempt = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        let reported = call
            .context
            .progress()
            .report(lash::ToolOutputChunk {
                text: format!("attempt {attempt}\n"),
            })
            .await
            .map_err(|refused| refused.to_string());
        self.reported.lock_recover().push(reported);
        call.context
            .cancellation_token()
            .cloned()
            .unwrap_or_default()
            .cancelled()
            .await;
        self.stopped.store(true, Ordering::SeqCst);
        lash_core::ToolOutcome::cancelled("the turn asked the tool to stop").into()
    }
}

/// A tool child whose attempt dies while its turn is live nowhere is
/// redriven on a context the deployment builds (FIG-3712). That successor
/// reopens the turn's capture: it fences the dead attempt's writer, retracts
/// what it wrote, and captures its own progress, so the turn's stop seals
/// the successor's chunk and never the lost one (ADR 0114 §3.2, Lane G
/// amendment).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_child_redriven_on_a_successor_captures_its_progress() {
    let backend = lash_restate_test::backend(
        SEED + 1,
        ServerConfig::default().time(lash_restate_test::TimeMode::Manual),
    )
    .await
    .expect("build the Restate test backend");
    let tool = ProgressTool::default();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stopped-partial-rebuilt-child")
        .complete(move |_request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "call-1".into(),
                    tool_name: PROGRESS_TOOL.into(),
                    input_json: "{}".into(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let core =
        lash::LashCore::standard_builder(backend.lash_backend(), lash::TurnBudget::Unbounded)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .provider(provider)
            .model(
                lash_core::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .tools(Arc::new(tool.clone()) as Arc<dyn lash_core::ToolProvider>)
            .build(lash_core::LeaseOwnerIdentity::opaque(
                "lash-restate-test",
                "stopped-partial-rebuilt-child",
            ))
            .expect("build the lash core");
    let session = core
        .session(PROGRESS_SESSION)
        .open()
        .await
        .expect("open the session");
    let handle = session
        .send(TurnInput::text("report progress"))
        .id(ROOT)
        .await
        .expect("accept the turn input");
    let run = tokio::spawn(handle.output());
    let server = backend.server().clone();
    let child =
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                if let Some(view) = server.invocations().into_iter().find(|view| {
                    view.target.starts_with(DISPATCH) && view.target.ends_with("/child")
                }) {
                    return view.id;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the tool child is dispatched");
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while tool.reported.lock_recover().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the first attempt reports beside its live turn");

    // The turn is held, so it is live nowhere; the child's attempt dies,
    // and its successor runs on a context the deployment builds.
    let hold = server
        .hold(
            TURN_DRIVER_SERVICE,
            &lash_restate::turn_workflow_key(
                &lash_core::SessionId::from(PROGRESS_SESSION),
                &lash::TurnId::from(ROOT),
            ),
        )
        .await;
    assert!(server.crash(&child), "the child is running");
    let ticker = tokio::spawn({
        let server = server.clone();
        async move {
            loop {
                server.advance(std::time::Duration::from_secs(1));
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while tool.reported.lock_recover().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the successor attempt runs the tool on a built context");
    assert_eq!(
        tool.reported.lock_recover().clone(),
        vec![Ok(()), Ok(())],
        "both attempts' chunks were accepted"
    );

    session
        .cancel(CancelTarget::Root(lash::TurnId::from(ROOT)))
        .reason("stop")
        .await
        .expect("the durable cancel is accepted");
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !tool.stopped.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the successor's tool sees its turn's stop");
    hold.release();
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), run)
        .await
        .expect("the stop settles the turn")
        .expect("send task")
        .expect("stopped report");
    ticker.abort();

    let partial = output
        .result
        .stopped_partial
        .expect("a stopped turn reports its partial");
    assert_eq!(partial.verify_digest(), Ok(()));
    assert_eq!(partial.reason, StopReason::UserCancel);
    assert!(
        partial.recovered_after_process_loss,
        "the successor fenced the dead attempt's writer"
    );
    let [
        PartialItem::ToolCall {
            call, execution, ..
        },
    ] = partial.items.as_slice()
    else {
        panic!("expected the one call, got {:?}", partial.items);
    };
    assert_eq!(call.call_id, "call-1");
    let lash::ToolExecutionState::Running(running) = execution else {
        panic!("the stop interrupted the call, got {execution:?}");
    };
    assert_eq!(
        running.output,
        lash::ToolOutputCapture::Captured {
            chunks: vec![lash::ToolOutputChunk {
                text: "attempt 2\n".to_string(),
            }],
            omitted_bytes: 0,
        },
        "the successor's chunk, and never the retracted one"
    );
}
