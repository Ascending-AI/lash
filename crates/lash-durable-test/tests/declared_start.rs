//! ADR 0116 §7.3 through a host's `send()`: a tool's `Pending` that declares
//! its child start, and `spawn_agent` on that shape (ported by FIG-5216 from
//! the deleted lash-conformance `declared_start.rs`; its crash laws are
//! `declared_start_crash_laws.rs`).
//!
//! Each law creates a session on a core that serves its own node and sends
//! it one parent turn whose model calls `spawn_agent` natively (or, for the
//! `Promise.all` width, from an RLM cell), or the law's own declaring probe.
//! The core's node runs the parent's turn, each child's `SessionTurn`
//! process and its child session; the model is the law's script, so a law
//! can hold a child mid-turn, count its steps and see which parent step saw
//! which result.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::llm::types::{LlmContentBlock, LlmRequest, LlmResponse};
use lash_core_execution::facade_support::process_child_session_id;
use lash_core_execution::{
    ProcessId, ProcessListFilter, ProcessRecord, ProcessStatus, ProcessStatusFilter,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use served::{Tier, WATCHDOG};

/// What every law's parent turn is asked: the parent's requests carry it,
/// and a child's never do.
const PARENT: &str = "declared-start law: spawn the children";
const PARENT_DONE: &str = "declared-start parent done";

/// The definition key of a delegated child's `SessionTurn` process.
const CHILD_DEFINITION: &str = delegation::SESSION_TURN_DEFINITION;

/// How often a law re-reads the registry for a fact nothing ticks on. A
/// cadence, not a deadline: a wait that never ends is a hang, which the
/// watchdog reports.
const REREAD: Duration = Duration::from_millis(20);

/// How long a law waits for a process it expects to end: each runs for
/// seconds, so one that has not ended by then never will.
const ENDED_WITHIN: Duration = Duration::from_secs(60);

fn child_task(index: usize) -> String {
    format!("declared-start child {index}: answer your literal")
}

fn child_reply(index: usize) -> String {
    format!("declared-start child {index} literal")
}

// --- the script --------------------------------------------------------------

/// A gate a scripted model step waits on until the law opens it.
#[derive(Clone)]
struct Gate(Arc<tokio::sync::watch::Sender<bool>>);

impl Gate {
    fn new(open: bool) -> Self {
        Self(Arc::new(tokio::sync::watch::channel(open).0))
    }

    fn close(&self) {
        self.0.send_replace(false);
    }

    fn open(&self) {
        self.0.send_replace(true);
    }

    async fn passed(&self) {
        let mut open = self.0.subscribe();
        let _ = open.wait_for(|open| *open).await;
    }
}

/// How the parent's first step calls its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Producer {
    /// One native `spawn_agent` call per child in one model response, with
    /// `siblings` ordinary echo calls beside them.
    Native { siblings: usize },
    /// One call to the law's declaring probe.
    Probe(Probe),
    /// Two probe calls, one step after the other: the first builds a
    /// declaration and keeps its bytes without launching it; the second
    /// submits those bytes as its own.
    ReusedIdentity,
    /// Concurrent deferring calls from one RLM cell.
    PromiseAll,
}

/// The one model every session of a law's world is served by.
struct Script {
    children: usize,
    producer: Producer,
    child_calls: AtomicUsize,
    /// The parent's first step waits here.
    parent_gate: Gate,
    /// Every child step waits here.
    child_gate: Gate,
    /// The parent's step after its tool results waits here.
    followup_gate: Gate,
    /// When set, a child's first step waits until this many children have
    /// started: the barrier that proves they run at once. A batch that
    /// serialized its children never fills it, since the first child holds
    /// its answer until its siblings start: the law hangs, and the
    /// watchdog reports it.
    barrier: Option<usize>,
    /// How many parent first steps and child steps the model was asked.
    parent_asked: tokio::sync::watch::Sender<usize>,
    started: tokio::sync::watch::Sender<usize>,
    /// The tool results the parent's follow-up step saw.
    parent_saw: Mutex<Vec<String>>,
    /// The parent's first step also keeps its turn's scope through the
    /// probe.
    parent_keeps_scope: bool,
    /// What a child's first step calls the probe with, before it answers.
    child_probe: Option<Probe>,
    /// The tool results a child's answering step saw.
    child_saw: Mutex<Vec<String>>,
}

impl Script {
    fn new(children: usize, producer: Producer) -> Self {
        Self {
            children,
            producer,
            child_calls: AtomicUsize::new(0),
            parent_gate: Gate::new(true),
            child_gate: Gate::new(true),
            followup_gate: Gate::new(true),
            barrier: None,
            parent_asked: tokio::sync::watch::channel(0).0,
            started: tokio::sync::watch::channel(0).0,
            parent_saw: Mutex::new(Vec::new()),
            parent_keeps_scope: false,
            child_probe: None,
            child_saw: Mutex::new(Vec::new()),
        }
    }

    /// Waits until children have taken `steps` model steps between them.
    async fn child_steps(&self, steps: usize) {
        let mut started = self.started.subscribe();
        let _ = started.wait_for(|started| *started >= steps).await;
    }

    /// Waits until the parent's first step was asked.
    async fn parent_first_step(&self) {
        let mut asked = self.parent_asked.subscribe();
        let _ = asked.wait_for(|asked| *asked >= 1).await;
    }

    fn child_calls(&self) -> usize {
        self.child_calls.load(Ordering::SeqCst)
    }

    fn parent_saw(&self) -> Vec<String> {
        self.parent_saw.lock_recover().clone()
    }

