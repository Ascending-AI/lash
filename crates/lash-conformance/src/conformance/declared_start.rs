//! ADR 0116 §7.3: a tool's `Pending` that declares its child start, and
//! `spawn_agent` on that shape.
//!
//! Each law drives one parent turn whose model calls `spawn_agent` natively
//! (or, for the `Promise.all` width, from an RLM cell). The child is a
//! `SessionTurn` process the tier's process worker runs; its model is the
//! law's own script, so a law can hold a child mid-turn, count its steps and
//! see which parent step saw which result. The laws take the subagent plugin
//! from the registering crate, which sits above this one.
//!
//! A crash is a [`ConformanceCrash`](crate::ConformanceCrash) the law fires
//! from a hook at the boundary it names: the crashing attempt dies at its
//! next await after the crash fired, and the tier redelivers the turn the way
//! it recovers a crashed turn.

use crate::admit;
use lash_core::testing::TestTurnDrive as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_sansio::{SessionId, TurnId};

/// What every law's parent turn is asked: the parent's requests carry it, and
/// a child's never do.
const PARENT_INPUT: &str = "declared-start law: spawn the children";

/// The definition key of the subagent child's `SessionTurn` process.
const SUBAGENT_DEFINITION: &str = "lash-subagent-session-turn";

/// How long a law waits for a fact a healthy run reaches in well under a
/// second, before it fails rather than hangs.
const PATIENCE: Duration = Duration::from_secs(60);

fn child_task(index: usize) -> String {
    format!("declared-start child {index}: answer your literal")
}

fn child_reply(index: usize) -> String {
    format!("declared-start child {index} literal")
}

/// The subagent plugin factory, under the pending deadline the law sets.
pub type SubagentPlugin =
    Arc<dyn Fn(Option<Duration>) -> Arc<dyn crate::facade_support::PluginFactory> + Send + Sync>;

/// What a registering tier hands every declared-start law.
#[derive(Clone)]
pub struct DeclaredStartTier {
    /// Distinguishes this tier's sessions from every other tier's.
    pub prefix: String,
    pub effect_host: Arc<dyn crate::EffectHost>,
    pub stores: Arc<dyn crate::StoreSet>,
    /// Runs the parent turn, crashes and redrives it, and serves the child's
    /// process segments.
    pub runner: Arc<dyn crate::ConformanceTurnRunner>,
    /// The RLM protocol plugin factories the `Promise.all` width runs under.
    pub rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    pub subagents: SubagentPlugin,
    /// The engine's process port, which a scope close delivers each `Until`
    /// child's `ParentEnded` cancel through.
    pub delivery: Arc<dyn crate::ProcessWorkSubstrate>,
}

/// A gate a scripted model step waits on until the law opens it.
#[derive(Clone)]
struct Gate(Arc<tokio::sync::watch::Sender<bool>>);

impl Gate {
    fn new(open: bool) -> Self {
        Self(Arc::new(tokio::sync::watch::channel(open).0))
    }

    fn open(&self) {
        self.0.send_replace(true);
    }

    async fn passed(&self) {
        let mut open = self.0.subscribe();
        let _ = open.wait_for(|open| *open).await;
    }
}

type Hook = Box<dyn FnOnce() + Send>;

/// How the parent's first step calls its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Producer {
    /// One native `spawn_agent` call per child in one model response, with
    /// `siblings` ordinary echo calls beside them.
    Native { siblings: usize },
    /// One RLM cell awaiting `Promise.all` over `agents.spawn`.
    PromiseAll,
    /// One call to the law's own declaring tool.
    Probe(Probe),
}

/// The tool name of [`DeclaringProbe`].
const PROBE_TOOL: &str = "conformance_declared_start_probe";

/// The metadata key a probe's external child carries its session under.
const PROBE_MARKER: &str = "declared_start_probe";

/// What a [`DeclaringProbe`] call declares: the call's arguments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Probe {
    /// The wait's cancel hint is [`CancelHint::Ignore`](crate::CancelHint::Ignore).
    ignore_cancel: bool,
    /// The child is `Detached` rather than living until its starter ends.
    detached: bool,
    /// The first attempt fails retryably, declaring a start as a completed
    /// intent; the retry declares the start and parks on it.
    fail_first: bool,
}

fn probe_tool() -> crate::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    crate::ToolDefinition::raw(
        "tool:conformance_declared_start_probe",
        PROBE_TOOL,
        "Declares one external child start and resolves on its terminal.",
        object.clone(),
        object,
    )
    .with_retry_policy(crate::ToolRetryPolicy::safe(2, 1, 1))
}

/// A tool whose `Pending` declares one externally owned child: nothing runs
/// it, so the law controls its lifetime, the wait's cancel hint and the
/// attempt that declares it.
struct DeclaringProbe;

