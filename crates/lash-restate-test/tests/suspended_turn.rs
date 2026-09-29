//! A tool turn whose handler is suspended while its tool child still needs an
//! attempt (FIG-3712).
//!
//! Restate suspends a handler that waits on the server past its inactivity
//! timeout; its in-process state, the turn's live opener included, goes with
//! it. A group tool child runs against its opener's live context, so a child
//! attempt that starts while the turn is suspended has no opener to run
//! against. The turn is waiting on that child's settlement and the child on a
//! live turn: each law here sends a real tool turn, which the engine drives in
//! its root's `LashTurn` workflow, puts it into that state and requires it to
//! finish anyway.
//!
//! Before FIG-3712 a group tool child could only run against its opener's
//! in-process dispatch context, found in the `LiveOpenerRegistry`, which a
//! handler suspension drops; every child attempt that started while the turn
//! was suspended answered `no executor currently routes … tool child` until
//! the engine paused it. A child with no live opener now builds its context
//! from the deployment's `ToolChildContextSource`.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::{RestateTestBackend, ServerConfig, TURN_DRIVER_SERVICE, TimeMode};
use serde_json::json;

const SESSION: &str = "suspended-turn";
const ROOT: &str = "turn-1";
const DISPATCH: &str = "EffectGroupDispatch";
const TOOL: &str = "gated_call";

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..Default::default()
    }
}

/// Asks for the tool once, directly or through `batch`, then answers `done`
/// once it sees the result.
fn model_reply(request: &LlmRequest, via_batch: bool) -> LlmResponse {
    let saw_tool_result = serde_json::to_string(&request.messages)
        .unwrap_or_default()
        .contains("gated result");
    if saw_tool_result {
        response(vec![LlmOutputPart::Text {
            text: "done".into(),
            response_meta: None,
        }])
    } else {
        let (tool_name, input) = if via_batch {
            (
                "batch".to_owned(),
                json!({"tool_calls": [{"tool": TOOL, "parameters": {}}]}),
            )
        } else {
            (TOOL.to_owned(), json!({}))
        };
        response(vec![LlmOutputPart::ToolCall {
            call_id: "call-1".into(),
            tool_name,
            input_json: input.to_string(),
            replay: None,
        }])
    }
}

/// How long the tool's first attempt asks to wait before its retry, when it
/// fails first: far past anything the test advances the clock by.
const RETRY_AFTER_MS: u64 = 3_600_000;

fn tool_definition(retries: bool) -> lash_core::ToolDefinition {
    let mut definition = lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Answer once the test opens the gate.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
    );
    if retries {
        definition.manifest.retry_policy =
            lash_core::ToolRetryPolicy::safe(3, RETRY_AFTER_MS, RETRY_AFTER_MS);
    }
    definition
}

/// A tool that waits for the test to open its gate before it answers, so the
/// test decides what the turn's handler goes through meanwhile. When
/// `fail_first` is set, its first attempt fails retryably instead and asks
/// for its retry an hour later.
struct GatedTool {
    executions: Arc<AtomicUsize>,
    gate: Arc<tokio::sync::Semaphore>,
    fail_first: bool,
    /// Set when an attempt saw its cancellation token fire while it waited
    /// for the gate; the attempt then answers cancelled.
    stopped: Arc<std::sync::atomic::AtomicBool>,
    /// The tool never watches its cancellation token: only dropping its
    /// attempt stops it.
    ignores_cancel: bool,
    /// Set when an attempt that was still waiting for the gate is dropped.
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

/// Marks its flag when dropped: an attempt dropped mid-wait.
struct DropWitness(Arc<std::sync::atomic::AtomicBool>);

impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for GatedTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition(self.fail_first).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition(self.fail_first).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let execution = self.executions.fetch_add(1, Ordering::SeqCst);
        if self.fail_first && execution == 0 {
            return lash_core::ToolOutcome::retryable_failure(
                lash_core::ToolFailureClass::External,
                "transient",
                "not yet; ask again in an hour",
                Some(RETRY_AFTER_MS),
            )
            .into();
        }
        // Held open for the rest of the test once the gate opens, so a
        // re-executed attempt answers at once.
        let stop = call
            .context
            .cancellation_token()
            .cloned()
            .unwrap_or_default();
        if self.ignores_cancel {
            let witness = DropWitness(Arc::clone(&self.dropped));
            let _permit = self.gate.acquire().await.expect("the gate never closes");
            std::mem::forget(witness);
            return lash_core::ToolOutcome::ok(json!({"result": "gated result"})).into();
        }
        tokio::select! {
            permit = self.gate.acquire() => {
                let _permit = permit.expect("the gate never closes");
                lash_core::ToolOutcome::ok(json!({"result": "gated result"})).into()
            }
            () = stop.cancelled() => {
                self.stopped.store(true, Ordering::SeqCst);
                lash_core::ToolOutcome::cancelled("the turn asked the tool to stop").into()
            }
        }
    }
}