    fn first_step(&self) -> LlmResponse {
        match self.producer {
            Producer::Native { siblings } => {
                let spawns = (0..self.children).map(|index| {
                    served::call(
                        &format!("declared-start-spawn-{index}"),
                        "spawn_agent",
                        serde_json::json!({ "task": child_task(index) }),
                    )
                });
                let echoes = (0..siblings).map(|index| {
                    served::call(
                        &format!("declared-start-echo-{index}"),
                        lash_core_execution::testing::FIXTURE_ECHO_TOOL,
                        serde_json::json!({ "value": format!("sibling {index}") }),
                    )
                });
                let keep = self.parent_keeps_scope.then(|| {
                    served::call(
                        "declared-start-keep-scope",
                        PROBE_TOOL,
                        Probe {
                            forge: Forge::KeepScope,
                            ..Probe::default()
                        }
                        .args(),
                    )
                });
                served::response(spawns.chain(echoes).chain(keep).collect())
            }
            Producer::PromiseAll => {
                let spawns = (0..self.children)
                    .map(|index| format!("agents.spawn({{ task: {:?} }})", child_task(index)))
                    .collect::<Vec<_>>()
                    .join(", ");
                served::cell(&format!(
                    "const replies = await Promise.all([{spawns}]);\nfinish(replies.join(\" | \"));"
                ))
            }
            Producer::Probe(probe) => probe_step(0, probe),
            Producer::ReusedIdentity => probe_step(
                0,
                Probe {
                    forge: Forge::Keep,
                    ..Probe::default()
                },
            ),
        }
    }

    async fn respond(self: Arc<Self>, request: LlmRequest) -> LlmResponse {
        let transcript = format!("{:?}", request.messages);
        if transcript.contains(PARENT) {
            let results = results(&request);
            if results.is_empty() {
                self.parent_asked.send_modify(|asked| *asked += 1);
                self.parent_gate.passed().await;
                return self.first_step();
            }
            if self.producer == Producer::ReusedIdentity && results.len() == 1 {
                return probe_step(
                    1,
                    Probe {
                        forge: Forge::Reuse,
                        ..Probe::default()
                    },
                );
            }
            self.followup_gate.passed().await;
            *self.parent_saw.lock_recover() = results;
            return served::text(&request, PARENT_DONE);
        }
        let index = (0..self.children)
            .find(|index| transcript.contains(&child_task(*index)))
            .unwrap_or_else(|| panic!("a child step names its task: {transcript}"));
        self.child_calls.fetch_add(1, Ordering::SeqCst);
        self.started.send_modify(|started| *started += 1);
        if let Some(width) = self.barrier {
            self.child_steps(width).await;
        }
        self.child_gate.passed().await;
        if let Some(probe) = self.child_probe {
            let seen = results(&request);
            if seen.is_empty() {
                return probe_step(0, probe);
            }
            *self.child_saw.lock_recover() = seen;
        }
        if self.producer == Producer::PromiseAll {
            served::cell(&format!("finish({:?});", child_reply(index)))
        } else {
            served::text(&request, &child_reply(index))
        }
    }
}

/// The tool results a request carries, rendered.
fn results(request: &LlmRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::ToolResult { content, .. } => Some(format!("{content:?}")),
            _ => None,
        })
        .collect()
}

fn model(script: &Arc<Script>) -> lash_core::facade_support::ProviderHandle {
    let script = Arc::clone(script);
    lash_core::testing::TestProvider::builder()
        .kind("declared-start-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let script = Arc::clone(&script);
            async move { Ok(script.respond(request).await) }
        })
        .build()
        .into_handle()
}

// --- the declaring probe -----------------------------------------------------

/// The tool name of [`DeclaringProbe`].
const PROBE_TOOL: &str = "declared_start_probe";

/// The payload key a probe's child carries its session under.
const PROBE_MARKER: &str = "declared_start_probe";

/// What a [`DeclaringProbe`] call declares: the call's arguments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Probe {
    /// The wait's cancel hint is `CancelHint::Ignore`.
    ignore_cancel: bool,
    /// The child is `Detached` rather than living until its starter ends.
    detached: bool,
    /// The first attempt fails retryably, declaring a start as a completed
    /// intent; the retry declares the start and parks on it.
    fail_first: bool,
    /// What the call does to the declaration's serialized bytes.
    forge: Forge,
}

impl Probe {
    fn args(self) -> serde_json::Value {
        serde_json::json!({
            "ignore_cancel": self.ignore_cancel,
            "detached": self.detached,
            "fail_first": self.fail_first,
            "forge": self.forge as u8,
        })
    }

    fn of(args: &serde_json::Value) -> Self {
        let flag = |name: &str| args[name].as_bool().unwrap_or_default();
        Self {
            ignore_cancel: flag("ignore_cancel"),
            detached: flag("detached"),
            fail_first: flag("fail_first"),
            forge: Forge::ALL[args["forge"].as_u64().unwrap_or_default() as usize],
        }
    }
}

/// How a [`DeclaringProbe`] call tampers with its declaration: a declared
/// start is durable, so it decodes without its constructor, and these are
/// the bytes a decoded one may carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Forge {
    /// The declaration exactly as `DeclaredStart::new` built it.
    #[default]
    None,
    /// The start and its identity name another session, the identity
    /// re-derived under it so its replay key is its own.
    ForeignSession,
    /// The identity is the declaring attempt's own for index 1.
    NonzeroIndex,
    /// Build the declaration, keep its bytes, launch nothing, and answer.
    Keep,
    /// Submit the bytes a [`Forge::Keep`] call kept: another call's
    /// identity.
    Reuse,
    /// Answer without declaring, keeping the turn's own scope.
    KeepScope,
    /// Declare a start that lives until the scope a [`Forge::KeepScope`]
    /// call of an earlier, ended turn kept.
    EndedScope,
}

impl Forge {
    /// Every forge, in declaration order: a call's argument names one by
    /// its position.
    const ALL: [Self; 7] = [
        Self::None,
        Self::ForeignSession,
        Self::NonzeroIndex,
        Self::Keep,
        Self::Reuse,
        Self::KeepScope,
        Self::EndedScope,
    ];
}

/// The session a [`Forge::ForeignSession`] declaration names.
const FOREIGN_SESSION: &str = "declared-start-law-foreign-session";

fn probe_tool() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{PROBE_TOOL}"),
        PROBE_TOOL,
        "Declares one child start and resolves on its terminal.",
        object.clone(),
        object,
    )
    .expect("the probe's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], PROBE_TOOL))
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(2).unwrap(),
        1,
        1,
    ))
    // It parks on a declared start, and its failing first attempt declares
    // the start as an ordinary intent that the retry discards.
    .with_declaration(
        lash_core::ToolDeclaration::deferring()
            .with_intents([lash_core::ToolIntentKind::StartProcess]),
    )
    .with_park(lash_core::ParkBound::Within(std::time::Duration::from_secs(120)))
}

