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
//! Before FIG-3712 a group tool child could only run against its opener's
//! in-process dispatch context, found in the `LiveOpenerRegistry`, which a
//! handler suspension drops; every child attempt that started while the turn
//! was suspended answered `no executor currently routes … tool child` until
//! the engine paused it. A child with no live opener now builds its context
//! from the deployment's `ToolChildContextSource`.

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

/// One tool turn on a fresh backend: its handler, its tool gate, and where it
/// records its answer.
struct Turn {
    /// Held for the turn's life, as a deployment holds its core: the core's
    /// wiring is what a child builds its context from when its turn is
    /// suspended.
    _core: lash::LashCore,
    session: lash::LashSession,
    backend: RestateTestBackend,
    gate: Arc<tokio::sync::Semaphore>,
    executions: Arc<AtomicUsize>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    answer: Arc<Mutex<Option<String>>>,
    /// The activities the turn reported on its last run.
    activities: Arc<Mutex<Vec<lash::TurnActivity>>>,
    run: tokio::task::JoinHandle<Result<(), String>>,
}

async fn start_turn(config: ServerConfig, gate_open: bool, via_batch: bool) -> Turn {
    start_turn_with(TurnOptions {
        config,
        gate_open,
        via_batch,
        fail_first: false,
        hold_redrive: None,
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
    /// Every attempt of the turn's handler after its first waits for a permit
    /// here before it runs the turn, so the turn stays not live meanwhile.
    hold_redrive: Option<Arc<tokio::sync::Semaphore>>,
}

async fn start_turn_with(options: TurnOptions) -> Turn {
    let TurnOptions {
        config,
        gate_open,
        via_batch,
        fail_first,
        hold_redrive,
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
    let activities = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempt: lash_restate_test::HandlerAttempt = {
        let session = session.clone();
        let answer = Arc::clone(&answer);
        let activities = Arc::clone(&activities);
        Arc::new(move |scoped| {
            let session = session.clone();
            let turn_id = turn_id.clone();
            let answer = Arc::clone(&answer);
            let activities = Arc::clone(&activities);
            let redrive = attempts.fetch_add(1, Ordering::SeqCst) > 0;
            let hold = hold_redrive.clone().filter(|_| redrive);
            Box::pin(async move {
                if let Some(hold) = hold {
                    let _permit = hold.acquire().await.expect("the hold never closes");
                }
                let output = session
                    .turn(lash::TurnInput::text("call the gated tool"))
                    .turn_id(turn_id)
                    .advanced()
                    .run_with_scope(scoped)
                    .await;
                *answer.lock().unwrap() = Some(match output {
                    Ok(output) => {
                        *activities.lock().unwrap() = output.activities.clone();
                        output
                            .result
                            .assistant_message()
                            .map_or_else(|| format!("{:?}", output.result.outcome), str::to_owned)
                    }
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
        _core: core,
        session,
        backend,
        gate,
        executions,
        stopped,
        answer,
        activities,
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
            Ok(Ok(Err(error))) => {
                format!(
                    "stuck: {error}; the turn last answered {:?}",
                    self.answer.lock().unwrap().clone()
                )
            }
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

    /// Whether the turn is not live anywhere, read once: its handler is
    /// suspended, or a redrive has started and is parked on the test's
    /// hold, never running the turn.
    fn is_held(&self) -> bool {
        self.backend.server().invocations().into_iter().any(|view| {
            view.target.starts_with(TURN_HOST) && (view.status == "suspended" || view.attempts >= 2)
        })
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
    /// that is blocked on the server is parked; so is any invocation that
    /// is not running at all (suspended, waiting on a timer). A held
    /// redrive parks on the test's hold rather than on the server, so it
    /// never reads parked this way — callers that count it must say so.
    fn is_parked(view: &lash_restate_test::InvocationView) -> bool {
        view.status != "running" || view.blocked_on_server == Some(true)
    }

    /// Waits until the turn's invocation is parked: suspended, a redrive
    /// held on the test's gate, or its live attempt blocked on the server.
    /// Only a time advance or outside input can move it from there.
    async fn turn_parked(&self) {
        loop {
            if self.backend.server().invocations().into_iter().any(|view| {
                view.target.starts_with(TURN_HOST) && (view.attempts >= 2 || Self::is_parked(&view))
            }) {
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
async fn a_child_attempt_after_its_turn_was_suspended_still_finishes_the_turn() {
    let turn = start_turn(ServerConfig::default().time(TimeMode::Manual), false, false).await;
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
async fn a_tool_turn_finishes_when_every_await_suspends_its_handler() {
    let turn = start_turn(ServerConfig::default().always_replay(true), true, false).await;
    let answer = turn.finish(Duration::from_secs(20)).await;
    assert_eq!(answer, "done");
}

/// The same suspension, with the tool reached through `batch`: the batch
/// child, running with no live turn, has no stream to send its nested call's
/// events to. They are recorded on its settlement and reach the turn when it
/// incorporates the child, never dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_child_with_no_live_turn_records_its_nested_events_for_the_turn() {
    let turn = start_turn(ServerConfig::default().time(TimeMode::Manual), false, true).await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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
    let activities = Arc::clone(&turn.activities);
    let answer = turn.finish(Duration::from_secs(20)).await;
    ticker.abort();
    assert_eq!(answer, "done");
    let nested: Vec<_> = activities
        .lock()
        .unwrap()
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::ToolCallCompleted {
                name,
                parent_call_id: Some(parent),
                ..
            } => Some((name.clone(), parent.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        nested,
        vec![(TOOL.to_owned(), "call-1".to_owned())],
        "the batch's nested call completed on the turn's stream, under the batch"
    );
}

/// FIG-3672 P9's durable turn cancel reaches a child running on a context the
/// deployment built, at the child's next durable wait.
///
/// The tool's first attempt fails and asks for its retry an hour later, so
/// the child sleeps durably, and both it and its turn are suspended. The turn
/// is then cancelled through its durable gate only, and every redrive of the
/// turn's handler is held, so the turn is not live anywhere while the child
/// resumes: the child runs on a built context, whose token nothing local
/// signals. Its retry sleep must still lose to the cancel, so the tool never
/// runs again. The child's attempt ends there as a live fault, never settled;
/// what ends the child is its turn's group close, once the turn runs again
/// and sees its own cancel, exactly as on a live opener.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_durable_turn_cancel_reaches_a_rebuilt_child_at_its_next_wait() {
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let turn = start_turn_with(TurnOptions {
        config: ServerConfig::default().time(TimeMode::Manual),
        gate_open: true,
        via_batch: false,
        fail_first: true,
        hold_redrive: Some(Arc::clone(&hold)),
    })
    .await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Idle past the inactivity timeout until the turn, waiting on its child,
    // and the child, in its hour-long retry sleep, are both suspended. The
    // server closes a starved attempt's input on a time advance, and an
    // attempt reads starved only once its task has parked on its input —
    // which a loaded executor may schedule late — so each advance waits for
    // both invocations to be parked before it checks and moves time again:
    // every advance that does not suspend them is still spent against the
    // inactivity timeout, and forty minute-long advances stay far short of
    // the retry without a wall-clock window anywhere.
    let mut suspended = false;
    for _ in 0..40 {
        turn.turn_parked().await;
        turn.invocation_parked(&child).await;
        if turn.is_held() && turn.has_status(&child, "suspended") {
            suspended = true;
            break;
        }
        turn.backend.server().advance(Duration::from_secs(61));
    }
    assert!(
        suspended,
        "the turn and its child are both suspended: {:#?}",
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

    turn.session
        .request_turn_cancel(&lash::TurnId::from("turn-1"), "stop", None, None)
        .await
        .expect("the durable cancel is accepted");

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
    hold.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
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
/// the tool when the turn is suspended; that attempt dies, and every redrive
/// of the turn is held, so the attempt that replaces it runs on a built
/// context. The turn is then cancelled through its durable gate only. The
/// tool, waiting on its own gate, must see its token fire and answer
/// cancelled, and the turn must end cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuilt_childs_tool_sees_its_turns_durable_cancel_as_its_token() {
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let turn = start_turn_with(TurnOptions {
        config: ServerConfig::default().time(TimeMode::Manual),
        gate_open: false,
        via_batch: false,
        fail_first: false,
        hold_redrive: Some(Arc::clone(&hold)),
    })
    .await;
    let child = turn.tool_child().await;
    while turn.executions.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Idle past the inactivity timeout until the turn is suspended or
    // held. Every advance waits for the turn to be parked before it checks
    // and moves time again — the suspend lands only after the turn's task
    // has parked on its input, which a loaded executor may schedule late.
    let mut suspended = false;
    for _ in 0..20 {
        turn.turn_parked().await;
        if turn.is_held() {
            suspended = true;
            break;
        }
        turn.backend.server().advance(Duration::from_secs(61));
    }
    assert!(suspended, "the turn is suspended");
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

    turn.session
        .request_turn_cancel(&lash::TurnId::from("turn-1"), "stop", None, None)
        .await
        .expect("the durable cancel is accepted");
    tokio::time::timeout(Duration::from_secs(20), async {
        while !turn.stopped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the rebuilt child's tool saw its token fire while its turn was held");

    hold.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
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
