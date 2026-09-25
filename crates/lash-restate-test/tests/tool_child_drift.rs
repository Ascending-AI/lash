//! A group tool child whose tool drifted, redeployed while the child was in
//! flight (FIG-3725).
//!
//! Each law runs a real tool turn and holds its tool child mid-flight: the
//! child's first effect, or its orchestrating body, waits on a gate. The turn
//! is then suspended, so no opener is live, and the deployment is replaced by
//! one whose tool drifted (another retry policy). The child's attempt dies,
//! and its next attempt runs on a context the new deployment builds (the
//! FIG-3712 rebuilt path). Every such attempt must be served only from the
//! child's journal: what it needs live refuses, dispatches nothing, and parks
//! the turn, keyed by its turn, with nothing journaled after the refused run,
//! so each retry refuses again the same way. Restoring the tool finishes the
//! turn.

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

const DISPATCH: &str = "EffectGroupDispatch";
const PROBE: &str = "probe";
const ORCH: &str = "orch";
const SESSION: &str = "tool-child-drift";
const TURN: &str = "turn-1";

/// What the orchestrating tool's body does once its gate opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Body {
    /// Two nested `probe` calls, one after the other.
    TwoNestedCalls,
    /// Starts one external process.
    StartsProcess,
}

/// Which tool the model calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Called {
    Probe,
    Orch(Body),
}

fn definition(name: &str, drifted: bool) -> lash_core::ToolDefinition {
    let mut definition = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A drift-probe tool.",
        json!({ "type": "object", "additionalProperties": true }),
        json!({ "type": "object", "additionalProperties": true }),
    );
    if drifted {
        definition.manifest.retry_policy = lash_core::ToolRetryPolicy::safe(3, 10, 100);
    }
    definition
}

/// What every deployment shares: the gates and the counts the laws read.
#[derive(Clone)]
struct World {
    /// Held closed on the first deployment: the probe's first attempt, or the
    /// orchestrating body, waits here until the child dies.
    gate: Arc<tokio::sync::Semaphore>,
    probe_executions: Arc<AtomicUsize>,
    bodies: Arc<AtomicUsize>,
}

impl Default for World {
    fn default() -> Self {
        Self {
            gate: Arc::new(tokio::sync::Semaphore::new(0)),
            probe_executions: Arc::default(),
            bodies: Arc::default(),
        }
    }
}

struct ProbeTool {
    world: World,
    drifted: bool,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ProbeTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![definition(PROBE, self.drifted).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == PROBE).then(|| Arc::new(definition(PROBE, self.drifted).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world.probe_executions.fetch_add(1, Ordering::SeqCst);
        let _permit = self
            .world
            .gate
            .acquire()
            .await
            .expect("the gate never closes");
        lash_core::ToolOutcome::ok(json!({ "probed": true })).into()
    }
}

struct OrchTool {
    world: World,
    drifted: bool,
    body: Body,
}

#[async_trait::async_trait]
impl lash_core::facade_support::OrchestratingToolImplementation for OrchTool {
    fn manifest(&self) -> lash_core::ToolManifest {
        definition(ORCH, self.drifted).manifest()
    }