impl DeclaringProbe {
    fn start(
        context: &crate::AttemptContext<'_>,
        probe: Probe,
    ) -> Result<crate::StartProcessIntent, String> {
        let session_id = SessionId::from(context.session_id());
        let lifetime = if probe.detached {
            crate::Lifetime::Detached
        } else {
            crate::lifetime::starter(&context.start_cx().map_err(|error| error.to_string())?)
        };
        let declaration = crate::ProcessStartDeclaration::external(
            crate::ProcessOriginator::Session {
                session_id: session_id.clone(),
                agent_frame_id: Some(context.agent_frame_id().clone()),
            },
            serde_json::json!({
                PROBE_MARKER: session_id.as_str(),
                "attempt": context.attempt_number(),
            }),
            lifetime,
        )
        .with_declared_identity(crate::DeclaredProcessIdentity::labelled(
            "probe",
            None::<String>,
        ));
        Ok(crate::StartProcessIntent {
            session_id,
            declaration,
        })
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for DeclaringProbe {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![probe_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == PROBE_TOOL).then(|| Arc::new(probe_tool().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        tool_id == probe_tool().id()
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let declared = serde_json::from_value::<Probe>(call.args.clone())
            .map_err(|error| error.to_string())
            .and_then(|probe| Ok((probe, Self::start(call.context, probe)?)));
        let (probe, start) = match declared {
            Ok(declared) => declared,
            Err(error) => return crate::ToolOutcome::err_fmt(error).into(),
        };
        if probe.fail_first && call.context.attempt_number() == 1 {
            return crate::ToolAttemptOutcome::done(
                crate::ToolOutcomeDone::failure(crate::ToolFailure::safe_retry(
                    crate::ToolFailureClass::External,
                    "transient",
                    "the probe's first attempt fails",
                    Some(1),
                )),
                crate::ToolIntents::v3(vec![crate::ToolIntent::StartProcess(Box::new(start))]),
            );
        }
        let start = match crate::DeclaredStart::new(call.context, start) {
            Ok(start) => start,
            Err(error) => return crate::ToolOutcome::err_fmt(error).into(),
        };
        let mut pending = crate::PendingCompletion::new();
        if probe.ignore_cancel {
            pending.on_cancel = crate::CancelHint::Ignore;
        }
        crate::ToolAttemptOutcome::pending(pending.resolved_by_declared_start(start))
    }
}

/// The one model every session of a law's world is served by.
struct Script {
    children: usize,
    producer: Producer,
    parent_calls: AtomicUsize,
    child_calls: AtomicUsize,
    /// Runs on the first child step, once.
    on_child_call: std::sync::Mutex<Option<Hook>>,
    /// Every child step waits here.
    child_gate: Gate,
    /// The parent's step after its tool results waits here.
    followup_gate: Gate,
    /// When set, a child's first step waits until this many children have
    /// started: the barrier that proves they run at once.
    barrier: Option<usize>,
    started: tokio::sync::watch::Sender<usize>,
    /// Children whose barrier never filled.
    serialized: AtomicUsize,
    /// The tool results the parent's follow-up step saw.
    parent_saw: std::sync::Mutex<Vec<String>>,
}

impl Script {
    fn new(children: usize, producer: Producer) -> Self {
        Self {
            children,
            producer,
            parent_calls: AtomicUsize::new(0),
            child_calls: AtomicUsize::new(0),
            on_child_call: std::sync::Mutex::new(None),
            child_gate: Gate::new(true),
            followup_gate: Gate::new(true),
            barrier: None,
            started: tokio::sync::watch::channel(0).0,
            serialized: AtomicUsize::new(0),
            parent_saw: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn on_first_child_call(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .on_child_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Waits until children have taken `steps` model steps between them.
    async fn child_steps(&self, steps: usize) {
        let mut started = self.started.subscribe();
        tokio::time::timeout(PATIENCE, started.wait_for(|started| *started >= steps))
            .await
            .unwrap_or_else(|_| panic!("children take {steps} steps"))
            .unwrap_or_else(|_| panic!("the script outlives its children"));
    }

    fn child_calls(&self) -> usize {
        self.child_calls.load(Ordering::SeqCst)
    }

    fn parent_saw(&self) -> Vec<String> {
        self.parent_saw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn parent_first_step(&self) -> crate::LlmResponse {
        match self.producer {
            Producer::Native { siblings } => {
                let spawns = (0..self.children).map(|index| crate::LlmOutputPart::ToolCall {
                    call_id: format!("declared-start-spawn-{index}"),
                    tool_name: "spawn_agent".to_string(),
                    input_json: serde_json::json!({
                        "task": child_task(index),
                        "capability": "default",
                    })
                    .to_string(),
                    replay: None,
                });
                let echoes = (0..siblings).map(|index| crate::LlmOutputPart::ToolCall {
                    call_id: format!("declared-start-echo-{index}"),
                    tool_name: crate::testing::FIXTURE_ECHO_TOOL.to_string(),
                    input_json: serde_json::json!({ "value": format!("sibling {index}") })
                        .to_string(),
                    replay: None,
                });
                crate::LlmResponse {
                    parts: spawns.chain(echoes).collect(),
                    ..crate::LlmResponse::default()
                }
            }
            Producer::Probe(probe) => crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::ToolCall {
                    call_id: "declared-start-probe-0".to_string(),
                    tool_name: PROBE_TOOL.to_string(),
                    input_json: serde_json::json!(probe).to_string(),
                    replay: None,
                }],
                ..crate::LlmResponse::default()
            },
            Producer::PromiseAll => {
                let spawns = (0..self.children)
                    .map(|index| {
                        format!(
                            "agents.spawn({{ task: {:?}, capability: \"default\" }})",
                            child_task(index)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                text_response(format!(
                    "<typescript>\nconst replies = await Promise.all([{spawns}]);\nfinish(replies.join(\" | \"));\n</typescript>"
                ))
            }
        }
    }

    async fn respond(self: Arc<Self>, request: crate::LlmRequest) -> crate::LlmResponse {
        let transcript = format!("{:?}", request.messages);
        if transcript.contains(PARENT_INPUT) {
            let results = request
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter_map(|block| match block {
                    crate::llm::types::LlmContentBlock::ToolResult { content, .. } => {
                        Some(format!("{content:?}"))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if results.is_empty() {
                self.parent_calls.fetch_add(1, Ordering::SeqCst);
                return self.parent_first_step();
            }
            self.followup_gate.passed().await;
            self.parent_calls.fetch_add(1, Ordering::SeqCst);
            *self
                .parent_saw
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = results;
            return text_response("declared-start parent done".to_string());
        }
        let index = (0..self.children)
            .find(|index| transcript.contains(&child_task(*index)))
            .unwrap_or_else(|| panic!("a child step names its task: {transcript}"));
        self.child_calls.fetch_add(1, Ordering::SeqCst);
        self.started.send_modify(|started| *started += 1);
        let hook = self
            .on_child_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
        if let Some(width) = self.barrier {
            let mut started = self.started.subscribe();
            if tokio::time::timeout(PATIENCE, started.wait_for(|started| *started >= width))
                .await
                .is_err()
            {
                self.serialized.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.child_gate.passed().await;
        match self.producer {
            Producer::Native { .. } | Producer::Probe(_) => text_response(child_reply(index)),
            Producer::PromiseAll => text_response(format!(
                "<typescript>\nfinish({:?});\n</typescript>",
                child_reply(index)
            )),
        }
    }
}

fn text_response(text: String) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text,
            response_meta: None,
        }],
        ..crate::LlmResponse::default()
    }
}

/// The intent outcomes a turn reported, by call id.
#[derive(Default)]
struct IntentOutcomes(std::sync::Mutex<Vec<(String, crate::ToolIntentExecutionOutcome)>>);

#[async_trait::async_trait]
impl crate::TurnActivitySink for IntentOutcomes {
    async fn emit(&self, activity: crate::TurnActivity) {
        if let crate::TurnEvent::ToolIntentOutcome { call_id, outcome } = activity.event {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((call_id, outcome));
        }
    }
}

/// What one attempt of a law's turn answered.
type Answer = Result<crate::AssembledTurn, crate::RuntimeError>;

/// One law's world: its session, the model, the registry the parent and its
/// children share, and the process worker the tier serves children with.
#[derive(Clone)]
struct World {
    session_id: SessionId,
    turn_id: TurnId,
    host: crate::RuntimeHostConfig,
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    store: Arc<dyn crate::RuntimeStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    faults: crate::testing::ProcessRegistryFaults,
    process_work: crate::ProcessWorkWiring,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    effect_host: Arc<dyn crate::EffectHost>,
    script: Arc<Script>,
    intents: Arc<IntentOutcomes>,
    /// The turn's own token: cancelled before the turn runs, it is a cancel
    /// the turn meets at its start. A cancel mid-flight is a request
    /// ([`World::request_cancel`]), which reaches a turn parked on any tier.
    cancel: tokio_util::sync::CancellationToken,
}

/// How a law builds its world.
struct Shape {
    children: usize,
    producer: Producer,
    timeout: Option<Duration>,
    barrier: Option<usize>,
}

impl Shape {
    fn one_child() -> Self {
        Self {
            children: 1,
            producer: Producer::Native { siblings: 0 },
            timeout: None,
            barrier: None,
        }
    }

    fn probe(probe: Probe) -> Self {
        Self {
            children: 0,
            producer: Producer::Probe(probe),
            timeout: None,
            barrier: None,
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
impl World {
    async fn new(tier: &DeclaredStartTier, name: &str, shape: Shape) -> Self {
        let session_id = SessionId::from(format!("{}-{name}", tier.prefix));
        let turn_id = TurnId::from(format!("{}-{name}-turn", tier.prefix));
        let mut script = Script::new(shape.children, shape.producer);
        script.barrier = shape.barrier;
        let script = Arc::new(script);
        let model = crate::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let script = Arc::clone(&script);
                move |request: crate::LlmRequest| {
                    let script = Arc::clone(&script);
                    async move { Ok(script.respond(request).await) }
                }
            })
            .build();
        let mut host =
            crate::LawBackend::over_stores(Arc::clone(&tier.stores), Arc::clone(&tier.effect_host))
                .host_config(
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                );
        host.providers.provider_resolver =
            Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
        let protocol = match shape.producer {
            Producer::Native { .. } | Producer::Probe(_) => {
                crate::testing::test_standard_protocol_factories()
            }
            Producer::PromiseAll => tier.rlm.clone(),
        };
        // The ordinary siblings are native calls; a cell reaches only
        // bindings its protocol renders.
        let tools: Option<(&str, Arc<dyn crate::ToolProvider>)> = match shape.producer {
            Producer::Native { .. } => Some((
                "conformance-declared-start-echo",
                Arc::new(crate::testing::FixtureTools),
            )),
            Producer::Probe(_) => {
                Some(("conformance-declared-start-probe", Arc::new(DeclaringProbe)))
            }
            Producer::PromiseAll => None,
        };
        let tools = tools.map(|(id, provider)| {
            Arc::new(crate::plugin::StaticPluginFactory::new(
                id,
                crate::facade_support::PluginSpec::new().with_tool_provider(provider),
            )) as Arc<dyn crate::facade_support::PluginFactory>
        });
        let factories = protocol
            .into_iter()
            .chain([(tier.subagents)(shape.timeout)])
            .chain(tools)
            .collect::<Vec<_>>();
        let faults = crate::testing::ProcessRegistryFaults::new(tier.stores.process_registry());
        // One watch, two consumers: the runtime's process port and the worker
        // observe the same registry handle.
        let watched = crate::facade_support::watch_process_registry(Arc::new(faults.clone()));
        let worker = lash_core_worker::DurableProcessWorker::new(
            lash_core_worker::DurableProcessWorkerConfig::new(
                Arc::new(crate::facade_support::PluginHost::new(factories.clone())),
                host.clone(),
                crate::ProcessWorkWiring::new(
                    watched.clone(),
                    Arc::new(crate::NoProcessWork::new(&watched)),
                ),
                Arc::new(crate::NoSessionWork::new()),
                crate::testing::runtime_lease_owner(),
            ),
        )
        .expect("build the declared-start process worker");
        let registry = Arc::clone(watched.registry());
        let process_work = tier.runner.process_work(watched, worker);
        Self {
            store: crate::conformance::law_session_store(tier.stores.as_ref(), &session_id).await,
            session_id,
            turn_id,
            host,
            factories,
            registry,
            faults,
            process_work,
            runner: Arc::clone(&tier.runner),
            effect_host: Arc::clone(&tier.effect_host),
            script,
            intents: Arc::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn admitted(&self) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::turn(&self.session_id, &self.turn_id))
    }

    async fn runtime(&self) -> crate::LashRuntime {
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        let state = crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            policy: policy.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        Box::pin(
            crate::LashRuntime::builder(self.host.clone(), crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(self.factories.clone())
                .with_store(crate::conformance::helpers::session_view(
                    &self.store,
                    self.session_id.clone(),
                ))
                .with_process_registry(Arc::clone(&self.registry))
                .with_process_work(self.process_work.clone())
                .with_queued_work(Arc::new(crate::NoSessionWork::new()))
                .build(),
        )
        .await
        .expect("build the declared-start runtime")
    }

    /// One attempt at the law's turn, answering on `answers` when it ends.
    fn attempt(
        &self,
        answers: Option<tokio::sync::mpsc::UnboundedSender<Answer>>,
    ) -> crate::ConformanceTurnAttempt {
        let world = self.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let answers = answers.clone();
            Box::pin(async move {
                let mut runtime = world.runtime().await;
                let mut input = crate::TurnInput::text(PARENT_INPUT);
                input.trace_turn_id = Some(world.turn_id.clone());
                let intents = Arc::clone(&world.intents);
                let turn = runtime
                    .drive_turn(
                        input,
                        crate::TurnOptions::new(world.cancel.clone(), scope)
                            .with_turn_events(intents.as_ref()),
                    )
                    .await;
                let end = crate::ConformanceTurnEnd::of(&turn);
                if let Some(answers) = answers {
                    let _ = answers.send(turn);
                }
                end
            })
        })
    }

    /// Runs the turn to its end and returns its answer.
    async fn run(&self) -> Answer {
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
        self.runner
            .run_turn(self.admitted(), self.attempt(Some(answers)))
            .await;
        answer(&mut answered).await
    }

    /// Runs the turn until `crash` fires, then redelivers it, and returns the
    /// redrive's answer.
    async fn run_crashed_at(&self, crash: crate::ConformanceCrash) -> Answer {
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
        self.runner
            .run_crashed_then_redriven_turn(
                self.admitted(),
                crashing(self.attempt(None), crash),
                self.attempt(Some(answers)),
            )
            .await;
        answer(&mut answered).await
    }

    /// Lets every child answer its step. A cancelled child that answers still
    /// ends `Cancelled`: a committed cancel outranks the child turn's own end.
    /// That is how a tier whose running children see no live stop observes a
    /// cancel, so a law that cancels its children releases them once the
    /// cancel is committed rather than rely on a stop mid-step.
    fn release_children(&self) {
        self.script.child_gate.open();
    }

    /// Requests the turn's immediate cancel, as a host does.
    async fn request_cancel(&self) {
        crate::TurnWorkDriver::for_session(
            Arc::clone(&self.effect_host),
            self.session_id.clone(),
            Arc::clone(&self.store),
        )
        .request_cancel(crate::TurnCancelRequest::new(
            crate::TurnAddress::new(self.session_id.clone(), self.turn_id.clone()),
            "declared-start-law-cancel",
            None,
        ))
        .await
        .expect("request the turn's cancel");
    }

    /// [`Self::request_cancel`] from a hook that cannot wait, after `delay`.
    fn request_cancel_after(&self, delay: Duration) {
        let world = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            world.request_cancel().await;
        });
    }

    /// The subagent children this world's session started.
    async fn children(&self) -> Vec<crate::ProcessRecord> {
        self.registry
            .list_processes(&crate::ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("list the registry's processes")
            .into_iter()
            .filter(|record| {
                matches!(
                    record.input.as_ref(),
                    crate::ProcessInput::SessionTurn { definition_key, .. }
                        if definition_key == SUBAGENT_DEFINITION
                ) && format!("{:?}", record.provenance).contains(self.session_id.as_str())
            })
            .collect()
    }

    async fn only_child(&self) -> crate::ProcessRecord {
        let children = self.children().await;
        assert_eq!(
            children.len(),
            1,
            "{}: exactly one child process: {children:#?}",
            self.session_id
        );
        children.into_iter().next().expect("one child")
    }

    /// Waits until `process_id` is terminal and returns its record.
    async fn terminal(&self, process_id: &crate::ProcessId) -> crate::ProcessRecord {
        let reached = tokio::time::timeout(PATIENCE, async {
            loop {
                let record = self
                    .registry
                    .get_process(process_id)
                    .await
                    .expect("read the child")
                    .expect("the child is registered");
                if record.is_terminal() {
                    return record;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        match reached {
            Ok(record) => record,
            Err(_) => {
                let record = self.registry.get_process(process_id).await;
                panic!(
                    "{}: the child reaches its terminal; it stands at {record:#?}",
                    self.session_id
                )
            }
        }
    }

    /// Waits until the law's session has started `count` children.
    async fn started(&self, count: usize) -> Vec<crate::ProcessRecord> {
        tokio::time::timeout(PATIENCE, async {
            loop {
                let children = self.children().await;
                if children.len() >= count {
                    return children;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{}: {count} children start", self.session_id))
    }

    /// The external children this world's probe declared.
    async fn probes(&self) -> Vec<crate::ProcessRecord> {
        self.registry
            .list_processes(&crate::ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("list the registry's processes")
            .into_iter()
            .filter(|record| {
                matches!(
                    record.input.as_ref(),
                    crate::ProcessInput::External { metadata }
                        if metadata[PROBE_MARKER] == self.session_id.as_str()
                )
            })
            .collect()
    }

    /// Waits until the probe's child is registered and `settled` holds of
    /// it, and returns it.
    async fn probe_until(
        &self,
        what: &str,
        settled: impl Fn(&crate::ProcessRecord) -> bool,
    ) -> crate::ProcessRecord {
        let reached = tokio::time::timeout(PATIENCE, async {
            loop {
                if let Some(probe) = self.probes().await.into_iter().find(|probe| settled(probe)) {
                    return probe;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        match reached {
            Ok(probe) => probe,
            Err(_) => {
                let probes = self.probes().await;
                panic!(
                    "{}: {what}; the probes stand at {probes:#?}",
                    self.session_id
                )
            }
        }
    }

    fn intent_outcomes(&self) -> Vec<(String, crate::ToolIntentExecutionOutcome)> {
        self.intents
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

async fn answer(answers: &mut tokio::sync::mpsc::UnboundedReceiver<Answer>) -> Answer {
    tokio::time::timeout(PATIENCE, answers.recv())
        .await
        .unwrap_or_else(|_| panic!("the tier's runner answers the law's turn"))
        .unwrap_or_else(|| panic!("the tier's runner ran the law's turn"))
}

/// `attempt`, dying at its next await once `crash` fired.
fn crashing(
    attempt: crate::ConformanceTurnAttempt,
    crash: crate::ConformanceCrash,
) -> crate::ConformanceTurnAttempt {
    Arc::new(move |scope| {
        let attempt = Arc::clone(&attempt);
        let crash = crash.clone();
        Box::pin(async move {
            tokio::select! {
                biased;
                () = crash.fired() => panic!("declared-start law: the crash fired"),
                end = attempt(scope) => {
                    panic!("declared-start law: the attempt ended ({end:?}) before its crash fired")
                }
            }
        })
    })
}

fn finished(world: &World, turn: Answer) -> crate::AssembledTurn {
    let turn =
        turn.unwrap_or_else(|error| panic!("{}: the turn ends: {error:?}", world.session_id));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "{}: outcome {:?}; errors {:?}",
        world.session_id,
        turn.outcome,
        turn.errors
    );
    turn
}

/// The `spawn_agent` records of `turn`, in call order.
fn spawn_records(turn: &crate::AssembledTurn) -> Vec<&crate::ToolCallRecord> {
    turn.tool_calls
        .iter()
        .filter(|record| record.tool == "spawn_agent")
        .collect()
}

/// The one `spawn_agent` record answered the child's own final value.
fn assert_answered_the_child(world: &World, turn: &crate::AssembledTurn) {
    let spawns = spawn_records(turn);
    assert_eq!(spawns.len(), 1, "{}: one spawn record", world.session_id);
    assert_eq!(
        spawns[0].output.value_for_projection(),
        serde_json::json!(child_reply(0)),
        "{}: the spawn answers the child's final value",
        world.session_id
    );
}

/// Where [`declared_start_crash_at_every_launch_boundary`] crashes the
/// parent.
#[derive(Clone, Copy, Debug)]
enum Boundary {
    /// After the seal (the attempt's `Pending` is recorded) and before the
    /// start is realized: the launching invocation dies before its
    /// registration reaches the registry.
    B1,
    /// After the registration and before the receipt: the launching
    /// invocation dies once the registry committed the registration.
    B2,
    /// After the receipt and before the parent parks: the child's first step
    /// fires the crash, on the first instant the child runs.
    B3,
    /// After arming and before the child's terminal: the child fires the
    /// crash once the parent has parked on it, then holds its answer until
    /// the redrive is under way.
    B4,
    /// After the child's terminal and before the parent's result is
    /// presented: the crash fires once the registry reads the child terminal,
    /// and the parent's next step waits for it.
    B5,
}

/// A crash at each launch boundary redrives to one process, one child
/// session, one child turn and one result.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn declared_start_crash_at_every_launch_boundary(tier: DeclaredStartTier) {
    for boundary in [
        Boundary::B1,
        Boundary::B2,
        Boundary::B3,
        Boundary::B4,
        Boundary::B5,
    ] {
        let world = World::new(
            &tier,
            &format!("crash-{boundary:?}").to_lowercase(),
            Shape::one_child(),
        )
        .await;
        let crash = crate::ConformanceCrash::new();
        let fire = {
            let crash = crash.clone();
            Arc::new(move || crash.fire()) as Arc<dyn Fn() + Send + Sync>
        };
        match boundary {
            // The launch runs in the invocation that owns the call, which
            // is the one a crash at B1 or B2 kills: the registration panics
            // there, and the engine delivers that invocation again.
            Boundary::B1 | Boundary::B2 => world.faults.hold_next_registration(
                match boundary {
                    Boundary::B1 => crate::testing::RegistrationHoldPoint::BeforeRegistering,
                    _ => crate::testing::RegistrationHoldPoint::AfterRegistering,
                },
                Arc::new(move || {
                    fire();
                    panic!("declared-start law: the launching invocation crashed");
                }),
            ),
            Boundary::B3 => world.script.on_first_child_call(move || fire()),
            Boundary::B4 => {
                // The child holds its answer until the crash fired, so the
                // parent is parked on a child that has not finished.
                world.script.child_gate.0.send_replace(false);
                let gate = world.script.child_gate.clone();
                let crash_for_gate = crash.clone();
                tokio::spawn(async move {
                    crash_for_gate.fired().await;
                    gate.open();
                });
                world.script.on_first_child_call(move || {
                    tokio::spawn(async move {
                        // The parent parks right after its launch receipt;
                        // give it that instant before the crash.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        fire();
                    });
                });
            }
            Boundary::B5 => {
                world.script.followup_gate.0.send_replace(false);
                let followup = world.script.followup_gate.clone();
                let watcher = world.clone();
                let crash_for_watch = crash.clone();
                tokio::spawn(async move {
                    let child = watcher.started(1).await.remove(0);
                    watcher.terminal(&child.id).await;
                    fire();
                    crash_for_watch.fired().await;
                    followup.open();
                });
            }
        }
        let turn = match boundary {
            Boundary::B1 | Boundary::B2 => world.run().await,
            _ => world.run_crashed_at(crash.clone()).await,
        };
        let turn = finished(&world, turn);
        assert!(crash.has_fired(), "{boundary:?}: the crash fired");
        assert_answered_the_child(&world, &turn);
        let child = world.only_child().await;
        let child = world.terminal(&child.id).await;
        assert_eq!(
            child.status,
            crate::ProcessStatus::Completed,
            "{boundary:?}: the one child completed: {child:#?}"
        );
        assert_eq!(
            world.script.child_calls(),
            1,
            "{boundary:?}: one child turn, run once"
        );
        let receipts = world
            .intent_outcomes()
            .into_iter()
            .filter(|(call, _)| call == "declared-start-spawn-0")
            .map(|(_, outcome)| outcome)
            .collect::<Vec<_>>();
        assert!(
            !receipts.is_empty()
                && receipts
                    .iter()
                    .all(|receipt| format!("{receipt:?}").contains(child.id.as_str())),
            "{boundary:?}: every receipt the call reported names the one child: {receipts:#?}"
        );
        let crate::ProcessInput::SessionTurn { result, .. } = child.input.as_ref() else {
            panic!("{boundary:?}: the child is a session turn");
        };
        assert_eq!(
            result,
            &crate::SessionTurnResult::FinalValue { schema: None },
            "{boundary:?}: the child's configuration is the declared one"
        );
        let _ = world
            .registry
            .get_process(&child.id)
            .await
            .expect("read the child");
    }
}

/// The tool record's intent outcome names the child process, and the model
/// sees the child's value only.
pub async fn spawn_agent_record_carries_child_identity(tier: DeclaredStartTier) {
    let world = World::new(&tier, "identity", Shape::one_child()).await;
    let turn = finished(&world, world.run().await);
    assert_answered_the_child(&world, &turn);
    let child = world.only_child().await;
    let identity = &child.identity;
    assert_eq!(
        identity.kind.as_str(),
        "subagent",
        "the child is a subagent"
    );
    assert_eq!(identity.label.as_deref(), Some("spawn"));
    // A tier that replays the turn reports its activity again; the outcome
    // it reports is the one.
    let mut receipts = world.intent_outcomes();
    receipts.dedup_by(|a, b| format!("{a:?}") == format!("{b:?}"));
    assert_eq!(receipts.len(), 1, "one intent outcome: {receipts:#?}");
    let (call_id, receipt) = &receipts[0];
    assert_eq!(call_id, "declared-start-spawn-0");
    let crate::ToolIntentExecutionOutcome::Executed { kind, result, .. } = receipt else {
        panic!("the receipt is an executed start: {receipt:?}");
    };
    assert_eq!(*kind, crate::ToolIntentKind::StartProcess);
    assert_eq!(
        crate::process_id_from_handle_json(result).ok().as_ref(),
        Some(&child.id),
        "the receipt names the child process"
    );
    let saw = world.script.parent_saw();
    assert_eq!(saw.len(), 1, "the parent saw one result");
    assert!(
        saw[0].contains(&child_reply(0)) && !saw[0].contains(child.id.as_str()),
        "the model sees the child's value and not its handle: {saw:?}"
    );
}

/// A deadline resolves the call as a timeout error and cancels the child.
pub async fn declared_start_timeout_cancels_the_child(tier: DeclaredStartTier) {
    let world = World::new(
        &tier,
        "timeout",
        Shape {
            timeout: Some(Duration::from_millis(500)),
            ..Shape::one_child()
        },
    )
    .await;
    // The child never answers on its own.
    world.script.child_gate.0.send_replace(false);
    let turn = finished(&world, world.run().await);
    world.release_children();
    let spawns = spawn_records(&turn);
    assert_eq!(spawns.len(), 1);
    let crate::ToolCallOutcome::Failure(failure) = &spawns[0].output.outcome else {
        panic!("a timed-out spawn is a failure: {:?}", spawns[0].output);
    };
    assert_eq!(failure.class, crate::ToolFailureClass::Timeout);
    assert_eq!(failure.message, "subagent timed out after 500ms");
    let child = world.only_child().await;
    let child = world.terminal(&child.id).await;
    assert_eq!(
        child.status,
        crate::ProcessStatus::Cancelled,
        "the timed-out child is cancelled: {child:#?}"
    );
    assert!(child.cancel_request.is_some(), "the cancel was requested");
}

/// Where [`declared_start_cancel_at_each_point`] cancels the parent turn.
#[derive(Clone, Copy, Debug)]
enum CancelPoint {
    /// Before the seal: the parent's spawn step is its last.
    C1,
    /// After the seal and before realization: the cancel arrives while the
    /// launch is at the registry. Whichever of the registration and the
    /// call's abandonment commits first decides: a child registered first is
    /// cancelled, and a registration after the abandonment is refused.
    C2,
    /// After the receipt and before the park: the child's first step.
    C3,
    /// While parked on a child that has not finished.
    C4,
    /// After the child's terminal: the terminal wins.
    C5,
}

/// A cancel at each point launches nothing, or cancels the child once, or
/// loses to a terminal that already arrived.
pub async fn declared_start_cancel_at_each_point(tier: DeclaredStartTier) {
    for point in [
        CancelPoint::C1,
        CancelPoint::C2,
        CancelPoint::C3,
        CancelPoint::C4,
        CancelPoint::C5,
    ] {
        let world = World::new(
            &tier,
            &format!("cancel-{point:?}").to_lowercase(),
            Shape::one_child(),
        )
        .await;
        match point {
            CancelPoint::C1 => world.cancel.cancel(),
            CancelPoint::C2 => {
                world.script.child_gate.0.send_replace(false);
                let registration = world.faults.pause_next_registration();
                let world = world.clone();
                tokio::spawn(async move {
                    registration.wait_until_validated().await;
                    world.request_cancel().await;
                    registration.resume();
                });
            }
            CancelPoint::C3 => {
                // The child holds its answer, so only the cancel ends it.
                world.script.child_gate.0.send_replace(false);
                let canceller = world.clone();
                world
                    .script
                    .on_first_child_call(move || canceller.request_cancel_after(Duration::ZERO));
            }
            CancelPoint::C4 => {
                world.script.child_gate.0.send_replace(false);
                let canceller = world.clone();
                world.script.on_first_child_call(move || {
                    canceller.request_cancel_after(Duration::from_millis(200));
                });
            }
            CancelPoint::C5 => {
                world.script.followup_gate.0.send_replace(false);
                let followup = world.script.followup_gate.clone();
                let watcher = world.clone();
                tokio::spawn(async move {
                    let child = watcher.started(1).await.remove(0);
                    watcher.terminal(&child.id).await;
                    watcher.request_cancel().await;
                    followup.open();
                });
            }
        }
        let turn = world.run().await;
        world.release_children();
        let children = world.children().await;
        match point {
            CancelPoint::C1 => {
                assert!(
                    children.is_empty(),
                    "C1: a cancel before the seal launches nothing: {children:#?}"
                );
            }
            CancelPoint::C2 => {
                assert!(children.len() <= 1, "C2: at most one child: {children:#?}");
                for child in &children {
                    let child = world.terminal(&child.id).await;
                    assert_eq!(
                        child.status,
                        crate::ProcessStatus::Cancelled,
                        "C2: a child the launch registered is cancelled: {child:#?}"
                    );
                }
            }
            CancelPoint::C3 | CancelPoint::C4 => {
                assert_eq!(children.len(), 1, "{point:?}: one child: {children:#?}");
                let child = world.terminal(&children[0].id).await;
                assert_eq!(
                    child.status,
                    crate::ProcessStatus::Cancelled,
                    "{point:?}: the child is cancelled: {child:#?}"
                );
            }
            CancelPoint::C5 => {
                assert_eq!(children.len(), 1, "C5: one child: {children:#?}");
                let child = world.terminal(&children[0].id).await;
                assert_eq!(
                    child.status,
                    crate::ProcessStatus::Completed,
                    "C5: the terminal wins over the cancel: {child:#?}"
                );
                assert!(
                    child.cancel_request.is_none(),
                    "C5: a child that already finished is not cancelled: {child:#?}"
                );
            }
        }
        if let Ok(turn) = &turn {
            assert!(
                spawn_records(turn).len() <= 1,
                "{point:?}: at most one spawn record"
            );
        }
        assert!(
            world.script.child_calls() <= 1,
            "{point:?}: the child ran at most once"
        );
    }
    // Under `CancelHint::Ignore` a cancelled wait only drops the wait: the
    // child keeps running and no cancel is requested of it.
    let world = World::new(
        &tier,
        "cancel-ignore",
        Shape::probe(Probe {
            ignore_cancel: true,
            detached: true,
            ..Probe::default()
        }),
    )
    .await;
    {
        let world = world.clone();
        tokio::spawn(async move {
            world
                .probe_until("the probe's child registers", |_| true)
                .await;
            world.request_cancel().await;
        });
    }
    let _ = world.run().await;
    let probes = world.probes().await;
    assert_eq!(probes.len(), 1, "Ignore: one child: {probes:#?}");
    assert!(
        !probes[0].is_terminal() && probes[0].cancel_request.is_none(),
        "Ignore: the child runs on, and nothing asked it to stop: {:#?}",
        probes[0]
    );
}

/// Prune leaves a held child, and a redrive of the parent finds it; once the
/// call consumed the child's terminal and released its hold, prune may take
/// it.
///
/// The parent is crashed on the child's first step, and the call's release
/// of its hold is paused: the law surveys the registry at that instant, when
/// the child is terminal and nothing but the hold keeps it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn declared_start_retention_hold_blocks_prune_until_consumed(tier: DeclaredStartTier) {
    let world = World::new(&tier, "retention", Shape::one_child()).await;
    let crash = crate::ConformanceCrash::new();
    {
        let crash = crash.clone();
        world.script.on_first_child_call(move || crash.fire());
    }
    let release = world.faults.pause_next_consumer_release();
    let surveyed = {
        let world = world.clone();
        let release = release.clone();
        tokio::spawn(async move {
            release.wait_until_validated().await;
            let child = world.only_child().await;
            assert!(child.is_terminal(), "the call consumed a terminal child");
            let prunable = world
                .registry
                .prunable_terminal_processes(
                    u64::MAX,
                    None,
                    crate::ProjectionWatermark::NoProjector,
                )
                .await
                .expect("survey the prunable processes");
            release.resume();
            (child.id, prunable)
        })
    };
    let turn = finished(&world, world.run_crashed_at(crash).await);
    assert_answered_the_child(&world, &turn);
    let (child, held) = surveyed.await.expect("the survey ran");
    assert!(
        !held.contains(&child),
        "a terminal child whose hold stands is not prunable: {held:?}"
    );
    assert_eq!(
        world.only_child().await.id,
        child,
        "the redrive found the held child"
    );
    assert_eq!(world.script.child_calls(), 1, "the child ran once");
    let released = world
        .registry
        .prunable_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
        .await
        .expect("survey the prunable processes");
    assert!(
        released.contains(&child),
        "once the call released it, the child is prunable: {released:?}"
    );
}

/// FIG-4131: the engine's cancellation of the start's invocation takes the
/// answer of the step that claimed the child's `ProcessStart` obligation,
/// after its closure took the claim. The claim is the step's own: the step
/// run again derives the same token and takes it back, so the start settles
/// the row `Delivered` itself — well inside one claim lapse, with no relay
/// pass retaking it — and the child it sent reaches its terminal.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn declared_start_cancel_at_the_claim_answer_delivers_the_start(tier: DeclaredStartTier) {
    let Some(cancels) = tier.runner.cancel_at_step_answer(".process-start-claim:v1") else {
        return;
    };
    let clock = tier.stores.clock();
    let began_ms = clock.timestamp_ms();
    let world = World::new(&tier, "cancel-at-claim-answer", Shape::one_child()).await;
    let turn = world.run().await;
    let child = world.only_child().await;
    let child = world.terminal(&child.id).await;
    assert_eq!(
        cancels(),
        1,
        "the engine's cancellation took the claim step's answer once"
    );
    let start = tier
        .stores
        .obligation_ledger(crate::store::ObligationKind::ProcessStart)
        .standing(&crate::store::process_start_obligation_id(&child.id))
        .await
        .expect("read the child's start obligation")
        .expect("registration armed the child's start obligation");
    let elapsed_ms = clock.timestamp_ms().saturating_sub(began_ms);
    let lapse_ms = lash_core::drive::relay::RelayPolicy::default().claim_ttl_ms;
    assert!(
        elapsed_ms < lapse_ms,
        "the law ran inside one claim lapse ({elapsed_ms} ms of {lapse_ms} ms), so no relay \
         could have retaken the claim"
    );
    assert_eq!(
        (start.state, start.attempts),
        (crate::store::ObligationState::Delivered, 1),
        "the rerun claim step took its own claim back and the start delivered the row, \
         within the one claim: {child:#?}"
    );
    assert!(
        child.external_ref.is_some(),
        "the start sent the child's run: {child:#?}"
    );
    assert_eq!(
        world.script.child_calls(),
        1,
        "the child ran once: {turn:?}"
    );
}

/// A child that finishes before the parent parks on it still resolves the
/// call: the parent is crashed when the child starts, and redriven only once
/// the child's terminal is recorded.
pub async fn declared_start_early_terminal_resolves_before_wait(tier: DeclaredStartTier) {
    let world = World::new(&tier, "early", Shape::one_child()).await;
    let crash = crate::ConformanceCrash::new();
    {
        let crash = crash.clone();
        world.script.on_first_child_call(move || crash.fire());
    }
    let terminal = Gate::new(false);
    {
        let world = world.clone();
        let terminal = terminal.clone();
        tokio::spawn(async move {
            let child = world.started(1).await.remove(0);
            world.terminal(&child.id).await;
            terminal.open();
        });
    }
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let redrive = world.attempt(Some(answers));
    let redrive: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
        let redrive = Arc::clone(&redrive);
        let terminal = terminal.clone();
        Box::pin(async move {
            terminal.passed().await;
            redrive(scope).await
        })
    });
    world
        .runner
        .run_crashed_then_redriven_turn(
            world.admitted(),
            crashing(world.attempt(None), crash),
            redrive,
        )
        .await;
    let turn = finished(&world, answer(&mut answered).await);
    assert_answered_the_child(&world, &turn);
    world.only_child().await;
    assert_eq!(world.script.child_calls(), 1, "the child ran once");
}

/// A redrive of a parked call re-arms the same wait: the child runs once,
/// the call resolves once, and the parent sees one result.
pub async fn declared_start_rearm_is_idempotent(tier: DeclaredStartTier) {
    let world = World::new(&tier, "rearm", Shape::one_child()).await;
    let crash = crate::ConformanceCrash::new();
    world.script.child_gate.0.send_replace(false);
    {
        let gate = world.script.child_gate.clone();
        let crash = crash.clone();
        tokio::spawn(async move {
            crash.fired().await;
            // The redrive re-arms the wait the crashed attempt armed before
            // the child answers.
            tokio::time::sleep(Duration::from_millis(200)).await;
            gate.open();
        });
    }
    {
        let crash = crash.clone();
        world.script.on_first_child_call(move || {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                crash.fire();
            });
        });
    }
    let turn = finished(&world, world.run_crashed_at(crash).await);
    assert_answered_the_child(&world, &turn);
    world.only_child().await;
    assert_eq!(world.script.child_calls(), 1, "the child ran once");
    assert_eq!(
        world.script.parent_saw().len(),
        1,
        "the parent saw the one resolution"
    );
}

/// Children spawned in one parent step run at once: each child's first step
/// waits until every sibling started. Width 8 is native parallel calls with
/// ordinary siblings; width 2 is RLM `Promise.all` over `agents.spawn`.
pub async fn batch_of_spawns_overlaps(tier: DeclaredStartTier) {
    for (width, producer) in [
        (8, Producer::Native { siblings: 2 }),
        (2, Producer::PromiseAll),
    ] {
        let world = World::new(
            &tier,
            &format!("overlap-{width}"),
            Shape {
                children: width,
                producer,
                timeout: None,
                barrier: Some(width),
            },
        )
        .await;
        let turn = finished(&world, world.run().await);
        assert_eq!(
            world.script.serialized.load(Ordering::SeqCst),
            0,
            "width {width}: every child started before any finished"
        );
        assert_eq!(
            world.children().await.len(),
            width,
            "width {width}: one child each"
        );
        assert_eq!(
            world.script.child_calls(),
            width,
            "width {width}: each child ran once"
        );
        match producer {
            Producer::Native { siblings } => {
                let spawns = spawn_records(&turn);
                assert_eq!(spawns.len(), width);
                for (index, record) in spawns.iter().enumerate() {
                    assert_eq!(
                        record.output.value_for_projection(),
                        serde_json::json!(child_reply(index)),
                        "width {width}: spawn {index} answers its own child"
                    );
                }
                assert_eq!(
                    turn.tool_calls.len(),
                    width + siblings,
                    "width {width}: every sibling settled"
                );
            }
            Producer::PromiseAll => {
                let reply = format!("{:?}", turn.outcome);
                for index in 0..width {
                    assert!(
                        reply.contains(&child_reply(index)),
                        "width {width}: the cell answers child {index}: {reply}"
                    );
                }
            }
            Producer::Probe(_) => unreachable!("the overlap law spawns agents"),
        }
    }
    batch_of_spawns_cancelled_mid_flight(&tier).await;
    batch_of_spawns_crash_redriven(&tier).await;
}

/// The width the mid-flight cancel and the crash-redrive run at.
const MID_FLIGHT_WIDTH: usize = 8;

fn mid_flight_shape() -> Shape {
    Shape {
        children: MID_FLIGHT_WIDTH,
        producer: Producer::Native { siblings: 2 },
        timeout: None,
        barrier: None,
    }
}

/// A turn cancelled while every child of its batch runs cancels each child,
/// once.
async fn batch_of_spawns_cancelled_mid_flight(tier: &DeclaredStartTier) {
    let world = World::new(tier, "overlap-cancel", mid_flight_shape()).await;
    world.script.child_gate.0.send_replace(false);
    {
        let world = world.clone();
        tokio::spawn(async move {
            world.script.child_steps(MID_FLIGHT_WIDTH).await;
            world.request_cancel().await;
        });
    }
    let _ = world.run().await;
    world.release_children();
    let children = world.children().await;
    assert_eq!(
        children.len(),
        MID_FLIGHT_WIDTH,
        "one child each: {children:#?}"
    );
    for child in children {
        let child = world.terminal(&child.id).await;
        assert_eq!(
            child.status,
            crate::ProcessStatus::Cancelled,
            "every child is cancelled: {child:#?}"
        );
        assert!(
            child.cancel_request.is_some(),
            "the cancel was requested of it"
        );
    }
    assert_eq!(
        world.script.child_calls(),
        MID_FLIGHT_WIDTH,
        "each child ran its one step"
    );
}

/// A parent crashed while every child of its batch runs is redriven onto
/// the same children: each is reused, runs once and answers its own call.
async fn batch_of_spawns_crash_redriven(tier: &DeclaredStartTier) {
    let world = World::new(tier, "overlap-crash", mid_flight_shape()).await;
    let crash = crate::ConformanceCrash::new();
    world.script.child_gate.0.send_replace(false);
    {
        let world = world.clone();
        let crash = crash.clone();
        tokio::spawn(async move {
            world.script.child_steps(MID_FLIGHT_WIDTH).await;
            crash.fire();
            // The children answer once the redrive is under way.
            tokio::time::sleep(Duration::from_millis(200)).await;
            world.script.child_gate.open();
        });
    }
    let turn = finished(&world, world.run_crashed_at(crash).await);
    assert_eq!(
        world.children().await.len(),
        MID_FLIGHT_WIDTH,
        "the redrive reused every child"
    );
    assert_eq!(
        world.script.child_calls(),
        MID_FLIGHT_WIDTH,
        "each child ran once"
    );
    let spawns = spawn_records(&turn);
    assert_eq!(spawns.len(), MID_FLIGHT_WIDTH);
    for (index, record) in spawns.iter().enumerate() {
        assert_eq!(
            record.output.value_for_projection(),
            serde_json::json!(child_reply(index)),
            "spawn {index} answers its own child"
        );
    }
}

/// A retryable failure followed by a declared start launches exactly one
/// child, from the final attempt: the failed attempt's own start intent is
/// discarded with it.
pub async fn declared_start_discarded_retry_launches_nothing(tier: DeclaredStartTier) {
    let world = World::new(
        &tier,
        "discarded-retry",
        Shape::probe(Probe {
            fail_first: true,
            ..Probe::default()
        }),
    )
    .await;
    {
        let world = world.clone();
        tokio::spawn(async move {
            world
                .probe_until("the probe's child registers", |_| true)
                .await;
            world.request_cancel().await;
        });
    }
    let _ = world.run().await;
    let probe = world
        .probe_until("the cancelled wait cancels its child", |probe| {
            probe.cancel_request.is_some()
        })
        .await;
    let probes = world.probes().await;
    assert_eq!(probes.len(), 1, "one child: {probes:#?}");
    let crate::ProcessInput::External { metadata } = probe.input.as_ref() else {
        panic!("the probe's child is external: {probe:#?}");
    };
    assert_eq!(
        metadata["attempt"], 2,
        "the child is the final attempt's: {probe:#?}"
    );
}

/// A start refused because its starter scope already closed settles the
/// call as a typed failure, and leaves no process and so no obligation.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn declared_start_refusal_settles_the_call(tier: DeclaredStartTier) {
    let world = World::new(&tier, "refusal", Shape::one_child()).await;
    world
        .registry
        .record_parent_end(&crate::ScopeId::turn(
            world.session_id.clone(),
            world.turn_id.clone(),
        ))
        .await
        .expect("close the turn's scope");
    let turn = finished(&world, world.run().await);
    let spawns = spawn_records(&turn);
    assert_eq!(spawns.len(), 1, "one spawn record");
    assert!(
        matches!(spawns[0].output.outcome, crate::ToolCallOutcome::Failure(_)),
        "a refused start settles the call as a failure: {:?}",
        spawns[0].output
    );
    let children = world.children().await;
    assert!(children.is_empty(), "no child registered: {children:#?}");
    assert_eq!(world.script.child_calls(), 0, "no child ran");
}

/// Closing the scope a child lives `Until` cancels it while the call is
/// still parked on it, through its lifetime: the cancel names the ended
/// scope, and the call resolves on the cancelled child.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn declared_start_scope_close_cancels_until_children(tier: DeclaredStartTier) {
    let world = World::new(&tier, "scope-close", Shape::one_child()).await;
    world.script.child_gate.0.send_replace(false);
    let closed = {
        let world = world.clone();
        let delivery = Arc::clone(&tier.delivery);
        tokio::spawn(async move {
            let child = world.started(1).await.remove(0);
            let scope = child
                .lifetime
                .scope()
                .cloned()
                .expect("a spawned child lives until its starter ends");
            crate::end_parent_scope(
                world.registry.as_ref(),
                delivery.as_ref(),
                &scope,
                crate::current_epoch_ms(),
            )
            .await
            .expect("end the child's starter scope");
            world.release_children();
            (child.id, scope)
        })
    };
    let turn = finished(&world, world.run().await);
    let (child, scope) = closed.await.expect("the scope closed");
    let child = world.terminal(&child).await;
    assert_eq!(
        child.status,
        crate::ProcessStatus::Cancelled,
        "the child is cancelled: {child:#?}"
    );
    let request = child
        .cancel_request
        .as_deref()
        .expect("the child carries a cancel request");
    assert_eq!(request.origin, crate::CancelOrigin::ParentEnded);
    assert_eq!(
        request.requester,
        scope.storage_id(),
        "the cancel names the ended scope"
    );
    let spawns = spawn_records(&turn);
    assert_eq!(spawns.len(), 1, "one spawn record");
    assert!(
        !matches!(spawns[0].output.outcome, crate::ToolCallOutcome::Success(_)),
        "the call resolved on the cancelled child: {:?}",
        spawns[0].output
    );
}
