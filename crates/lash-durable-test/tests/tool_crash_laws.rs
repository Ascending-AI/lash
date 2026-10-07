//! The tool-semantics laws a node kill must not break, cut at every commit
//! label (FIG-4546, FIG-4079; ported by FIG-5210 from the deleted
//! lash-conformance `tool_batch_parallelism/limit.rs` and
//! `tool_call_identity/replay.rs` crash laws).
//!
//! A host creates a session on a core and sends it an input; the core
//! serves no node of its own here, the simulated nodes A and B run its
//! sessions' turns with its turn services over the production store. The
//! matrix runs the uncut turn, then cuts it at every labelled write under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers
//! on the other node, and checks the laws:
//!
//! - **Limit:** a turn whose first group fills `max_tool_calls` and whose
//!   next group asks past it refuses the same calls however it was cut: no
//!   refused call ever runs, and the model is shown the refusal counting
//!   the first group once, whichever executions formed it. A step of
//!   native calls counts its own group; a cell counts its total.
//! - **Identity:** an RLM turn of two cells, three probe calls: every body
//!   entry of one call sees one call id, and the three calls are three ids.
//! - **Activity (FIG-5251):** a step of one native call cut at its
//!   `round.outcome`: the host's sink was handed the call's
//!   `ToolCallStarted` and `ToolCallCompleted` (a dead owner's provisional
//!   activity may precede the redrive's), and the session's committed
//!   observation holds exactly one outcome for the call. Cut at its
//!   `turn.commit`, the commit reaches a host following the session's
//!   recoverable chat from before the turn even when the owner lost the
//!   commit's acknowledgement or its life before it published the commit: a
//!   `TerminalReplacement`, or a `ReplayGap` with the durable head.
//! - **Plugin state (FIG-5266):** a tool's plugin-state change commits with
//!   its outcome (ADR 0132 §5). Cut at its `round.outcome`, the session's
//!   committed state holds the value the call's committed outcome carries,
//!   and a zombie's change, which never committed, is nowhere in it. In a
//!   round of two members, a member never observes the other's state before
//!   the other's outcome committed.
//! - The turn committed once and ended, and a zombie's writes after its reap
//!   are refused.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::llm::types::LlmResponse;
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ToolCall, ToolOutcome};
use lash_core_execution::StoreSet;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore, LeaseConfig};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use dialect::Dialect;
use served::Tier;

const SESSION: &str = "tool-crash-session";
const INPUT: &str = "tool-crash-turn";
const PROBE: &str = "crash_probe";
/// The label of the [`Turn::Activity`] call.
const ACTIVITY: &str = "activity";
/// The plugin whose namespace the state scenarios change.
const STATE_PLUGIN: &str = "crash-state-law";
/// The plugin's tool that sets the namespace's key to its entry's value.
const STATE_SET: &str = "state_set";
/// The plugin's tool that first looks for the other member's value.
const STATE_OBSERVE: &str = "state_observe";
/// The key both tools set.
const STATE_KEY: &str = "t";
/// How many times [`STATE_OBSERVE`] yields while it looks for the other
/// member's value before it stops looking.
const OBSERVE_YIELDS: usize = 512;
/// How long the recoverable chat may take to yield what is already on the
/// live stream.
const PUBLISHED_WITHIN: Duration = Duration::from_secs(10);

/// The `max_tool_calls` the limit scenarios' session records. One call
/// fills it: a first group of one commits its outcome in one write, so
/// every rerun of the matrix makes the same writes on every database.
const LIMIT: usize = 1;

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which turn the scenario runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// Two steps of native calls: `LIMIT`, then `LIMIT + 1`.
    LimitStep,
    /// One cell of two aggregates: `LIMIT` calls, then one more.
    LimitCell,
    /// Two cells: one probe call, then two.
    CellIdentity,
    /// One step of one native call, followed by a host's sink.
    Activity,
    /// One step of one [`STATE_SET`] call.
    State,
    /// One step of two members: [`STATE_SET`], and [`STATE_OBSERVE`]
    /// looking for its value.
    StatePair,
}