/// A tool whose `Pending` declares one held child: the child idles until it
/// is cancelled, so the law controls its lifetime, the wait's cancel hint
/// and the attempt that declares it.
#[derive(Default)]
struct DeclaringProbe {
    /// The serialized declaration a [`Forge::Keep`] call kept.
    kept: Mutex<Option<serde_json::Value>>,
    /// The scope a [`Forge::KeepScope`] call kept.
    scope: Mutex<Option<lash_core::ScopeRef>>,
}

impl DeclaringProbe {
    /// The declaration `start` as `forge` leaves it: built by its
    /// constructor, then re-read from its serialized bytes. `Ok(None)` is a
    /// call that kept its bytes and declares nothing.
    fn declare(
        &self,
        context: &lash_core::AttemptContext<'_>,
        start: lash_core::StartProcessIntent,
        forge: Forge,
    ) -> Result<Option<lash_core::DeclaredStart>, String> {
        let declared =
            lash_core::DeclaredStart::new(context, start).map_err(|error| error.to_string())?;
        let mut bytes = serde_json::to_value(&declared).map_err(|error| error.to_string())?;
        match forge {
            Forge::None | Forge::EndedScope | Forge::KeepScope => return Ok(Some(declared)),
            Forge::ForeignSession => {
                let mut identity = context.intent_identity(0);
                let foreign = lash_core::RuntimeOwner::Session(SessionId::from(FOREIGN_SESSION));
                identity.owner = foreign.clone();
                let identity = lash_core::rederive_tool_intent_identity(&identity);
                bytes["start"]["owner"] =
                    serde_json::to_value(&foreign).map_err(|error| error.to_string())?;
                bytes["identity"] =
                    serde_json::to_value(identity).map_err(|error| error.to_string())?;
            }
            Forge::NonzeroIndex => {
                bytes["identity"] = serde_json::to_value(context.intent_identity(1))
                    .map_err(|error| error.to_string())?;
            }
            Forge::Keep => {
                *self.kept.lock_recover() = Some(bytes);
                return Ok(None);
            }
            Forge::Reuse => {
                bytes = self
                    .kept
                    .lock_recover()
                    .clone()
                    .ok_or("no call kept a declaration to reuse")?;
            }
        }
        serde_json::from_value(bytes)
            .map(Some)
            .map_err(|error| format!("a serialized declaration decodes: {error}"))
    }

