//! A tool turn whose handler is suspended while its tool child still needs an
//! attempt (FIG-3712).
//!
//! Restate suspends a handler that waits on the server past its inactivity
//! timeout; its in-process state, the turn's live opener included, goes with
//! it. A group tool child runs against its opener's live context, so a child
//! attempt that starts while the turn is suspended has no opener to run
//! against. The turn is waiting on that child's settlement and the child on a
//! live turn: each law here drives a real tool turn into that state and
//! requires it to finish anyway.
//!
//! # Status: red, ignored until FIG-3712 lands
//!
//! Root cause: on Restate a group tool child can only run against its
//! opener's in-process dispatch context, which it finds in the
//! `LiveOpenerRegistry`. A handler suspension drops the turn's registration,
//! and the turn resumes only when the child settles. So every child attempt
//! that starts while the turn is suspended answers `no executor currently
//! routes … tool child`, and the child backs off until the engine pauses it.
//!
//! Both laws are `#[ignore]`d with that reason. Run them with
//! `kiln run //crates/lash-restate-test:suspended_turn__test -- --ignored`,
//! and delete both `#[ignore]` lines in the change that fixes FIG-3712.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::{RestateTestBackend, ServerConfig, TimeMode};
use serde_json::json;

const TURN_HOST: &str = "LashTestHandlerHost";
const DISPATCH: &str = "EffectGroupDispatch";
const TOOL: &str = "gated_call";

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..Default::default()
    }
}

/// Asks for the tool once, then answers `done` once it sees the result.
fn model_reply(request: &LlmRequest) -> LlmResponse {
    let saw_tool_result = serde_json::to_string(&request.messages)
        .unwrap_or_default()
        .contains("gated result");
    if saw_tool_result {
        response(vec![LlmOutputPart::Text {
            text: "done".into(),
            response_meta: None,
        }])
    } else {
        response(vec![LlmOutputPart::ToolCall {
            call_id: "call-1".into(),
            tool_name: TOOL.into(),
            input_json: "{}".into(),
            replay: None,
        }])
    }
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Answer once the test opens the gate.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
    )
}

/// A tool that waits for the test to open its gate before it answers, so the
/// test decides what the turn's handler goes through meanwhile.
struct GatedTool {
    executions: Arc<AtomicUsize>,
    gate: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for GatedTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        // Held open for the rest of the test once the gate opens, so a
        // re-executed attempt answers at once.
        let _permit = self.gate.acquire().await.expect("the gate never closes");
        lash_core::ToolOutcome::ok(json!({"result": "gated result"})).into()
    }
}

/// One tool turn on a fresh backend: its handler, its tool gate, and where it
/// records its answer.
struct Turn {
    backend: RestateTestBackend,
    gate: Arc<tokio::sync::Semaphore>,
    executions: Arc<AtomicUsize>,
    answer: Arc<Mutex<Option<String>>>,
    run: tokio::task::JoinHandle<Result<(), String>>,
}

async fn start_turn(config: ServerConfig, gate_open: bool) -> Turn {
    let backend = lash_restate_test::backend(0x3712, config)
        .await
        .expect("build the Restate test backend");
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    if gate_open {
        gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
    }
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("suspended-turn")
        .complete(move |request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(model_reply(&request))
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(
        Arc::new(backend.clone()) as Arc<dyn lash::Backend>,
        lash::TurnBudget::Unbounded,
    )
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .provider(provider)
    .model(
        lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("model spec"),
    )
    .tools(Arc::new(GatedTool {
        executions: Arc::clone(&executions),
        gate: Arc::clone(&gate),
    }) as Arc<dyn lash_core::ToolProvider>)
    .build(lash_core::LeaseOwnerIdentity::opaque(
        "lash-restate-test",
        "suspended-turn",
    ))
    .expect("build the lash core");
    let session = core
        .session("suspended-turn")
        .open()
        .await
        .expect("open the session");
    let turn_id = lash::TurnId::from("turn-1");
    let admitted = lash_core::AdmittedScope::unpinned(session.turn_scope(turn_id.clone()))
        .expect("admit the turn scope");
    let answer = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let answer = Arc::clone(&answer);
        Arc::new(move |scoped| {
            let session = session.clone();
            let turn_id = turn_id.clone();
            let answer = Arc::clone(&answer);
            Box::pin(async move {
                let output = session
                    .turn(lash::TurnInput::text("call the gated tool"))
                    .turn_id(turn_id)
                    .advanced()
                    .run_with_scope(scoped)
                    .await;
                *answer.lock().unwrap() = Some(match output {
                    Ok(output) => output
                        .result
                        .assistant_message()
                        .map_or_else(|| format!("{:?}", output.result.outcome), str::to_owned),
                    Err(error) => format!("error: {error}"),
                });
            })
        })
    };
    let run = tokio::spawn({
        let backend = backend.clone();
        async move { backend.run_in_handler(admitted, attempt).await }
    });
    Turn {
        backend,
        gate,
        executions,
        answer,
        run,
    }
}