    fn contract(&self) -> Arc<lash_core::ToolContract> {
        Arc::new(definition(ORCH, self.drifted).contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &lash_core::facade_support::OrchestrationContext<'_>,
    ) -> lash_core::ToolOutcome {
        self.world.bodies.fetch_add(1, Ordering::SeqCst);
        drop(
            self.world
                .gate
                .acquire()
                .await
                .expect("the gate never closes"),
        );
        match self.body {
            Body::TwoNestedCalls => {
                let mut replies = Vec::new();
                for call in ["nested-1", "nested-2"] {
                    replies.extend(
                        context
                            .call_tool_batch(vec![lash_core::facade_support::ToolInvocation::new(
                                call,
                                lash_core::ToolId::from(format!("tool:{PROBE}")),
                                json!({}),
                            )])
                            .await,
                    );
                }
                lash_core::ToolOutcome::ok(json!({ "replies": replies.len() }))
            }
            Body::StartsProcess => {
                let started = lash_core::ProcessId::from(format!(
                    "{}-started",
                    context.tool_call_id().unwrap_or(ORCH)
                ));
                match context
                    .start_process(lash_core::ProcessStartRequest::external(
                        started,
                        lash_core::ProcessOriginator::host(),
                        json!({ "lane": "drift" }),
                        lash_core::ProcessLifecyclePolicy::new(
                            lash_core::ParentScope::Host,
                            lash_core::OnParentEnd::Abandon,
                        ),
                    ))
                    .await
                {
                    Ok(view) => lash_core::ToolOutcome::ok(json!({ "started": view.process_id })),
                    Err(error) => lash_core::ToolOutcome::err_fmt(format!("{error}")),
                }
            }
        }
    }
}

#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this test owns the tool contract it registers"
)]
fn orch_plugin(
    world: &World,
    drifted: bool,
    body: Body,
) -> Arc<lash::plugins::StaticPluginFactory> {
    let implementation: Arc<dyn lash_core::facade_support::OrchestratingToolImplementation> =
        Arc::new(OrchTool {
            world: world.clone(),
            drifted,
            body,
        });
    // SAFETY: this test owns the `orch` contract and its body.
    let definition = unsafe {
        lash_core::facade_support::OrchestratingToolDef::from_first_party(implementation)
    };
    Arc::new(lash::plugins::StaticPluginFactory::new(
        "tool-child-drift-orch",
        lash_core::facade_support::PluginSpec::new().with_orchestrating_tool(definition),
    ))
}

/// Asks for the called tool once, then answers `done` once a result is in.
fn model_reply(request: &LlmRequest, called: Called) -> LlmResponse {
    let answered = request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .any(|block| {
            matches!(
                block,
                lash_core::llm::types::LlmContentBlock::ToolResult { .. }
            )
        });
    let part = if answered {
        LlmOutputPart::Text {
            text: "done".into(),
            response_meta: None,
        }
    } else {
        LlmOutputPart::ToolCall {
            call_id: "call-1".into(),
            tool_name: match called {
                Called::Probe => PROBE.to_owned(),
                Called::Orch(_) => ORCH.to_owned(),
            },
            input_json: "{}".into(),
            replay: None,
        }
    };
    LlmResponse {
        parts: vec![part],
        response_metadata: Default::default(),
        ..Default::default()
    }
}

/// One deployment: a core over the shared backend and its open session.
struct Deployment {
    _core: lash::LashCore,
    session: lash::LashSession,
}

async fn deploy(
    backend: &RestateTestBackend,
    world: &World,
    called: Called,
    drifted: bool,
) -> Deployment {
    let provider = lash_core::testing::TestProvider::builder()
        .kind("tool-child-drift")
        .complete(move |request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(model_reply(&request, called))
        })
        .build()
        .into_handle();
    let mut builder =
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
            .tools(Arc::new(ProbeTool {
                world: world.clone(),
                drifted: drifted && called == Called::Probe,
            }) as Arc<dyn lash_core::ToolProvider>);
    if let Called::Orch(body) = called {
        builder = builder.plugin(orch_plugin(world, drifted, body));
    }
    let core = builder
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "tool-child-drift",
        ))
        .expect("build the lash core");
    let session = core
        .session(SESSION)
        .open()
        .await
        .expect("open the session");
    Deployment {
        _core: core,
        session,
    }
}

/// A tool turn on a fresh backend, with its deployment replaceable.
struct Turn {
    backend: RestateTestBackend,
    world: World,
    called: Called,
    /// The deployment every run of the turn's handler runs on.
    live: Arc<Mutex<Option<Deployment>>>,
    answer: Arc<Mutex<Option<String>>>,
    run: tokio::task::JoinHandle<Result<(), String>>,
}