    fn start(
        &self,
        context: &lash_core::AttemptContext<'_>,
        probe: Probe,
    ) -> Result<lash_core::StartProcessIntent, String> {
        let session_id = context
            .session_id()
            .map_err(|error| error.to_string())?
            .clone();
        let agent_frame_id = context
            .agent_frame_id()
            .map_err(|error| error.to_string())?
            .clone();
        let start_cx = context.start_cx().map_err(|error| error.to_string())?;
        let lifetime = if probe.detached {
            lash_core::Lifetime::Detached
        } else if probe.forge == Forge::EndedScope {
            lash_core::Lifetime::Until(
                self.scope
                    .lock_recover()
                    .clone()
                    .ok_or("no earlier turn kept its scope")?,
            )
        } else {
            lash_core::lifetime::starter(&start_cx)
        };
        let declaration = lash_core::ProcessStartDeclaration::new(
            lash_core_execution::testing::held_engine_input(serde_json::json!({
                PROBE_MARKER: session_id.as_str(),
                "attempt": context.attempt_number(),
            })),
            lash_core::ProcessOriginator::Session {
                session_id: session_id.clone(),
                agent_frame_id: Some(agent_frame_id),
            },
            lifetime,
        )
        .with_declared_identity(lash_core::DeclaredProcessIdentity::labelled(
            "probe",
            None::<String>,
        ))
        // The child runs under the attempt's captured environment.
        .with_env_ref(
            context
                .process_execution_env_ref()
                .map_err(|error| error.to_string())?,
        );
        Ok(lash_core::StartProcessIntent {
            owner: lash_core::RuntimeOwner::Session(session_id),
            declaration,
        })
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for DeclaringProbe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![probe_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == PROBE_TOOL).then(|| Arc::new(probe_tool().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let probe = Probe::of(call.args);
        if probe.forge == Forge::KeepScope {
            return match call.context.start_cx() {
                Ok(start_cx) => {
                    *self.scope.lock_recover() = Some(start_cx.starter());
                    lash_core::ToolOutcome::ok(serde_json::json!("kept")).into()
                }
                Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
            };
        }
        let start = match self.start(call.context, probe) {
            Ok(start) => start,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        if probe.fail_first && call.context.attempt_number() == 1 {
            return lash_core::ToolAttemptOutcome::done(
                lash_core::ToolOutcomeDone::failure(lash_core::ToolFailure::with_suggested_delay(
                    lash_core::ToolFailureClass::External,
                    "transient",
                    "the probe's first attempt fails",
                    Some(1),
                )),
                lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::StartProcess(Box::new(
                    start,
                ))]),
            );
        }
        let start = match self.declare(call.context, start, probe.forge) {
            Ok(Some(start)) => start,
            Ok(None) => return lash_core::ToolOutcome::ok(serde_json::json!("kept")).into(),
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        let mut pending = lash_core::PendingCompletion::new();
        if probe.ignore_cancel {
            pending.on_cancel = lash_core::CancelHint::Ignore;
        }
        lash_core::ToolAttemptOutcome::pending(pending.resolved_by_declared_start(start))
    }
}

/// One parent step calling the law's declaring probe once, as call `index`.
fn probe_step(index: usize, probe: Probe) -> LlmResponse {
    served::response(vec![served::call(
        &format!("declared-start-probe-{index}"),
        PROBE_TOOL,
        probe.args(),
    )])
}

// --- recorded facts ----------------------------------------------------------

/// The plugin id and config namespace of [`FactsFactory`].
const FACTS: &str = "declared-start-recorded-facts";

/// The value the parent session's creator states.
const PARENT_FACTS: &str = "recorded-by-the-parent";

/// The default the core installs [`FactsFactory`] with. A child that records
/// or runs under it took the node's default for a fact its parent recorded.
const NODE_FACTS_DEFAULT: &str = "the-node-default";

/// The owner of the [`FACTS`] namespace: a creator's value, else this
/// deployment's default. No parent's value reaches it (ADR 0134).
struct FactsOwner;

/// Its recorded namespace is [`recorded_facts`]'s shape.
impl lash_core::ConfigOwner for FactsOwner {
    type Create = serde_json::Value;
    type Recorded = serde_json::Value;
    type Refusal = String;
    type RunOptions = lash_core::NoRunOptions;

    fn create(
        &self,
        input: Option<serde_json::Value>,
    ) -> Result<Option<serde_json::Value>, String> {
        Ok(Some(
            input.unwrap_or_else(|| recorded_facts(NODE_FACTS_DEFAULT)),
        ))
    }

    fn validate(
        &self,
        _value: &serde_json::Value,
        _base: Option<&serde_json::Value>,
        _facts: &lash_core::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn apply_run_options(
        &self,
        recorded: &serde_json::Value,
        _options: lash_core::NoRunOptions,
    ) -> Result<serde_json::Value, String> {
        Ok(recorded.clone())
    }
}

/// A plugin that owns one recorded namespace and reports, per owner, the
/// namespace every plugin session it built for that owner was handed.
#[derive(Default)]
struct FactsFactory {
    builds: Mutex<Vec<(String, Option<serde_json::Value>)>>,
}

impl FactsFactory {
    /// What every plugin session this factory built for `owner` was handed.
    fn built_for(&self, owner: &str) -> Vec<Option<serde_json::Value>> {
        self.builds
            .lock_recover()
            .iter()
            .filter(|(built, _)| built == owner)
            .map(|(_, value)| value.clone())
            .collect()
    }
}

struct FactsPlugin;

impl lash_core::plugin::SessionPlugin for FactsPlugin {
    fn id(&self) -> &'static str {
        FACTS
    }

    fn register(
        &self,
        _reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::facade_support::PluginFactory for FactsFactory {
    fn id(&self) -> &'static str {
        FACTS
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> Result<(), lash_core::ConfigRegistrationError> {
        registrar.owner(FactsOwner)
    }

    fn build(
        &self,
        ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        self.builds.lock_recover().push((
            ctx.owner.to_string(),
            ctx.plugin_config.config.get(FACTS).cloned(),
        ));
        Ok(Arc::new(FactsPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for FactsFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(FACTS)
    }
}

fn recorded_facts(value: &str) -> serde_json::Value {
    serde_json::json!({ "value": value })
}

// --- the world ---------------------------------------------------------------

/// How a law's delegated children live.
#[derive(Clone, Copy)]
enum Lifetimes {
    /// Until the turn that spawned them ends.
    Starter,
    /// Until their parent's session ends.
    Session,
}

/// How a law builds its world.
struct Shape {
    children: usize,
    producer: Producer,
    barrier: Option<usize>,
    lifetimes: Lifetimes,
    /// The parent session's creator states [`PARENT_FACTS`].
    facts: bool,
    /// The parent keeps its turn's scope, and each child calls the probe
    /// with this before it answers.
    child_probe: Option<Probe>,
}

impl Shape {
    fn one_child() -> Self {
        Self {
            children: 1,
            producer: Producer::Native { siblings: 0 },
            barrier: None,
            lifetimes: Lifetimes::Starter,
            facts: false,
            child_probe: None,
        }
    }

    fn probe(probe: Probe) -> Self {
        Self {
            children: 0,
            producer: Producer::Probe(probe),
            ..Self::one_child()
        }
    }
}

/// One law's world: a core serving its own node, the parent session, the
/// script, the probe and the facts plugin.
struct World {
    served: served::World,
    session: lash::DurableSession,
    script: Arc<Script>,
    probe: Arc<DeclaringProbe>,
    facts: Arc<FactsFactory>,
}

/// The host's delegation tool (`examples/delegation`): children are created
/// from the parent's spec, stated explicitly, and run RLM when the parent does.
fn delegation(
    lifetimes: Lifetimes,
    rlm: bool,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    let factory = match lifetimes {
        Lifetimes::Starter => {
            delegation::DelegationPluginFactory::new(served::spec(64), lash_core::lifetime::starter)
        }
        Lifetimes::Session => delegation::DelegationPluginFactory::new(served::spec(64), |cx| {
            cx.session()
                .map_or(lash_core::Lifetime::Detached, lash_core::Lifetime::Until)
        }),
    };
    Arc::new(if rlm {
        factory.with_rlm_children(lash::rlm::RlmFinalAnswerFormat::RawFinalValue)
    } else {
        factory
    })
}

impl World {
    async fn new(tier: Tier, name: &str, shape: Shape) -> Option<Self> {
        let mut script = Script::new(shape.children, shape.producer);
        script.barrier = shape.barrier;
        script.parent_keeps_scope = shape.child_probe.is_some();
        script.child_probe = shape.child_probe;
        let script = Arc::new(script);
        let probe = Arc::new(DeclaringProbe::default());
        let facts = Arc::new(FactsFactory::default());
        let served = {
            let script = Arc::clone(&script);
            let probe = Arc::clone(&probe);
            let facts = Arc::clone(&facts);
            served::World::with_model(
                tier,
                vec![Arc::new(lash_core_execution::testing::HeldProcessEngine)],
                model(&script),
                move |backend| {
                    let builder = if shape.producer == Producer::PromiseAll {
                        lash::LashCore::rlm_builder(
                            backend.clone(),
                            served::rlm(backend, None, sim::untimed_workers()),
                        )
                    } else {
                        lash::LashCore::standard_builder(backend.clone())
                    };
                    let builder = builder
                        .plugin(delegation(
                            shape.lifetimes,
                            shape.producer == Producer::PromiseAll,
                        ))
                        .plugin(facts);
                    if shape.producer == Producer::PromiseAll {
                        builder
                    } else {
                        builder
                            .tools(probe)
                            .tools(Arc::new(lash_core_execution::testing::FixtureTools::new()))
                    }
                },
            )
            .await?
        };
        let mut spec = served::spec(64);
        if shape.facts {
            spec.plugin_options =
                lash_core::PluginOptions::typed(FACTS, recorded_facts(PARENT_FACTS))
                    .expect("the facts encode");
        }
        let session = served
            .session(&format!("declared-start-{name}"), spec)
            .await;
        Some(Self {
            served,
            session,
            script,
            probe,
            facts,
        })
    }

    fn backend(&self) -> &lash::Backend {
        &self.served.backend
    }

    /// Send the parent's input, run `during` beside the turn with the turn's
    /// cancel, and answer the turn's outcome.
    async fn run_with(
        &self,
        during: impl AsyncFnOnce(lash::CancelBuilder),
    ) -> lash::Result<lash::SendOutcome> {
        let handle = self
            .session
            .send(lash::TurnInput::text(PARENT))
            .await
            .expect("the parent's input is accepted");
        let cancel = handle.cancel();
        let settled = handle.outcome();
        let (outcome, ()) =
            tokio::time::timeout(WATCHDOG, async { tokio::join!(settled, during(cancel)) })
                .await
                .expect("deadlock watchdog: the parent's turn never settled");
        outcome
    }

    /// Run the parent's turn to its output.
    async fn run(&self) -> lash::TurnOutput {
        settled(self.run_with(async |_| {}).await)
    }

    /// Every process the deployment registered, whatever its status.
    async fn registered(&self) -> Vec<ProcessRecord> {
        self.backend()
            .process_registry()
            .list_processes(&ProcessListFilter {
                status: ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("the registry lists its processes")
    }

    /// The delegated children the deployment registered.
    async fn children(&self) -> Vec<ProcessRecord> {
        self.registered()
            .await
            .into_iter()
            .filter(|record| {
                matches!(
                    record.input.as_ref(),
                    lash_core::ProcessInput::SessionTurn { definition_key, .. }
                        if definition_key == CHILD_DEFINITION
                )
            })
            .collect()
    }

    /// The probe's children the deployment registered.
    async fn probes(&self) -> Vec<ProcessRecord> {
        self.registered()
            .await
            .into_iter()
            .filter(|record| {
                matches!(
                    record.input.as_ref(),
                    lash_core::ProcessInput::Engine { payload, .. }
                        if payload.get(PROBE_MARKER).is_some()
                )
            })
            .collect()
    }

    async fn only_child(&self) -> ProcessRecord {
        let mut children = self.children().await;
        assert_eq!(children.len(), 1, "one child: {children:#?}");
        children.remove(0)
    }

    /// The first probe child that satisfies `ready`, re-read until one does.
    async fn probe_until(&self, ready: impl Fn(&ProcessRecord) -> bool) -> ProcessRecord {
        loop {
            if let Some(probe) = self.probes().await.into_iter().find(&ready) {
                return probe;
            }
            tokio::time::sleep(REREAD).await;
        }
    }

    /// `process` once it ended.
    /// `process`'s record once it ended, within [`ENDED_WITHIN`].
    async fn terminal(&self, process: &ProcessId) -> ProcessRecord {
        let ended = async {
            loop {
                let record = self
                    .backend()
                    .process_registry()
                    .get_process(process)
                    .await
                    .expect("read the process")
                    .expect("the process is retained");
                if record.is_terminal() {
                    return record;
                }
                tokio::time::sleep(REREAD).await;
            }
        };
        tokio::time::timeout(ENDED_WITHIN, ended)
            .await
            .unwrap_or_else(|_| panic!("process {process} did not end within {ENDED_WITHIN:?}"))
    }

    async fn shutdown(self) {
        self.served.shutdown().await;
    }
}

/// The settled output of `outcome`.
fn settled(outcome: lash::Result<lash::SendOutcome>) -> lash::TurnOutput {
    match outcome.expect("the parent's turn answers") {
        lash::SendOutcome::Settled { output, .. } => *output,
        other => panic!("the parent's turn settles: {other:?}"),
    }
}

/// Cancel the parent's turn through `cancel`.
async fn cancel_turn(cancel: lash::CancelBuilder) {
    cancel.await.expect("the parent's turn is cancelled");
}

/// The one spawn's result the parent's follow-up read is the child's own
/// reply.
fn assert_answered_the_child(world: &World, index: usize) {
    let saw = world.script.parent_saw();
    assert_eq!(saw.len(), 1, "the parent saw one result: {saw:?}");
    assert!(
        saw[0].contains(&child_reply(index)),
        "the parent's call answers the child's reply: {saw:?}"
    );
}

// --- the laws ----------------------------------------------------------------

/// The spawn's child is a `subagent` labelled `spawn`, started by the parent
/// session's frame and bound to it; the host's activity carries one receipt
/// for the call, an executed start naming the child; and the model sees the
/// child's value only, never its handle.
async fn spawn_agent_record_carries_child_identity(tier: Tier) {
    let Some(world) = World::new(tier, "identity", Shape::one_child()).await else {
        return;
    };
    let output = world.run().await;
    served::assert_answered("the parent", &output);
    assert_answered_the_child(&world, 0);
    let child = world.only_child().await;
    assert_eq!(
        child.identity.kind.as_str(),
        "subagent",
        "the child is a subagent"
    );
    assert_eq!(child.identity.label.as_deref(), Some("spawn"));
    let parent = world.session.session_id().clone();
    assert!(
        matches!(
            &child.provenance.originator,
            lash_core::ProcessOriginator::Session { session_id, agent_frame_id: Some(_) }
                if *session_id == parent
        ),
        "the parent session's frame started the child: {:?}",
        child.provenance.originator
    );
    assert_eq!(
        child.session_capability.as_ref(),
        Some(&parent),
        "the child is bound to its parent's session"
    );
    let spawned = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash_core::TurnEvent::ToolCallStarted {
                call_id,
                provider_call_id: Some(provider),
                ..
            } if provider == "declared-start-spawn-0" => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        spawned.len(),
        1,
        "the host saw the spawn start once: {:#?}",
        output.activities
    );
    let receipts = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash_core::TurnEvent::ToolIntentOutcome { call_id, outcome } => {
                Some((call_id.clone(), outcome.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1, "one intent outcome: {receipts:#?}");
    let (call_id, receipt) = &receipts[0];
    assert_eq!(call_id, &spawned[0], "the receipt is the spawn's");
    let lash_core::ToolIntentExecutionOutcome::Executed {
        realized: lash_core::ToolIntentRealized::StartProcess(result),
        ..
    } = receipt
    else {
        panic!("the receipt is an executed start: {receipt:?}");
    };
    assert_eq!(
        &result.process_id, &child.id,
        "the receipt names the child process"
    );
    let saw = world.script.parent_saw();
    assert!(
        !saw[0].contains(child.id.as_str()),
        "the model sees the child's value and not its handle: {saw:?}"
    );
    world.shutdown().await;
}

/// L08: a session-owned subagent outlives the cancel and the end of the turn
/// that observes it.
async fn a_session_lifetime_subagent_survives_its_waiting_turn(tier: Tier) {
    let Some(world) = World::new(
        tier,
        "session-lifetime",
        Shape {
            lifetimes: Lifetimes::Session,
            ..Shape::one_child()
        },
    )
    .await
    else {
        return;
    };
    world.script.child_gate.close();
    let script = Arc::clone(&world.script);
    let _ = world
        .run_with(async move |cancel| {
            script.child_steps(1).await;
            cancel_turn(cancel).await;
        })
        .await;
    let child = world.only_child().await;
    assert!(
        child.outcome().is_none() && child.cancel_request.is_none(),
        "neither the cancel nor the end of the observing turn ends session-lifetime work: \
         {child:#?}"
    );
    world.script.child_gate.open();
    let child = world.terminal(&child.id).await;
    assert_eq!(
        child.status(),
        ProcessStatus::Completed,
        "the child ran on to its own end: {child:#?}"
    );
    world.shutdown().await;
}

/// The process that runs a spawned child runs under the environment its
/// start captured (FIG-4396), and the child session records only what its
/// creator stated (ADR 0134).
///
/// The parent's creator states [`PARENT_FACTS`]; the core installs the
/// owner under [`NODE_FACTS_DEFAULT`]. The child's `SessionTurn` process
/// captures the parent's recorded environment at its start, and the node
/// builds the process's own plugin runtime from it. The child session the
/// process creates states no facts, so it records, and runs under, the
/// owner's default: nothing of its parent's config reaches it.
async fn declared_start_child_records_only_its_creators_facts(tier: Tier) {
    let Some(world) = World::new(
        tier,
        "recorded-facts",
        Shape {
            facts: true,
            ..Shape::one_child()
        },
    )
    .await
    else {
        return;
    };
    let output = world.run().await;
    served::assert_answered("the parent", &output);
    assert_answered_the_child(&world, 0);
    let recorded = Some(recorded_facts(PARENT_FACTS));
    let default = Some(recorded_facts(NODE_FACTS_DEFAULT));

    let child = world.only_child().await;
    let env_ref = child
        .env_ref
        .clone()
        .expect("the child's SessionTurn process captured its parent's environment");
    let stores = world.backend().stores();
    let bytes = stores
        .process_env_store()
        .get_process_execution_env(&env_ref)
        .await
        .expect("read the child's captured environment")
        .expect("the child's environment is stored");
    let environment = lash_core::ProcessExecutionEnvSpec::from_store_bytes(&bytes)
        .expect("decode the child's captured environment");
    assert_eq!(
        environment.plugin_config.config.get(FACTS),
        recorded.as_ref(),
        "the captured environment is the parent's recorded config"
    );
    let process_builds = world
        .facts
        .built_for(&lash_core::RuntimeOwner::Process(child.id.clone()).to_string());
    assert!(
        !process_builds.is_empty() && process_builds.iter().all(|seen| *seen == recorded),
        "the node built the child process's runtime under the recorded facts, not its own \
         default: {process_builds:?}"
    );

    let child_session = process_child_session_id(&child.id);
    let head = lash_core::SessionCommitStore::load_session_head_meta(
        stores.session_store_factory().as_ref(),
        &child_session,
    )
    .await
    .expect("read the child's config head")
    .expect("the child session recorded a head");
    assert_eq!(
        head.config.plugin_config.get(FACTS),
        default.as_ref(),
        "the child session recorded the owner's default, not its parent's value"
    );
    let session_builds = world
        .facts
        .built_for(&lash_core::RuntimeOwner::Session(child_session).to_string());
    assert!(
        !session_builds.is_empty() && session_builds.iter().all(|seen| *seen == default),
        "the child session ran under its recorded value: {session_builds:?}"
    );
    world.shutdown().await;
}

/// Where [`declared_start_cancel_at_each_point`] cancels the parent turn.
#[derive(Clone, Copy, Debug)]
enum CancelPoint {
    /// While the parent's spawn step is being asked: the call never runs.
    BeforeTheSpawn,
    /// While parked on a child that has not finished.
    Parked,
    /// After the child's terminal: the terminal wins.
    AfterTheTerminal,
}

/// A cancel at each point launches nothing, or cancels the child once, or
/// loses to a terminal that already arrived. Under `CancelHint::Ignore` a
/// cancelled wait only drops the wait: the child runs on, and nothing asks
/// it to stop.
async fn declared_start_cancel_at_each_point(tier: Tier) {
    for point in [
        CancelPoint::BeforeTheSpawn,
        CancelPoint::Parked,
        CancelPoint::AfterTheTerminal,
    ] {
        let Some(world) = World::new(
            tier,
            &format!("cancel-{point:?}").to_lowercase(),
            Shape::one_child(),
        )
        .await
        else {
            return;
        };
        let script = Arc::clone(&world.script);
        let outcome = match point {
            CancelPoint::BeforeTheSpawn => {
                script.parent_gate.close();
                world
                    .run_with(async move |cancel| {
                        script.parent_first_step().await;
                        cancel_turn(cancel).await;
                        script.parent_gate.open();
                    })
                    .await
            }
            CancelPoint::Parked => {
                // The child holds its answer, so only the cancel ends it.
                script.child_gate.close();
                world
                    .run_with(async move |cancel| {
                        script.child_steps(1).await;
                        cancel_turn(cancel).await;
                    })
                    .await
            }
            CancelPoint::AfterTheTerminal => {
                script.followup_gate.close();
                let backend = world.backend().clone();
                world
                    .run_with(async move |cancel| {
                        script.child_steps(1).await;
                        let child = loop {
                            let ended = backend
                                .process_registry()
                                .list_processes(&ProcessListFilter {
                                    status: ProcessStatusFilter::Any,
                                    ..ProcessListFilter::default()
                                })
                                .await
                                .expect("list the processes")
                                .into_iter()
                                .find(ProcessRecord::is_terminal);
                            if let Some(child) = ended {
                                break child;
                            }
                            tokio::time::sleep(REREAD).await;
                        };
                        assert_eq!(child.status(), ProcessStatus::Completed);
                        cancel_turn(cancel).await;
                        script.followup_gate.open();
                    })
                    .await
            }
        };
        let children = world.children().await;
        match point {
            CancelPoint::BeforeTheSpawn => assert!(
                children.is_empty(),
                "a cancel before the spawn launches nothing: {children:#?}"
            ),
            CancelPoint::Parked => {
                assert_eq!(children.len(), 1, "one child: {children:#?}");
                // The child's model step is still held: only the cancel can
                // end it.
                let child = world.terminal(&children[0].id).await;
                assert_eq!(
                    child.status(),
                    ProcessStatus::Cancelled,
                    "the parked call's child is cancelled: {child:#?}"
                );
            }
            CancelPoint::AfterTheTerminal => {
                assert_eq!(children.len(), 1, "one child: {children:#?}");
                let child = world.terminal(&children[0].id).await;
                assert_eq!(
                    child.status(),
                    ProcessStatus::Completed,
                    "the terminal wins over the cancel: {child:#?}"
                );
                assert!(
                    child.cancel_request.is_none(),
                    "a child that already finished is not cancelled: {child:#?}"
                );
            }
        }
        world.script.child_gate.open();
        assert!(
            world.script.child_calls() <= 1,
            "{point:?}: the child ran at most once"
        );
        drop(outcome);
        world.shutdown().await;
    }

    let Some(world) = World::new(
        tier,
        "cancel-ignore",
        Shape::probe(Probe {
            ignore_cancel: true,
            detached: true,
            ..Probe::default()
        }),
    )
    .await
    else {
        return;
    };
    let backend = world.backend().clone();
    let _ = world
        .run_with(async move |cancel| {
            // Once the probe's child registers, the turn is cancelled.
            while !probes_of(&backend).await.iter().any(|_| true) {
                tokio::time::sleep(REREAD).await;
            }
            cancel_turn(cancel).await;
        })
        .await;
    let probes = world.probes().await;
    assert_eq!(probes.len(), 1, "Ignore: one child: {probes:#?}");
    assert!(
        !probes[0].is_terminal() && probes[0].cancel_request.is_none(),
        "Ignore: the child runs on, and nothing asked it to stop: {:#?}",
        probes[0]
    );
    world.shutdown().await;
}

/// The probe children `backend` registered.
async fn probes_of(backend: &lash::Backend) -> Vec<ProcessRecord> {
    backend
        .process_registry()
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list the processes")
        .into_iter()
        .filter(|record| {
            matches!(
                record.input.as_ref(),
                lash_core::ProcessInput::Engine { payload, .. }
                    if payload.get(PROBE_MARKER).is_some()
            )
        })
        .collect()
}

/// A retryable failure followed by a declared start launches exactly one
/// child, from the final attempt: the failed attempt's own start intent is
/// discarded with it.
async fn declared_start_discarded_retry_launches_nothing(tier: Tier) {
    let Some(world) = World::new(
        tier,
        "discarded-retry",
        Shape::probe(Probe {
            fail_first: true,
            ..Probe::default()
        }),
    )
    .await
    else {
        return;
    };
    let backend = world.backend().clone();
    let _ = world
        .run_with(async move |cancel| {
            // Once the probe's child registers, the turn is cancelled.
            while probes_of(&backend).await.is_empty() {
                tokio::time::sleep(REREAD).await;
            }
            cancel_turn(cancel).await;
        })
        .await;
    let probe = world
        .probe_until(|probe| probe.cancel_request.is_some())
        .await;
    let probes = world.probes().await;
    assert_eq!(probes.len(), 1, "one child: {probes:#?}");
    let lash_core::ProcessInput::Engine { payload, .. } = probe.input.as_ref() else {
        panic!("the probe's child is an engine process: {probe:#?}");
    };
    assert_eq!(
        payload["attempt"], 2,
        "the child is the final attempt's: {probe:#?}"
    );
    world.shutdown().await;
}

/// A start refused because a scope it lives under already closed settles
/// the call as a typed failure, and leaves no process and so no obligation.
///
/// The parent's turn keeps its scope and spawns a session-lifetime child,
/// then is cancelled and ends while the child holds its first step. The
/// child's turn then declares a start that lives until the parent's ended
/// turn, a scope in the child's own lineage.
async fn declared_start_refusal_settles_the_call(tier: Tier) {
    let Some(world) = World::new(
        tier,
        "refusal",
        Shape {
            lifetimes: Lifetimes::Session,
            child_probe: Some(Probe {
                forge: Forge::EndedScope,
                ..Probe::default()
            }),
            ..Shape::one_child()
        },
    )
    .await
    else {
        return;
    };
    world.script.child_gate.close();
    let script = Arc::clone(&world.script);
    let _ = world
        .run_with(async move |cancel| {
            script.child_steps(1).await;
            cancel_turn(cancel).await;
        })
        .await;
    let ended = world
        .probe
        .scope
        .lock_recover()
        .clone()
        .expect("the parent's turn kept its scope");
    world.script.child_gate.open();
    let child = world.only_child().await;
    let child = world.terminal(&child.id).await;
    assert_eq!(
        child.status(),
        ProcessStatus::Completed,
        "the child answered past its refused start: {child:#?}"
    );
    let saw = world.script.child_saw.lock_recover().clone();
    assert_eq!(
        saw.len(),
        1,
        "the child saw its probe's one result: {saw:?}"
    );
    assert!(
        saw[0].contains("[Tool execution failed]")
            && saw[0].contains("process_parent_ended")
            && saw[0].contains(&ended.id().to_string()),
        "the call settles as the closed-scope refusal naming the exact scope that closed ({}): \
         {saw:?}",
        ended.id()
    );
    let probes = world.probes().await;
    assert!(probes.is_empty(), "no child registered: {probes:#?}");
    world.shutdown().await;
}

/// A declared start is durable, so it decodes without its constructor. A
/// decoded declaration that names another session, carries its declaring
/// attempt's identity for a nonzero index, or carries another call's
/// identity is refused before it launches: the call settles as the typed
/// refusal, and nothing is registered and no child runs.
///
/// The other call is a real one: the parent's first step calls the probe to
/// build a declaration and keep its bytes, and its second step calls the
/// probe again to submit them.
async fn declared_start_rejects_foreign_or_reused_serialized_identity_before_launch(tier: Tier) {
    for (name, shape, call) in [
        (
            "foreign-session",
            Shape::probe(Probe {
                forge: Forge::ForeignSession,
                ..Probe::default()
            }),
            "declared-start-probe-0",
        ),
        (
            "nonzero-index",
            Shape::probe(Probe {
                forge: Forge::NonzeroIndex,
                ..Probe::default()
            }),
            "declared-start-probe-0",
        ),
        (
            "reused-identity",
            Shape {
                producer: Producer::ReusedIdentity,
                ..Shape::probe(Probe::default())
            },
            "declared-start-probe-1",
        ),
    ] {
        let Some(world) = World::new(tier, &format!("unbound-{name}"), shape).await else {
            return;
        };
        let output = world.run().await;
        let called = served::calls(&output);
        let record = called
            .iter()
            .find(|called| called.provider_call_id == call)
            .unwrap_or_else(|| panic!("{name}: the forged call is in the transcript: {called:#?}"));
        let call_id = record.call_id.clone().expect("the forged call has an id");
        let answered = served::results(&output)
            .into_iter()
            .find(|answered| answered.call_id.as_ref() == Some(&call_id))
            .unwrap_or_else(|| panic!("{name}: the forged call has a result"));
        let code = match name {
            "foreign-session" => "owner_mismatch",
            _ => "declared_start_identity_mismatch",
        };
        assert!(
            answered.content.contains("[Tool execution failed]") && answered.content.contains(code),
            "{name}: the call settles as the start's typed refusal `{code}`: {answered:?}"
        );
        let registered = world.probes().await;
        assert!(
            registered.is_empty(),
            "{name}: the refused declaration registered nothing: {registered:#?}"
        );
        world.shutdown().await;
    }
}

/// Children spawned in one parent step run at once: each child's first
/// model call waits for every sibling to start. Native width 8 includes two
/// ordinary siblings; RLM width 2 uses `Promise.all` over `agents.spawn`.
/// A turn cancelled while its native batch runs cancels each child once.
async fn batch_of_spawns_overlaps(tier: Tier) {
    const WIDTH: usize = 8;
    const SIBLINGS: usize = 2;
    for (width, producer) in [
        (WIDTH, Producer::Native { siblings: SIBLINGS }),
        (2, Producer::PromiseAll),
    ] {
        let Some(world) = World::new(
            tier,
            &format!("overlap-{width}"),
            Shape {
                children: width,
                producer,
                barrier: Some(width),
                ..Shape::one_child()
            },
        )
        .await
        else {
            return;
        };
        let output = world.run().await;
        assert!(
            output.is_success(),
            "width {width}, {producer:?}: the batch answers: {:?}; activities: {:#?}; parent asked: {}; child calls: {}",
            output.result.outcome,
            output.activities,
            *world.script.parent_asked.borrow(),
            world.script.child_calls()
        );
        assert_eq!(world.children().await.len(), width, "one child each");
        assert_eq!(world.script.child_calls(), width, "each child ran once");
        match producer {
            Producer::Native { siblings } => {
                let saw = world.script.parent_saw();
                assert_eq!(saw.len(), width + siblings, "every call settled: {saw:?}");
                for index in 0..width {
                    assert_eq!(
                        saw.iter()
                            .filter(|result| result.contains(&child_reply(index)))
                            .count(),
                        1,
                        "spawn {index} answers its own child: {saw:?}"
                    );
                }
            }
            Producer::PromiseAll => {
                let reply = format!("{:?}", output.result.outcome);
                for index in 0..width {
                    assert!(
                        reply.contains(&child_reply(index)),
                        "the cell answers child {index}: {reply}"
                    );
                }
            }
            _ => unreachable!("the overlap law spawns agents"),
        }
        world.shutdown().await;
    }

    let Some(world) = World::new(
        tier,
        "overlap-cancel",
        Shape {
            children: WIDTH,
            producer: Producer::Native { siblings: SIBLINGS },
            ..Shape::one_child()
        },
    )
    .await
    else {
        return;
    };
    world.script.child_gate.close();
    let script = Arc::clone(&world.script);
    let _ = world
        .run_with(async move |cancel| {
            script.child_steps(WIDTH).await;
            cancel_turn(cancel).await;
        })
        .await;
    let children = world.children().await;
    assert_eq!(children.len(), WIDTH, "one child each: {children:#?}");
    for child in children {
        let child = world.terminal(&child.id).await;
        assert_eq!(
            child.status(),
            ProcessStatus::Cancelled,
            "every child is cancelled: {child:#?}"
        );
        assert!(
            child.cancel_request.is_some(),
            "the cancel was requested of it"
        );
    }
    world.script.child_gate.open();
    assert_eq!(
        world.script.child_calls(),
        WIDTH,
        "each child ran its one step"
    );
    world.shutdown().await;
}

tiered_laws!(
    spawn_agent_record_carries_child_identity,
    a_session_lifetime_subagent_survives_its_waiting_turn,
    declared_start_child_records_only_its_creators_facts,
    declared_start_cancel_at_each_point,
    declared_start_discarded_retry_launches_nothing,
    declared_start_refusal_settles_the_call,
    declared_start_rejects_foreign_or_reused_serialized_identity_before_launch,
    batch_of_spawns_overlaps,
);