impl Turn {
    /// Waits for the turn's handler to finish, or reports every invocation
    /// still open after `budget` of wall time.
    async fn finish(self, budget: Duration) -> String {
        let server = self.backend.server().clone();
        match tokio::time::timeout(budget, self.run).await {
            Ok(Ok(Ok(()))) => self
                .answer
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "the handler completed without an answer".to_owned()),
            Ok(Ok(Err(error))) => format!("stuck: {error}"),
            Ok(Err(join)) => format!("the handler task failed: {join}"),
            Err(_) => {
                let open: Vec<_> = server
                    .invocations()
                    .into_iter()
                    .filter(|view| view.status != "completed")
                    .map(|view| {
                        format!(
                            "{} {} attempts={} last_failure={:?}",
                            view.target, view.status, view.attempts, view.last_failure
                        )
                    })
                    .collect();
                format!("stuck: {open:#?}")
            }
        }
    }

    /// The invocation id of the tool child's dispatch, once it exists.
    async fn tool_child(&self) -> String {
        loop {
            if let Some(view) = self
                .backend
                .server()
                .invocations()
                .into_iter()
                .find(|view| view.target.starts_with(DISPATCH) && view.target.ends_with("/child"))
            {
                return view.id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until the turn's handler has been suspended.
    async fn turn_suspended(&self) {
        loop {
            if self
                .backend
                .server()
                .invocations()
                .into_iter()
                .any(|view| view.target.starts_with(TURN_HOST) && view.status == "suspended")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// The turn's handler is suspended while its tool is still running, and the
/// tool child's attempt then dies, so the child's next attempt starts with no
/// live turn in the process. The turn must still finish with the tool's
/// answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-3712: a suspended turn drops its live opener, so the child's next attempt never routes; un-ignore with the FIG-3712 fix"]
async fn a_child_attempt_after_its_turn_was_suspended_still_finishes_the_turn() {
    let turn = start_turn(ServerConfig::default().time(TimeMode::Manual), false).await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Idle past the inactivity timeout: the turn, waiting on its child's
    // settlement, is suspended. The child is inside its tool call, not
    // waiting on the server, so it keeps running.
    turn.backend.server().advance(Duration::from_secs(61));
    turn.turn_suspended().await;
    // The child's attempt dies, and the one that replaces it starts while
    // the turn is suspended.
    assert!(turn.backend.server().crash(&child), "the child is running");
    turn.gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
    let server = turn.backend.server().clone();
    let ticker = tokio::spawn(async move {
        loop {
            server.advance(Duration::from_secs(1));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let answer = turn.finish(Duration::from_secs(20)).await;
    ticker.abort();
    assert_eq!(answer, "done");
}

/// With every await suspending the handler that makes it (the
/// `INACTIVITY_TIMEOUT=0s` mode), the turn is suspended before its tool child
/// ever starts. The turn must still finish with the tool's answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-3712: a suspended turn drops its live opener, so the child's next attempt never routes; un-ignore with the FIG-3712 fix"]
async fn a_tool_turn_finishes_when_every_await_suspends_its_handler() {
    let turn = start_turn(ServerConfig::default().always_replay(true), true).await;
    let answer = turn.finish(Duration::from_secs(20)).await;
    assert_eq!(answer, "done");
}