/// One tool turn on a fresh backend: its session, its tool gate, and the
/// handle its answer arrives on.
struct Turn {
    /// Held for the turn's life, as a deployment holds its core: the core's
    /// wiring is what a child builds its context from when its turn is
    /// suspended, and its driver is the one the engine runs the turn on.
    _core: lash::LashCore,
    session: lash::LashSession,
    backend: RestateTestBackend,
    gate: Arc<tokio::sync::Semaphore>,
    executions: Arc<AtomicUsize>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
    /// The sent input's settled turn.
    run: tokio::task::JoinHandle<lash::Result<lash::TurnOutput>>,
}

async fn start_turn(config: ServerConfig, gate_open: bool, via_batch: bool) -> Turn {
    start_turn_with(TurnOptions {
        config,
        gate_open,
        via_batch,
        fail_first: false,
        ignores_cancel: false,
    })
    .await
}

/// How a test's tool turn is set up.
struct TurnOptions {
    config: ServerConfig,
    /// The tool answers as soon as it runs.
    gate_open: bool,
    /// The model reaches the tool through `batch`.
    via_batch: bool,
    /// The tool's first attempt fails retryably and asks for its retry an
    /// hour later.
    fail_first: bool,
    /// The tool never watches its cancellation token.
    ignores_cancel: bool,
}