async fn start_turn(called: Called) -> Turn {
    let backend =
        lash_restate_test::backend(0x3725, ServerConfig::default().time(TimeMode::Manual))
            .await
            .expect("build the Restate test backend");
    let world = World::default();
    let first = deploy(&backend, &world, called, false).await;
    let admitted =
        lash_core::AdmittedScope::unpinned(first.session.turn_scope(lash::TurnId::from(TURN)))
            .expect("admit the turn scope");
    let live = Arc::new(Mutex::new(Some(first)));
    let answer = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let live = Arc::clone(&live);
        let answer = Arc::clone(&answer);
        Arc::new(move |scoped| {
            let session = live
                .lock()
                .unwrap()
                .as_ref()
                .map(|deployment| deployment.session.clone());
            let answer = Arc::clone(&answer);
            Box::pin(async move {
                let Some(session) = session else {
                    // Between deployments: nothing serves the turn, as a
                    // redeploy's gap between two builds does.
                    std::future::pending::<()>().await;
                    return;
                };
                let output = session
                    .turn(lash::TurnInput::text("call the tool"))
                    .turn_id(lash::TurnId::from(TURN))
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
        world,
        called,
        live,
        answer,
        run,
    }
}

impl Turn {
    /// The tool child's dispatch invocation, once it exists.
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

    /// Holds the child mid-flight on the first deployment, suspends the turn,
    /// redeploys with the tool drifted, and kills the child's attempt: its
    /// next attempt runs on a context the drifted deployment builds.
    async fn redeploy_drifted_under_the_child(&self) -> String {
        let child = self.tool_child().await;
        let in_flight = || match self.called {
            Called::Probe => self.world.probe_executions.load(Ordering::SeqCst),
            Called::Orch(_) => self.world.bodies.load(Ordering::SeqCst),
        };
        while in_flight() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.backend.server().advance(Duration::from_secs(61));
        self.turn_suspended().await;
        self.redeploy(true).await;
        self.world
            .gate
            .add_permits(tokio::sync::Semaphore::MAX_PERMITS);
        assert!(self.backend.server().crash(&child), "the child is running");
        child
    }

    /// Replaces the deployment: the old one is dropped first, so exactly one
    /// deployment's context source is installed.
    async fn redeploy(&self, drifted: bool) {
        drop(self.live.lock().unwrap().take());
        let next = deploy(&self.backend, &self.world, self.called, drifted).await;
        *self.live.lock().unwrap() = Some(next);
    }

    async fn turn_suspended(&self) {
        loop {
            if self.backend.server().invocations().into_iter().any(|view| {
                view.target.starts_with("LashTestHandlerHost") && view.status == "suspended"
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The session's park, once one with at least `attempts` refusals is
    /// recorded, moving virtual time on so the child retries.
    async fn park_with(&self, attempts: u32) -> lash_core::store::TurnPark {
        let store = lash_core::StoreSet::session_store_factory(self.backend.stores().as_ref())
            .open_existing_store_by_id(&lash_core::SessionId::from(SESSION))
            .await
            .expect("open the session's store")
            .expect("the session has a store");
        let parked = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(park) = store
                    .load_turn_park(&lash_core::SessionId::from(SESSION))
                    .await
                    .expect("read the park")
                    .filter(|park| park.attempts >= attempts)
                {
                    return park;
                }
                self.backend.server().advance(Duration::from_secs(1));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        parked.unwrap_or_else(|_| {
            panic!(
                "the turn parked {attempts} times: {:#?}",
                self.backend.server().invocations()
            )
        })
    }

    /// The child's attempts never failed on a journal mismatch: nothing was
    /// journaled after its refused run.
    fn assert_no_journal_mismatch(&self, child: &str) {
        let view = self
            .backend
            .server()
            .invocations()
            .into_iter()
            .find(|view| view.id == child)
            .expect("the child's invocation exists");
        if let Some((code, message)) = &view.last_failure {
            assert!(
                *code != 570 && !message.contains("JOURNAL_MISMATCH"),
                "the child's retry failed on its journal: {code} {message}"
            );
        }
    }

    fn assert_parked_on_drift(park: &lash_core::store::TurnPark, tool: &str) {
        assert_eq!(
            park.turn_id.as_str(),
            TURN,
            "the park is the child's turn's"
        );
        let lash_core::store::ParkReason::BindingDrift { message } = &park.reason else {
            panic!("the park names the binding drift: {park:?}");
        };
        assert!(
            message.contains(&format!("tool:{tool}")) && message.contains("changed"),
            "the park names the drifted tool: {message}"
        );
    }

    /// Restores the tool and lets the turn finish.
    async fn restore_and_finish(self) -> (String, World, Option<lash_core::store::TurnPark>) {
        self.redeploy(false).await;
        let server = self.backend.server().clone();
        let ticker = tokio::spawn(async move {
            loop {
                server.advance(Duration::from_secs(1));
                for view in server.invocations() {
                    if view.status == "paused" {
                        server.resume(&view.id);
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let finished = tokio::time::timeout(Duration::from_secs(30), self.run).await;
        ticker.abort();
        let answer = match finished {
            Ok(Ok(Ok(()))) => self.answer.lock().unwrap().clone().unwrap_or_default(),
            other => format!(
                "stuck: {other:?}; open: {:#?}",
                self.backend.server().invocations()
            ),
        };
        let park = lash_core::StoreSet::session_store_factory(self.backend.stores().as_ref())
            .open_existing_store_by_id(&lash_core::SessionId::from(SESSION))
            .await
            .expect("open the session's store")
            .expect("the session has a store")
            .load_turn_park(&lash_core::SessionId::from(SESSION))
            .await
            .expect("read the park");
        (answer, self.world, park)
    }
}

/// A model-issued call whose attempt was in flight when its tool drifted: the
/// rebuilt child refuses its attempt at the live frontier, dispatches
/// nothing, and parks the turn under its turn id, again on every retry with
/// no journal mismatch. Restoring the tool runs the call and finishes the
/// turn, which clears the park.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuilt_child_on_a_drifted_tool_parks_its_turn_until_the_tool_is_restored() {
    let turn = start_turn(Called::Probe).await;
    let child = turn.redeploy_drifted_under_the_child().await;
    Turn::assert_parked_on_drift(&turn.park_with(1).await, PROBE);
    Turn::assert_parked_on_drift(&turn.park_with(2).await, PROBE);
    turn.assert_no_journal_mismatch(&child);
    assert_eq!(
        turn.world.probe_executions.load(Ordering::SeqCst),
        1,
        "the drifted deployment dispatched nothing"
    );
    let (answer, world, park) = turn.restore_and_finish().await;
    assert_eq!(answer, "done");
    assert_eq!(
        world.probe_executions.load(Ordering::SeqCst),
        2,
        "the restored tool runs the call once"
    );
    assert!(park.is_none(), "the finished turn is not parked: {park:?}");
}

/// A drifted orchestrating child whose body issues two nested calls: the first
/// refuses at the live frontier, and the second is refused before it reaches
/// the engine, so the refused run is the last thing the attempt journals.
/// Every retry refuses and parks cleanly, with no journal mismatch, and the
/// restored tool finishes the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drifted_orchestrating_child_parks_cleanly_after_its_first_refused_call() {
    let turn = start_turn(Called::Orch(Body::TwoNestedCalls)).await;
    let child = turn.redeploy_drifted_under_the_child().await;
    Turn::assert_parked_on_drift(&turn.park_with(1).await, ORCH);
    Turn::assert_parked_on_drift(&turn.park_with(3).await, ORCH);
    turn.assert_no_journal_mismatch(&child);
    assert_eq!(
        turn.world.probe_executions.load(Ordering::SeqCst),
        0,
        "no nested call was dispatched"
    );
    let (answer, world, park) = turn.restore_and_finish().await;
    assert_eq!(answer, "done");
    assert_eq!(world.probe_executions.load(Ordering::SeqCst), 2);
    assert!(park.is_none(), "the finished turn is not parked: {park:?}");
}

/// A drifted orchestrating child whose body starts a process: the process
/// command reaches Restate served only, refuses before it acts, and the turn
/// parks with no process started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drifted_orchestrating_child_starts_no_process() {
    let turn = start_turn(Called::Orch(Body::StartsProcess)).await;
    let child = turn.redeploy_drifted_under_the_child().await;
    Turn::assert_parked_on_drift(&turn.park_with(2).await, ORCH);
    turn.assert_no_journal_mismatch(&child);
    let started = lash_core::ProcessId::from("call-1-started");
    assert!(
        lash_core::StoreSet::process_registry(turn.backend.stores().as_ref())
            .get_process(&started)
            .await
            .expect("read the registry")
            .is_none(),
        "the drifted child started no process"
    );
    assert!(
        turn.backend
            .server()
            .invocations()
            .iter()
            .all(|view| !view.target.contains(started.as_str())),
        "the drifted child submitted no process workflow"
    );
}