impl Turn {
    fn code(self) -> bool {
        matches!(self, Self::LimitCell | Self::CellIdentity)
    }

    fn script(self) -> Vec<LlmResponse> {
        let call = |label: &str| format!("tools.{PROBE}({{ label: \"{label}\" }})");
        let leaves = |range: std::ops::Range<usize>| {
            range.map(|leaf| format!("leaf-{leaf}")).collect::<Vec<_>>()
        };
        match self {
            Self::LimitStep => [leaves(0..LIMIT), leaves(LIMIT..2 * LIMIT + 1)]
                .iter()
                .map(|step| {
                    served::response(
                        step.iter()
                            .map(|label| {
                                served::call(
                                    &format!("call-{label}"),
                                    PROBE,
                                    serde_json::json!({ "label": label }),
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
            Self::LimitCell => {
                let aggregate = |labels: Vec<String>| {
                    format!(
                        "await Promise.all([{}])",
                        labels
                            .iter()
                            .map(|label| call(label))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                vec![served::cell(&format!(
                    "const first = {};\nconst rest = {};\nfinish([first, rest]);",
                    aggregate(leaves(0..LIMIT)),
                    aggregate(leaves(LIMIT..LIMIT + 1)),
                ))]
            }
            Self::Activity => vec![served::response(vec![served::call(
                &format!("call-{ACTIVITY}"),
                PROBE,
                serde_json::json!({ "label": ACTIVITY }),
            )])],
            Self::State => vec![served::response(vec![served::call(
                "call-state",
                STATE_SET,
                serde_json::json!({ "label": "T" }),
            )])],
            Self::StatePair => vec![served::response(vec![
                served::call("call-a", STATE_SET, serde_json::json!({ "label": "A" })),
                served::call("call-b", STATE_OBSERVE, serde_json::json!({ "label": "B" })),
            ])],
            Self::CellIdentity => vec![
                served::cell(&format!("await {};", call("cell-one"))),
                served::cell(&format!(
                    "await {};\nfinish(await {});",
                    call("cell-two-a"),
                    call("cell-two-b")
                )),
            ],
        }
    }

    /// The calls the turn must run, and the ones it must refuse.
    fn calls(self) -> (Vec<String>, Vec<String>) {
        let leaves = |range: std::ops::Range<usize>| {
            range.map(|leaf| format!("leaf-{leaf}")).collect::<Vec<_>>()
        };
        match self {
            Self::LimitStep => (leaves(0..LIMIT), leaves(LIMIT..2 * LIMIT + 1)),
            Self::LimitCell => (leaves(0..LIMIT), leaves(LIMIT..LIMIT + 1)),
            Self::CellIdentity => (
                vec![
                    "cell-one".to_owned(),
                    "cell-two-a".to_owned(),
                    "cell-two-b".to_owned(),
                ],
                Vec::new(),
            ),
            Self::Activity => (vec![ACTIVITY.to_owned()], Vec::new()),
            Self::State => (vec!["T".to_owned()], Vec::new()),
            Self::StatePair => (vec!["A".to_owned(), "B".to_owned()], Vec::new()),
        }
    }

    fn changes_state(self) -> bool {
        matches!(self, Self::State | Self::StatePair)
    }

    /// The refusal the model must be shown: after `counted` calls, `rest`
    /// more refused.
    fn refusal(self) -> Option<String> {
        let (counted, requested) = match self {
            Self::LimitStep => (0, LIMIT + 1),
            Self::LimitCell => (LIMIT, 1),
            Self::CellIdentity | Self::Activity | Self::State | Self::StatePair => return None,
        };
        let exceeded = lash::ToolCallLimitExceeded {
            scope: lash::ToolCallLimitScope::Cell,
            limit: lash::MaxToolCalls::new(LIMIT),
            counted,
            requested,
        };
        refusal_in(&exceeded.to_string())
    }

    fn max_tool_calls(self) -> usize {
        match self {
            Self::LimitStep | Self::LimitCell => LIMIT,
            Self::CellIdentity | Self::Activity | Self::State | Self::StatePair => 64,
        }
    }
}

/// The refusal sentence in `text`, through the count it refused.
fn refusal_in(text: &str) -> Option<String> {
    let start = text.find("tool call limit exceeded")?;
    let refusal = &text[start..];
    let end = refusal.find(" more")? + " more".len();
    Some(refusal[..end].to_owned())
}

/// Every body entry of every probe call, across every node: the outside
/// world, which no kill undoes.
#[derive(Default)]
struct World {
    entries: Mutex<BTreeMap<String, Vec<lash_core::ToolCallId>>>,
    /// What each [`STATE_OBSERVE`] entry saw of the other member's value,
    /// and whether any `round.outcome` had committed when it saw it.
    observations: Mutex<Vec<(Option<serde_json::Value>, bool)>>,
    /// The deployment, whose trace an observation reads.
    nodes: OnceLock<Weak<SimNodes>>,
}

impl World {
    fn entries(&self) -> BTreeMap<String, Vec<lash_core::ToolCallId>> {
        self.entries.lock_recover().clone()
    }

    /// Record a body entry of the call `label`, and answer the value its
    /// entry sets: the label and the entry's count, so a rerun sets a value
    /// of its own.
    fn enter(&self, label: &str, call: &lash_core::ToolCallId) -> String {
        let mut entries = self.entries.lock_recover();
        let seen = entries.entry(label.to_owned()).or_default();
        seen.push(call.clone());
        format!("{label}-{}", seen.len())
    }

    /// Whether a `round.outcome` write has committed by now: read from the
    /// trace at once, without yielding.
    fn outcome_committed(&self) -> bool {
        self.nodes
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|nodes| {
                nodes.script().trace().iter().any(|write| {
                    write.point.label == CommitLabel::ROUND_OUTCOME && write.committed()
                })
            })
    }
}

fn probe_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{PROBE}"),
        PROBE,
        "Records each body entry's call id and answers its label.",
        object.clone(),
        object,
    )
    .expect("the probe's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], PROBE))
    // A call a kill interrupted runs again at its ordinal, so every call
    // the turn makes is answered and the turn reaches the call past the
    // limit whatever was cut.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

struct Probe {
    world: Arc<World>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Probe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == PROBE).then(|| Arc::new(probe_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        self.world
            .entries
            .lock_recover()
            .entry(label.clone())
            .or_default()
            .push(call.context.call_id().clone());
        ToolOutcome::ok(serde_json::json!({ "label": label })).into()
    }
}

fn state_definition(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Sets the plugin's key to its entry's value.",
        object.clone(),
        object,
    )
    .expect("the state tool's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
    // A call a kill interrupted runs again at its ordinal and sets a value
    // of its own entry.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

/// The plugin whose tools change its namespace.
#[derive(Clone)]
struct StatePlugin {
    world: Arc<World>,
}

impl lash::plugins::PluginDefinition for StatePlugin {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(STATE_PLUGIN)
    }
}

impl lash::plugins::PluginFactory for StatePlugin {
    fn id(&self) -> &'static str {
        STATE_PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for StatePlugin {
    fn id(&self) -> &'static str {
        STATE_PLUGIN
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        let view = reg.state();
        reg.tools().provider(Arc::new(StateTools {
            world: Arc::clone(&self.world),
            view,
        }))
    }
}

/// The plugin's tools, reading its namespace through its published view.
struct StateTools {
    world: Arc<World>,
    view: lash::plugins::PluginStateView,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for StateTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        [STATE_SET, STATE_OBSERVE]
            .map(|name| state_definition(name).manifest())
            .into()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        [STATE_SET, STATE_OBSERVE]
            .contains(&name)
            .then(|| Arc::new(state_definition(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        let value = self.world.enter(&label, call.context.call_id());
        if call.name() == STATE_OBSERVE {
            // Look for the other member's value while it runs, and note
            // whether its outcome had committed by the time it showed.
            let mut seen = None;
            for _ in 0..OBSERVE_YIELDS {
                seen = self.view.get(STATE_KEY);
                if seen.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let committed = self.world.outcome_committed();
            self.world
                .observations
                .lock_recover()
                .push((seen, committed));
        }
        lash_core::ToolAttemptOutcome::Done {
            result: lash_core::ToolOutcomeDone::ok(serde_json::json!({ "value": value }))
                .with_state(lash::plugins::StateCommands::new().set(STATE_KEY, value.into())),
            intents: lash_core::ToolIntents::default(),
        }
    }
}

/// The scenario on one turn and one dialect, fresh for every matrix cell.
struct Crash {
    turn: Turn,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    /// The host's session and its send, which [`Turn::Activity`] follows.
    host: Mutex<Option<(lash::DurableSession, lash::SendHandle)>>,
    /// The session opened for observation, and the updates of its
    /// recoverable chat, followed from the head before the turn, which
    /// [`Turn::Activity`] reads.
    chat: Mutex<
        Option<(
            lash::LashSession,
            tokio::sync::mpsc::UnboundedReceiver<lash::recoverable_chat::RecoverableChatUpdate>,
        )>,
    >,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Crash {
    fn new(turn: Turn, dialect: Dialect, postgres_url: Option<String>) -> Self {
        let scripts = Arc::new(served::Scripts::default());
        scripts.register(INPUT, turn.script());
        Self {
            turn,
            dialect,
            postgres_url,
            world: Arc::default(),
            scripts,
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            host: Mutex::default(),
            chat: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core over the scenario's backend: it serves no
    /// node of its own, the simulated nodes run its sessions' turns.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let builder = if self.turn.code() {
                    lash::LashCore::rlm_builder(backend.clone(), served::rlm(&backend, None))
                } else {
                    lash::LashCore::standard_builder(backend.clone())
                };
                builder
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .serve_test_llm_profile(
                        served::model(Arc::clone(&self.scripts)),
                        served::metadata(),
                    )
                    .tools(Arc::new(Probe {
                        world: Arc::clone(&self.world),
                    }))
                    .plugin(Arc::new(StatePlugin {
                        world: Arc::clone(&self.world),
                    }))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "tool-crash-deployment",
                        "tool-crash-boot",
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// The laws of one run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();

        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits != 1 {
            violations.push(format!("the turn committed {commits} times"));
        }
        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }

        // Every call the turn must run ran under one call id per call, and
        // distinct calls are distinct ids.
        let entries = self.world.entries();
        let (run, refused) = self.turn.calls();
        let mut ids = BTreeSet::new();
        for label in &run {
            match entries.get(label) {
                None => violations.push(format!("call `{label}` never ran")),
                Some(seen) => {
                    if seen.iter().any(|id| *id != seen[0]) {
                        violations.push(format!(
                            "call `{label}`'s body entries saw more than one call id: {seen:?}"
                        ));
                    }
                    if !ids.insert(seen[0].clone()) {
                        violations.push(format!("call `{label}` shares another call's id"));
                    }
                }
            }
        }
        // No refused call ever ran, in any execution.
        for label in &refused {
            if let Some(seen) = entries.get(label) {
                violations.push(format!("refused call `{label}` ran {} times", seen.len()));
            }
        }
        // The model was shown the refusal, counting the first group once.
        if let Some(expected) = self.turn.refusal() {
            let shown = self
                .scripts
                .requests(INPUT)
                .iter()
                .filter_map(|request| refusal_in(&request.replace("\\\"", "\"")))
                .collect::<BTreeSet<_>>();
            if shown != BTreeSet::from([expected.clone()]) {
                violations.push(format!(
                    "the model was shown {shown:?}, not only `{expected}`"
                ));
            }
        }

        if self.turn == Turn::Activity {
            violations.extend(self.activity_laws(cut).await);
        }
        if self.turn.changes_state() {
            violations.extend(self.state_laws().await);
        }
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        if !violations.is_empty() {
            violations.push(format!("body entries: {entries:?}"));
            if let Some(last) = self.scripts.requests(INPUT).last() {
                violations.push(format!("last request: {last}"));
            }
        }
        violations
    }

    /// The [`Turn::State`] and [`Turn::StatePair`] laws, once the turn
    /// ended: the session's committed state holds the value of a call's
    /// committed outcome, the one the model was shown, and no other; and no
    /// member saw the other's value before an outcome committed.
    async fn state_laws(&self) -> Vec<String> {
        let mut violations = Vec::new();
        for (seen, committed) in self.world.observations.lock_recover().iter() {
            if seen.is_some() && !committed {
                violations.push(format!(
                    "a member observed the other member's state {seen:?} before its outcome \
                     committed"
                ));
            }
        }
        let shown = self
            .scripts
            .requests(INPUT)
            .last()
            .map(|request| shown_values(request))
            .unwrap_or_default();
        let committed = match self.committed_state().await {
            Ok(value) => value,
            Err(error) => return vec![error],
        };
        match &committed {
            Some(serde_json::Value::String(value)) if shown.contains(value) => {}
            other => violations.push(format!(
                "the committed state holds {other:?}, not a value of an outcome the model \
                 was shown ({shown:?})"
            )),
        }
        if self.turn == Turn::State && shown.len() != 1 {
            violations.push(format!("the model was shown {shown:?}, not one value"));
        }
        violations
    }

    /// The value of [`STATE_KEY`] in the session's committed plugin state.
    async fn committed_state(&self) -> Result<Option<serde_json::Value>, String> {
        let factory = self.backend().stores().session_store_factory();
        let view = lash_core_execution::store::SessionStore::new(factory, session())
            .map_err(|error| format!("the session's store: {error}"))?;
        let loaded = lash_core_execution::store::load_session_window_state(
            &view,
            lash_core_execution::store::WindowSelector::Current,
        )
        .await
        .map_err(|error| format!("the session's committed state: {error}"))?
        .ok_or("the session has no committed state")?;
        Ok(loaded
            .state
            .plugin_state()
            .and_then(|state| state.plugins.get(STATE_PLUGIN))
            .and_then(|namespace| namespace.values.get(STATE_KEY))
            .cloned())
    }

    /// The [`Turn::Activity`] laws, once the turn ended: the host follows
    /// its send to the settled turn, and reads its session's committed
    /// observation.
    async fn activity_laws(&self, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        // The chat is read before the host's follow below, whose resolution
        // adopts the head into this process's resident session, which
        // publishes a commit of its own.
        violations.extend(self.chat_laws().await);
        let Some((session, handle)) = self.host.lock_recover().take() else {
            return vec!["the host sent nothing".to_owned()];
        };
        let sink = Collected::default();
        match tokio::time::timeout(served::WATCHDOG, handle.outcome_into(&sink)).await {
            Ok(Ok(lash::SendOutcome::Settled { .. })) => {}
            other => violations.push(format!("the host's follow did not settle: {other:?}")),
        }
        // The live stream is provisional: a dead owner's activity may come
        // before the redrive's, but the call's start and completion arrive,
        // from the run that answers the call. A cut at `turn.commit` resumes
        // past the answered round and runs the call in no later run, and
        // what a lost owner never published is not promised (ADR 0122, decision 4).
        let provider = format!("call-{ACTIVITY}");
        let rerun = cut.is_none_or(|cut| cut.point.label != CommitLabel::TURN_COMMIT);
        let activities = sink.0.lock_recover().clone();
        let started = activities.iter().any(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::ToolCallStarted { provider_call_id: Some(id), .. }
                    if *id == provider
            )
        });
        let completed = activities.iter().any(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::ToolCallCompleted { provider_call_id: Some(id), .. }
                    if *id == provider
            )
        });
        if rerun && (!started || !completed) {
            violations.push(format!(
                "the host's sink was not handed the call's start and completion: {activities:#?}"
            ));
        }
        // The committed observation settles it: one outcome for the call.
        match session.read().await {
            Ok(Some(view)) => {
                let parts = view
                    .messages()
                    .iter()
                    .flat_map(|message| message.parts.iter())
                    .collect::<Vec<_>>();
                let called = parts
                    .iter()
                    .filter(|part| {
                        part.kind() == lash_core::PartKind::ToolCall
                            && part.provider_call_id() == Some(provider.as_str())
                    })
                    .filter_map(|part| part.call_id().cloned())
                    .collect::<Vec<_>>();
                let answered = parts
                    .iter()
                    .filter(|part| {
                        part.kind() == lash_core::PartKind::ToolResult
                            && called.first().is_some_and(|id| part.call_id() == Some(id))
                    })
                    .count();
                if called.len() != 1 || answered != 1 {
                    violations.push(format!(
                        "the committed observation holds {} calls and {answered} outcomes for \
                         the call, not one of each",
                        called.len()
                    ));
                }
            }
            other => violations.push(format!("the committed observation: {other:?}")),
        }
        violations
    }

    /// The host following the session's recoverable chat from before the
    /// turn reached the durable head: by the commit's `TerminalReplacement`,
    /// or by a `ReplayGap` whose snapshot is the head. The session's owner
    /// published it before it released the session, so it is on the live
    /// stream by now: the chat waits for it only [`PUBLISHED_WITHIN`].
    async fn chat_laws(&self) -> Vec<String> {
        use lash::recoverable_chat::RecoverableChatUpdate;
        let Some((_observed, mut chat)) = self.chat.lock_recover().take() else {
            return vec!["the host follows no recoverable chat".to_owned()];
        };
        let factory = self.backend().stores().session_store_factory();
        let head = match lash_core::SessionCommitStore::load_session_head_meta(
            factory.as_ref(),
            &crate::session(),
        )
        .await
        {
            Ok(Some(head)) => lash_core::SessionRevision::of_durable_head(&head),
            other => return vec![format!("the session's head: {other:?}")],
        };
        let mut seen = Vec::new();
        loop {
            let update = match tokio::time::timeout(PUBLISHED_WITHIN, chat.recv()).await {
                Ok(Some(update)) => update,
                other => {
                    return vec![format!(
                        "the recoverable chat never reached the head {head:?}: {other:?} \
                         after {seen:#?}"
                    )];
                }
            };
            let reached = match &update {
                RecoverableChatUpdate::TerminalReplacement { event, .. } => {
                    event.revision() >= head
                }
                RecoverableChatUpdate::ReplayGap { gap, .. } => gap.latest_revision >= head,
                _ => false,
            };
            seen.push(update);
            if reached {
                return Vec::new();
            }
        }
    }
}

/// The values the state tools' outcomes carry in `request`, as the model
/// was shown them: every `<label>-<entry>` of the scenarios' labels.
fn shown_values(request: &str) -> BTreeSet<String> {
    let request = request.replace("\\\"", "\"");
    let mut shown = BTreeSet::new();
    for label in ["T", "A", "B"] {
        let marker = format!("\"{label}-");
        let mut rest = request.as_str();
        while let Some(at) = rest.find(&marker) {
            rest = &rest[at + marker.len()..];
            let entry: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !entry.is_empty() {
                shown.insert(format!("{label}-{entry}"));
            }
        }
    }
    shown
}

/// A host's activity sink: everything it was handed, in order.
#[derive(Default)]
struct Collected(Mutex<Vec<lash_core::TurnActivity>>);

#[async_trait::async_trait]
impl lash_core::facade_support::TurnActivitySink for Collected {
    async fn emit(&self, activity: lash_core::TurnActivity) {
        self.0.lock_recover().push(activity);
    }
}

#[async_trait::async_trait]
impl Scenario for Crash {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(served::backend(stores));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: LeaseConfig::default(),
            decodes: self.backend().formats().decodes(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(SessionActivation::new(
            self.backend(),
            lash::testing::session_turn_services(&self.core()),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let _ = self.world.nodes.set(Arc::downgrade(nodes));
        // The host is outside the deployment under test: its send is uncut.
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(served::spec(
                self.turn.max_tool_calls(),
            )))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        if self.turn == Turn::Activity {
            let observed = self
                .core()
                .session(crate::session())
                .open()
                .await
                .map_err(|error| format!("open the session for observation: {error}"))?;
            let snapshot = observed
                .observe()
                .recoverable_chat_snapshot()
                .await
                .map_err(|error| format!("snapshot the session: {error}"))?;
            let mut chat = observed
                .observe()
                .subscribe_recoverable_chat(snapshot.cursor);
            // The host follows the chat while the deployment runs, as a
            // host's live view does.
            let (updates, received) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                use lash::observe::Stream as _;
                while let Some(Ok(update)) =
                    std::future::poll_fn(|cx| std::pin::Pin::new(&mut chat).poll_next(cx)).await
                {
                    if updates.send(update).is_err() {
                        return;
                    }
                }
            });
            *self.chat.lock_recover() = Some((observed, received));
        }
        let handle = session
            .send(lash::TurnInput::text(INPUT))
            .await
            .map_err(|error| format!("send the turn's input: {error}"))?;
        *self.host.lock_recover() = Some((session, handle));
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        self.laws(nodes, cut).await
    }
}

/// Once a zombie's actors moved, every owner write it attempts is refused
/// with `OwnershipLost`.
fn zombie_laws(cut: &Cut, trace: &[lash_durable_test::Write]) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.fault != Fault::Zombie || cut.kind != WriteKind::Actor {
        return violations;
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec!["the zombie's cut write is not in the trace".to_owned()];
    };
    for write in trace[at..]
        .iter()
        .filter(|write| write.node == cut.node && write.kind == WriteKind::Actor)
    {
        match &write.stored {
            Stored::Refused(DurableError::OwnershipLost(_)) => {}
            other => violations.push(format!("zombie write {write} was {other:?}")),
        }
    }
    violations
}

/// Cut `turn` on `tier` at every label of its uncut run.
async fn prove(turn: Turn, tier: Tier) {
    prove_at(turn, tier, &[]).await;
}

/// Cut `turn` on `tier` at every write of its uncut run labelled one of
/// `cut_labels`, or at every label when `cut_labels` is empty.
async fn prove_at(turn: Turn, tier: Tier, cut_labels: &[CommitLabel]) {
    prove_under(
        turn,
        tier,
        cut_labels,
        &[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ],
    )
    .await;
}

/// [`prove_at`] under `faults` only.
async fn prove_under(turn: Turn, tier: Tier, cut_labels: &[CommitLabel], faults: &[Fault]) {
    let (dialect, postgres_url) = match tier {
        Tier::SqliteMemory => (Dialect::SqliteMemory, None),
        Tier::SqliteFile => (Dialect::SqliteFile, None),
        Tier::Postgres => {
            let Some(url) = dialect::postgres_url() else {
                eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
                return;
            };
            (Dialect::Postgres, Some(url))
        }
    };
    let matrix = if cut_labels.is_empty() {
        Matrix::new()
    } else {
        Matrix::new().labels(cut_labels)
    };
    let report = matrix
        .faults(faults)
        .horizon(Duration::from_secs(600))
        .run(|| Crash::new(turn, dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "{turn:?} on {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    let expected = if cut_labels.is_empty() {
        vec![
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::TURN_COMMIT,
        ]
    } else {
        cut_labels.to_vec()
    };
    for label in expected {
        assert!(
            report.labels().contains(&label),
            "{turn:?}: the matrix never cut {label}"
        );
    }
}

/// A turn that is cut anywhere refuses the same call the same way: the
/// first group's calls are counted once, however many executions formed
/// the group, and the refused calls run in no execution. A step of native
/// calls counts its own group.
async fn tool_call_limit_refuses_the_same_call_across_a_crash(tier: Tier) {
    prove(Turn::LimitStep, tier).await;
}

/// The same law in a cell, which counts its total: after a cut anywhere the
/// cell's next call past the limit is refused with the first aggregate's
/// calls counted once.
async fn tool_call_limit_refuses_the_same_call_across_a_crash_in_a_cell(tier: Tier) {
    prove(Turn::LimitCell, tier).await;
}

/// Code cells cut anywhere keep each call's identity, and a fresh call is
/// a different call: every body entry of one call sees one call id, and the
/// turn's three calls are three ids.
async fn code_cells_keep_identity_and_distinguish_fresh_calls_across_a_kill(tier: Tier) {
    prove(Turn::CellIdentity, tier).await;
}

/// A native call cut at its `round.outcome` and redriven on the other node
/// streams its start and completion to the host's sink, and the session's
/// committed observation holds exactly one outcome for it (FIG-5251).
async fn a_native_call_cut_at_its_outcome_streams_its_activity_and_commits_one_outcome(tier: Tier) {
    prove_at(Turn::Activity, tier, &[CommitLabel::ROUND_OUTCOME]).await;
}

/// A turn cut at its `turn.commit`, its acknowledgement or its owner lost
/// after the commit landed and before it was published, still reaches the
/// host following the session's recoverable chat (FIG-5251).
async fn a_commit_whose_publication_was_lost_still_reaches_the_host(tier: Tier) {
    prove_at(Turn::Activity, tier, &[CommitLabel::TURN_COMMIT]).await;
}

/// A tool's plugin-state change cut at its `round.outcome`, before or after
/// the commit, its acknowledgement or its owner lost: once the turn
/// resumes, the session's committed state is the value the call's committed
/// outcome carries (FIG-5266).
async fn a_tool_state_change_cut_at_its_outcome_is_the_committed_value_after_resume(tier: Tier) {
    prove_under(
        Turn::State,
        tier,
        &[CommitLabel::ROUND_OUTCOME],
        &[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Abort,
            Fault::CommitThenAbort,
        ],
    )
    .await;
}

/// A zombie member's state change commits nothing: its `round.outcome` is
/// refused, and the session's committed state is the value of the outcome
/// the new owner committed, never the zombie's (FIG-5266).
async fn a_zombie_members_state_change_commits_nothing(tier: Tier) {
    prove_under(
        Turn::State,
        tier,
        &[CommitLabel::ROUND_OUTCOME],
        &[Fault::Zombie],
    )
    .await;
}

/// In a round of two members, one never observes the other's state before
/// the other's outcome committed, however the round is cut (FIG-5266).
async fn a_member_never_observes_another_members_uncommitted_state(tier: Tier) {
    prove_at(Turn::StatePair, tier, &[CommitLabel::ROUND_OUTCOME]).await;
}

tiered_laws!(
    current_thread:
    a_tool_state_change_cut_at_its_outcome_is_the_committed_value_after_resume,
    a_zombie_members_state_change_commits_nothing,
    a_member_never_observes_another_members_uncommitted_state,
    a_native_call_cut_at_its_outcome_streams_its_activity_and_commits_one_outcome,
    a_commit_whose_publication_was_lost_still_reaches_the_host,
    tool_call_limit_refuses_the_same_call_across_a_crash,
    tool_call_limit_refuses_the_same_call_across_a_crash_in_a_cell,
    code_cells_keep_identity_and_distinguish_fresh_calls_across_a_kill,
);