async fn start_turn_with(options: TurnOptions) -> Turn {
    let TurnOptions {
        config,
        gate_open,
        via_batch,
        fail_first,
        ignores_cancel,
    } = options;
    let backend = lash_restate_test::backend(0x3712, config)
        .await
        .expect("build the Restate test backend");
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    if gate_open {
        gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
    }
    let executions = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("suspended-turn")
        .complete(move |request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(model_reply(&request, via_batch))
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
            .tools(Arc::new(GatedTool {
                executions: Arc::clone(&executions),
                stopped: Arc::clone(&stopped),
                gate: Arc::clone(&gate),
                fail_first,
                ignores_cancel,
                dropped: Arc::clone(&dropped),
            }) as Arc<dyn lash_core::ToolProvider>)
            .build(lash_core::LeaseOwnerIdentity::opaque(
                "lash-restate-test",
                "suspended-turn",
            ))
            .expect("build the lash core");
    let session = created_session(&core, SESSION)
        .await
        .open()
        .await
        .expect("open the session");
    let handle = session
        .send(lash::TurnInput::text("call the gated tool"))
        .id(ROOT)
        .await
        .expect("accept the turn input");
    let run = tokio::spawn(handle.output());
    Turn {
        _core: core,
        session,
        backend,
        gate,
        executions,
        stopped,
        dropped,
        run,
    }
}

impl Turn {
    /// Waits for the sent turn to settle, or reports every invocation still
    /// open after `budget` of wall time.
    async fn finish(self, budget: Duration) -> String {
        self.finish_with_activities(budget).await.0
    }

    /// [`finish`](Self::finish), with the activities the settled turn
    /// reported.
    async fn finish_with_activities(self, budget: Duration) -> (String, Vec<lash::TurnActivity>) {
        let server = self.backend.server().clone();
        let answer = match tokio::time::timeout(budget, self.run).await {
            Ok(Ok(Ok(output))) => {
                return (
                    output
                        .result
                        .assistant_message()
                        .map_or_else(|| format!("{:?}", output.result.outcome), str::to_owned),
                    output.activities,
                );
            }
            Ok(Ok(Err(error))) => format!("error: {error}"),
            Ok(Err(join)) => format!("the turn's task failed: {join}"),
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
        };
        (answer, Vec::new())
    }

    /// Hold the turn's root workflow: its running attempt stops at its next
    /// await, and no attempt of it starts until the hold is released, so the
    /// turn is live nowhere meanwhile.
    async fn hold_turn(&self) -> lash_restate_test::Hold {
        self.backend
            .server()
            .hold(
                TURN_DRIVER_SERVICE,
                &lash_restate::turn_workflow_key(
                    &lash_core::SessionId::from(SESSION),
                    &lash::TurnId::from(ROOT),
                ),
            )
            .await
    }

    /// Cancel the turn's root through its durable gate.
    async fn cancel_root(&self) {
        self.session
            .cancel(lash::CancelTarget::Root(lash::TurnId::from(ROOT)))
            .reason("stop")
            .await
            .expect("the durable cancel is accepted");
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

    /// Whether invocation `id` reports `status`, read once.
    fn has_status(&self, id: &str, status: &str) -> bool {
        self.backend
            .server()
            .invocations()
            .into_iter()
            .any(|view| view.id == id && view.status == status)
    }

    /// Whether the invocation `view` reports is parked: it can make no
    /// progress until the server moves time or feeds it. A live attempt
    /// whose open input the SDK waits on is parked; so is any invocation
    /// that is not running at all (suspended, waiting on a timer). An
    /// attempt whose input the server has closed but whose task has not
    /// drained the close yet is not parked — it still has the suspension
    /// to run.
    fn is_parked(view: &lash_restate_test::InvocationView) -> bool {
        view.status != "running" || view.blocked_on_server == Some(true)
    }

    /// Waits until the turn's root workflow is parked: suspended, held, or
    /// its live attempt blocked on the server. Only a time advance or outside
    /// input can move it from there.
    async fn turn_parked(&self) {
        loop {
            if self
                .backend
                .server()
                .invocations()
                .into_iter()
                .any(|view| view.target.starts_with(TURN_DRIVER_SERVICE) && Self::is_parked(&view))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until invocation `id` is parked.
    async fn invocation_parked(&self, id: &str) {
        loop {
            if self
                .backend
                .server()
                .invocations()
                .into_iter()
                .any(|view| view.id == id && Self::is_parked(&view))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until invocation `id`'s last failure contains `message`.
    async fn invocation_failed_with(&self, id: &str, message: &str) {
        loop {
            if self.backend.server().invocations().into_iter().any(|view| {
                view.id == id
                    && view
                        .last_failure
                        .as_ref()
                        .is_some_and(|(_, failure)| failure.contains(message))
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until invocation `id` reports `status`.
    async fn invocation_reaches(&self, id: &str, status: &str) {
        while !self.has_status(id, status) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The commands the tool child's drive journaled, in order, from its
    /// read of its recorded environment on: what the child asked the engine
    /// to do. Each is its type and what it names and carries; the completion
    /// ids a command is numbered with are positions in the whole journal, so
    /// they are left out, as is the group-admission prefix before the drive,
    /// whose waits depend on when the child's dispatch overtook its opener's
    /// registration of the group, not on the context the child ran on.
    fn child_commands(&self, child: &str) -> Vec<(String, String, bytes::Bytes)> {
        use lash_restate_test::protocol::MessageType;
        use lash_restate_test::protocol::generated as pb;
        use prost::Message as _;
        self.backend
            .server()
            .journal(child)
            .expect("the child's invocation exists")
            .into_iter()
            .filter(|entry| entry.ty.is_command())
            .map(|entry| match entry.ty {
                MessageType::CallCommand => {
                    let call = pb::CallCommandMessage::decode(entry.payload)
                        .expect("a journaled call decodes");
                    (
                        "call".to_owned(),
                        format!("{}/{}/{}", call.service_name, call.key, call.handler_name),
                        call.parameter,
                    )
                }
                MessageType::RunCommand => {
                    let run = pb::RunCommandMessage::decode(entry.payload)
                        .expect("a journaled run decodes");
                    ("run".to_owned(), run.name, bytes::Bytes::new())
                }
                other => (format!("{other:?}"), String::new(), entry.payload),
            })
            .skip_while(|(kind, name, _)| !(kind == "run" && name.ends_with(":env")))
            .collect()
    }

    /// Waits until the turn's root workflow has been suspended.
    async fn turn_suspended(&self) {
        loop {
            if self.backend.server().invocations().into_iter().any(|view| {
                view.target.starts_with(TURN_DRIVER_SERVICE) && view.status == "suspended"
            }) {
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
async fn a_child_attempt_after_its_turn_was_suspended_still_finishes_the_turn() {
    let turn = start_turn(ServerConfig::default().time(TimeMode::Manual), false, false).await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Idle past the inactivity timeout: the turn, waiting on its child's
    // settlement, is suspended. The child is inside its tool call, not
    // waiting on the server, so it keeps running. The advance counts only
    // once the turn's task has parked on its input, so wait for that first.
    turn.turn_parked().await;
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
async fn a_tool_turn_finishes_when_every_await_suspends_its_handler() {
    let turn = start_turn(ServerConfig::default().always_replay(true), true, false).await;
    let answer = turn.finish(Duration::from_secs(20)).await;
    assert_eq!(answer, "done");
}

/// The same suspension, with the tool reached through `batch`: the member's
/// child, running with no live turn, has no stream to send its events to.
/// They are recorded on its settlement and reach the turn when it
/// incorporates the child, never dropped. A member is one of the step's flat
/// slots, `{wrapper}/batch/{i}` (ADR 0116 §2), so its completion is a slot's,
/// exactly as a live turn reports it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_child_with_no_live_turn_records_its_events_for_the_turn() {
    let turn = start_turn(ServerConfig::default().time(TimeMode::Manual), false, true).await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    turn.turn_parked().await;
    turn.backend.server().advance(Duration::from_secs(61));
    turn.turn_suspended().await;
    assert!(turn.backend.server().crash(&child), "the child is running");
    turn.gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
    let server = turn.backend.server().clone();
    let ticker = tokio::spawn(async move {
        loop {
            server.advance(Duration::from_secs(1));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let (answer, activities) = turn.finish_with_activities(Duration::from_secs(20)).await;
    ticker.abort();
    assert_eq!(answer, "done");
    let completed: Vec<_> = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::ToolCallCompleted {
                name,
                output,
                provider_call_id,
                ..
            } => Some((name.clone(), provider_call_id.clone(), output.is_success())),
            _ => None,
        })
        .collect();
    // A batch member is lash's own call, a child of the wrapper's id: it
    // carries no provider correlation of its own (ADR 0117 §3).
    assert_eq!(
        completed,
        vec![(TOOL.to_owned(), None, true)],
        "the batch's member completed on the turn's stream, as the step's slot"
    );
}

/// FIG-3672 P9's durable turn cancel reaches a child running on a context the
/// deployment built, at the child's next durable wait.
///
/// The tool's first attempt fails and asks for its retry an hour later, so
/// the child sleeps durably. The turn's root workflow is held, so the turn is
/// not live anywhere, and the child is idled until it is suspended too. The
/// turn is then cancelled through its durable gate only: the child resumes on
/// a built context, whose token nothing local signals. Its retry sleep must
/// still lose to the cancel, so the tool never runs again. The child's
/// attempt ends there as a live fault, never settled; what ends the child is
/// its turn's group close, once the turn runs again and sees its own cancel,
/// exactly as on a live opener.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_durable_turn_cancel_reaches_a_rebuilt_child_at_its_next_wait() {
    let turn = start_turn_with(TurnOptions {
        config: ServerConfig::default().time(TimeMode::Manual),
        gate_open: true,
        via_batch: false,
        fail_first: true,
        ignores_cancel: false,
    })
    .await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let hold = turn.hold_turn().await;
    // Idle past the inactivity timeout until the child, in its hour-long
    // retry sleep, is suspended. The server closes a starved attempt's input
    // on a time advance, and an attempt reads starved only once its task has
    // parked on its input — which a loaded executor may schedule late — so
    // each advance waits for the child to be parked before it checks and
    // moves time again: every advance that does not suspend it is still
    // spent against the inactivity timeout, and forty minute-long advances
    // stay far short of the retry without a wall-clock window anywhere.
    let mut suspended = false;
    for _ in 0..40 {
        turn.invocation_parked(&child).await;
        if turn.has_status(&child, "suspended") {
            suspended = true;
            break;
        }
        turn.backend.server().advance(Duration::from_secs(61));
    }
    assert!(
        suspended,
        "the child is suspended: {:#?}",
        turn.backend
            .server()
            .invocations()
            .into_iter()
            .filter(|view| view.status != "completed")
            .map(|view| format!(
                "{} {} attempts={} last_failure={:?}",
                view.target, view.status, view.attempts, view.last_failure
            ))
            .collect::<Vec<_>>()
    );

    turn.cancel_root().await;

    // The turn is held, so only the child's rebuilt context can observe the
    // cancel: its retry sleep loses to the turn's durable gate. The gate's
    // resolution resuming the child and cancelling its sleep is work the
    // cancel's receipt already set in motion, so the law waits on the
    // child's recorded failure, not on a wall-clock window.
    turn.invocation_failed_with(&child, "runtime_effect_sleep_cancelled")
        .await;
    assert_eq!(
        turn.executions.load(Ordering::SeqCst),
        1,
        "the cancelled child never ran its retry"
    );

    let executions = Arc::clone(&turn.executions);
    hold.release();
    let answer = turn.finish(Duration::from_secs(20)).await;
    assert!(
        answer.contains("Cancel"),
        "the turn ends cancelled: {answer}"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the tool never ran again after its turn was cancelled"
    );
}

/// A rebuilt child's tool gets a stop its turn's durable gate fires, as a
/// live opener's children get the stop the opener's drive fires (FIG-3672
/// P9): a tool body that waits on nothing durable still sees the cancel.
///
/// The child's first attempt starts beside its live turn and is still inside
/// the tool when the turn's root workflow is held, so the turn is live
/// nowhere; that attempt dies, and the attempt that replaces it runs on a
/// built context. The turn is then cancelled through its durable gate only.
/// The tool, waiting on its own gate, must see its token fire and answer
/// cancelled, and the turn must end cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuilt_childs_tool_sees_its_turns_durable_cancel_as_its_token() {
    let turn = start_turn_with(TurnOptions {
        config: ServerConfig::default().time(TimeMode::Manual),
        gate_open: false,
        via_batch: false,
        fail_first: false,
        ignores_cancel: false,
    })
    .await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let hold = turn.hold_turn().await;
    assert!(turn.backend.server().crash(&child), "the child is running");
    let server = turn.backend.server().clone();
    let ticker = tokio::spawn(async move {
        loop {
            server.advance(Duration::from_secs(1));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        while turn.executions.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the replacement attempt runs the tool on a built context");

    turn.cancel_root().await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !turn.stopped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the rebuilt child's tool saw its token fire while its turn was held");

    hold.release();
    let answer = turn.finish(Duration::from_secs(20)).await;
    ticker.abort();
    assert!(
        answer.contains("Cancel"),
        "the turn ends cancelled: {answer}"
    );
}

/// A leaf tool child in an ordinary session asks the engine for the same
/// commands whether it ran on its live opener's context or on one the
/// deployment built: the rebuilt context changes where the child's context
/// comes from, never what the child does. The second run's turn is never
/// live while its child runs, so only a built context can serve the child
/// there (without one, that child is never routed; see
/// `a_tool_turn_finishes_when_every_await_suspends_its_handler`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leaf_child_records_the_same_commands_on_a_rebuilt_context_as_on_its_live_opener() {
    // The turn is live while its child runs.
    let live = start_turn(ServerConfig::default(), true, false).await;
    let live_child = live.tool_child().await;
    live.invocation_reaches(&live_child, "completed").await;
    let live_commands = live.child_commands(&live_child);
    assert_eq!(live.finish(Duration::from_secs(20)).await, "done");

    // Every await suspends the handler making it, so the turn is never live
    // while its child runs.
    let rebuilt = start_turn(ServerConfig::default().always_replay(true), true, false).await;
    let rebuilt_child = rebuilt.tool_child().await;
    rebuilt
        .invocation_reaches(&rebuilt_child, "completed")
        .await;
    let rebuilt_commands = rebuilt.child_commands(&rebuilt_child);
    assert_eq!(rebuilt.finish(Duration::from_secs(20)).await, "done");

    assert!(
        live_commands
            .iter()
            .any(|(kind, name, _)| kind == "run" && name.ends_with(":attempt:1")),
        "the live child's drive ran its tool: {live_commands:#?}"
    );
    assert_eq!(
        live_commands.len(),
        rebuilt_commands.len(),
        "live {live_commands:#?}\nrebuilt {rebuilt_commands:#?}"
    );
    for (index, (live, rebuilt)) in live_commands.iter().zip(&rebuilt_commands).enumerate() {
        assert_eq!(
            live, rebuilt,
            "command {index} differs between the live and the rebuilt child"
        );
    }
}

/// D20 (FIG-3904): a tool child whose group is cancelled while its attempt
/// runs replays exactly the journal its live run left.
///
/// The tool ignores its token, so only the child's own cancel can stop the
/// attempt. The turn is cancelled while the attempt runs; the turn's close
/// decides the child's cancel, and the child's attempt ends. The child's
/// invocation then dies before it stores its next command, and its redrive
/// replays the journal the cancelled run left: it issues the commands that
/// journal holds, never re-runs the attempt the cancel ended, and settles.
///
/// Red before D20: the child raced its drive against a live ingress watch and
/// dropped the drive mid-journal, leaving the attempt's run unrecorded; the
/// redrive, which finds no recorded result for that run, ran the tool again
/// before its own watch came back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_tool_child_issues_the_same_commands_as_its_journal() {
    use lash_restate_test::{CrashPoint, CrashRule};
    let turn = start_turn_with(TurnOptions {
        config: ServerConfig::default().time(TimeMode::Manual),
        gate_open: false,
        via_batch: false,
        fail_first: false,
        ignores_cancel: true,
    })
    .await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The attempt is inside the tool once the server stored its run command:
    // the child's next journal command is the first one it stores once the
    // cancel ends the attempt. Its attempt dies right there, once, and the
    // redrive replays what it left.
    let next = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let commands: Vec<_> = turn
                .backend
                .server()
                .journal(&child)
                .expect("the child's invocation exists")
                .into_iter()
                .filter(|entry| entry.ty.is_command())
                .collect();
            if commands
                .last()
                .and_then(|entry| entry.name.as_deref())
                .is_some_and(|name| name.contains(":attempt:"))
            {
                return commands.len();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the server stores the running attempt's command");
    // The child runs on its group's dispatch lane, a service of its own.
    let lane = turn
        .backend
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.id == child)
        .and_then(|view| view.target.split('/').next().map(str::to_owned))
        .expect("the child's dispatch lane");
    turn.backend.server().crash_on(
        CrashRule::new(CrashPoint::BeforeCommand { index: next })
            .service(lane)
            .handler("child")
            .times(1),
    );

    turn.cancel_root().await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !turn.dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the cancel ends the attempt of a tool that ignores its token");
    let server = turn.backend.server().clone();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !turn.has_status(&child, "completed") {
            server.advance(Duration::from_secs(1));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the cancelled child's redrive settles: {:#?}",
            server
                .invocations()
                .into_iter()
                .filter(|view| view.status != "completed")
                .map(|view| format!(
                    "{} {} attempts={} last_failure={:?}",
                    view.target, view.status, view.attempts, view.last_failure
                ))
                .collect::<Vec<_>>()
        )
    });
    let view = server
        .invocations()
        .into_iter()
        .find(|view| view.id == child)
        .expect("the child's invocation");
    assert!(
        view.attempts >= 2,
        "the child's attempt died and was redriven: {view:?}"
    );
    assert!(
        !view
            .last_failure
            .as_ref()
            .is_some_and(|(code, _)| *code == 570),
        "the redrive issued a command its journal does not hold: {view:?}"
    );
    assert_eq!(
        turn.executions.load(Ordering::SeqCst),
        1,
        "the redrive never re-ran the attempt the cancel ended"
    );

    let executions = Arc::clone(&turn.executions);
    let answer = turn.finish(Duration::from_secs(20)).await;
    assert!(
        answer.contains("Cancel"),
        "the turn ends cancelled: {answer}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1, "the tool ran once");
}

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
