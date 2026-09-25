//! One cross-tier law: a tool batch's leaves really overlap (FIG-3400).
//!
//! Overlap is proven by *rendezvous*, never by wall time. Every leaf in a
//! width-n batch reports that it started and then refuses to produce its answer
//! until the whole width has reported. A tier that runs the leaves one at a time
//! cannot get past the first leaf, so it deadlocks and fails on the turn's
//! deadlock budget, whose message names the leaves that never started; a tier
//! that overlaps them finishes and leaves behind an observation log in which
//! all n starts precede the first answer.
//!
//! No wall-clock appears in that assertion, and none may: the leaves wait on
//! the rendezvous itself, so under a starved executor — a one-CPU remote
//! worker, say — the scenario is slow, never wrong (FIG-3423). Two
//! consequences: a leaf's start is recorded and its `Started` event logged in
//! one critical section, or a preempted leaf would let the waiters release on
//! a start the log has not yet shown and the log would read as an answer
//! preceding a start; and the only clock anywhere is the turn's deadlock
//! budget, generous by construction, which is what still catches a genuinely
//! serial tier.
//!
//! Three things follow from the same log and are asserted here rather than
//! re-derived by each backend:
//!
//! * observed peak in-flight equals n (the counterpart of the
//!   `max_in_flight_tool_attempts` double in lash-core's runtime tests);
//! * the activation shape — all n dispatches are observed before any settlement
//!   is served — with the consumer's reply order asserted exactly; and
//! * a serial-versus-concurrent differential: the same width-n program run with
//!   leaves that never rendezvous returns the identical answers, so the
//!   rendezvous changes the schedule and nothing else.
//!
//! The law is parameterised over two axes. The *tier* arrives as an
//! [`crate::EffectHost`], so native, SQLite and PostgreSQL run the identical
//! assertions. The *producer* arrives as a [`ToolBatchProducer`]: the product
//! surface that spells a width-n parallel batch — parallel model tool calls on
//! the standard protocol, an orchestrating relay that dispatches granted and
//! deferred leaves, `Promise.all` on the RLM bridge, a Lashlang aggregate on the
//! process bridge. A producer contributes its plugin factories and the model
//! script that issues the plan; everything else is shared.
//!
//! One producer needs more than a script: an aggregate that is the body of a
//! started process is issued by a second, independently written
//! `call_tool_batch` caller, and reaching it takes a process registry, process
//! work bound to that registry, the engines the producer's plugins contribute,
//! and a worker driving the registry while the turn is parked on the process.
//! The law stands all four up when a producer declares a registry, and a
//! producer that issues its batch from the turn pays none of it.
//!
//! The *turn* runs where the tier runs turns, through the tier's
//! [`crate::ConformanceTurnRunner`]: in process on the host itself; a
//! handler-bound tier hands in a runner that drives the turn inside a live
//! handler. Restate is not registered yet: its direct batches overlap as
//! effect groups of child invocations (FIG-3397), but an orchestrating relay's
//! nested batch still runs its leaves on the relay child's own invocation
//! journal. Restate replays that journal by position, so its controller
//! drives the leaves one at a time (`drive_independent_effect_work`,
//! FIG-3671). There is no expected-failure mechanism here and none may be
//! added.

use crate::admit;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::ToolDefinitionBindingExt as _;
use lash_sansio::sync::MutexExt as _;

use pretty_assertions::assert_eq;

/// How long one scenario's turn may take before the law gives up on it.
///
/// This is the law's only clock, and it is a deadlock budget, not a scheduling
/// assumption: a leaf that is merely slow to be scheduled must never fail the
/// law, so the leaves themselves wait without a wall-clock bound (FIG-3423).
/// A genuinely serial tier cannot leave its first leaf, so the turn outlasts
/// nothing useful — the budget's expiry reports the leaves that never started.
const TURN_BUDGET: Duration = Duration::from_secs(60);

/// One leaf of a planned batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolBatchLeaf {
    /// The tool name the producer must call.
    pub tool: String,
    /// The route this leaf takes inside its group child's invocation driver.
    pub route: ToolBatchRoute,
}

/// The dispatch route a leaf takes inside its group child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBatchRoute {
    /// A catalogue-authorised leaf provider call.
    Leaf,
    /// A leaf carrying a `ToolExecutionGrant`, which never enters orchestration.
    Granted,
    /// An orchestrating body, dispatched through the orchestration lane.
    Orchestrating,
    /// A leaf that parks on a completion key and settles out of band.
    Deferred,
}

/// How the producer must enter the batch.
///
/// A grant and an orchestrating body cannot be spelled as a model tool call, so
/// the routes that need them are entered through one call to the plan's
/// orchestrating relay, which dispatches the whole width itself. Every producer
/// that can name a tool can therefore reach every route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBatchEntry {
    /// Issue each leaf directly, as the surface's own parallel construct.
    Direct,
    /// Issue one call to [`ToolBatchPlan::relay_tool`] with
    /// [`ToolBatchPlan::relay_args`].
    Relay,
}

/// What the producer is asked to issue: one batch, this wide, over these tools.
#[derive(Clone, Debug)]
pub struct ToolBatchPlan {
    /// A label naming the scenario, used in assertion messages.
    pub scenario: String,
    /// The leaves, in the order the producer must issue them.
    pub leaves: Vec<ToolBatchLeaf>,
    /// How the producer enters the batch.
    pub via: ToolBatchEntry,
    /// The name of the orchestrating relay tool, for producers that issue the
    /// granted/orchestrating/deferred scenario through a single call.
    pub relay_tool: String,
}

impl ToolBatchPlan {
    /// The plan's width.
    pub fn width(&self) -> usize {
        self.leaves.len()
    }

    /// The relay call's arguments: the leaves it must dispatch as one batch.
    pub fn relay_args(&self) -> serde_json::Value {
        serde_json::json!({
            "leaves": self
                .leaves
                .iter()
                .map(|leaf| serde_json::json!({
                    "tool": leaf.tool,
                    "route": format!("{:?}", leaf.route),
                }))
                .collect::<Vec<_>>()
        })
    }
}

/// The model script a producer hands the law: the responses, in order, that
/// make the runtime issue one plan as one batch.
pub type ToolBatchScript = Arc<dyn Fn(&ToolBatchPlan) -> Vec<crate::LlmResponse> + Send + Sync>;

/// A fresh process registry for one scenario, supplied by the tier.
///
/// Only a producer whose batch runs *inside a process* needs one. It is a
/// factory rather than a handle because each scenario opens its own session,
/// and a durable registry must not carry the previous scenario's rows.
pub type ToolBatchProcessRegistryFactory =
    Arc<dyn Fn() -> Arc<dyn crate::ProcessRegistry> + Send + Sync>;

/// A product surface that issues a width-n parallel tool batch.
///
/// The law owns the leaves, the runtime, the tier and every assertion; a
/// producer contributes only the plugin factories its surface needs and the
/// model script that makes the runtime issue `plan` as one batch.
#[derive(Clone)]
pub struct ToolBatchProducer {
    /// Names the surface in assertion messages.
    pub label: String,
    /// Plugin factories beyond the law's own leaf provider.
    pub factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    /// The provider script that issues `plan` as one batch. The law appends the
    /// terminal text response, so a script that ends after the batch is enough.
    pub script: ToolBatchScript,
    /// Every surface that can name a tool can; the flag exists for surfaces whose front door
    /// is a fixed shape (a bare aggregate over catalogue leaves).
    pub reaches_relay: bool,
    /// Present only when the producer's batch is issued from inside a process.
    ///
    /// The law then stands the process substrate up itself — the registry this
    /// factory yields, the process work bound to it, and the engine
    /// contributions the producer's own plugins declare. A producer that issues
    /// its batch from the turn leaves this absent, so no tier has to supply a
    /// process engine it never runs.
    pub process_registry: Option<ToolBatchProcessRegistryFactory>,
    /// When true the law runs the scenario's turn under a borrowed scoped
    /// controller — the shape a Restate-style host produces — so the runtime
    /// cannot hold a `'static` controller and every effect the turn and its
    /// process commands issue must cross `EffectTaskController` (FIG-3415). A
    /// producer that leaves this false exercises only the `'static` shortcut
    /// and never reaches the proxy.
    pub through_task_proxy: bool,
}

impl std::fmt::Debug for ToolBatchProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolBatchProducer")
            .field("label", &self.label)
            .field("factories", &self.factories.len())
            .field("reaches_relay", &self.reaches_relay)
            .field("runs_in_a_process", &self.process_registry.is_some())
            .field("through_task_proxy", &self.through_task_proxy)
            .finish()
    }
}

/// The standard protocol's parallel model tool calls: one model response
/// carrying n `ToolCall` parts, which the turn driver prepares into exactly one
/// `PreparedToolBatch`.
pub fn parallel_model_tool_calls_producer() -> ToolBatchProducer {
    ToolBatchProducer {
        label: "parallel-model-tool-calls".to_string(),
        factories: crate::testing::test_standard_protocol_factories(),
        script: Arc::new(|plan| {
            let parts = match plan.via {
                ToolBatchEntry::Direct => plan
                    .leaves
                    .iter()
                    .enumerate()
                    .map(|(position, leaf)| crate::LlmOutputPart::ToolCall {
                        call_id: format!("parallel-call-{position}"),
                        tool_name: leaf.tool.clone(),
                        input_json: serde_json::json!({ "position": position }).to_string(),
                        replay: None,
                    })
                    .collect(),
                ToolBatchEntry::Relay => vec![crate::LlmOutputPart::ToolCall {
                    call_id: "relay-call".to_string(),
                    tool_name: plan.relay_tool.clone(),
                    input_json: plan.relay_args().to_string(),
                    replay: None,
                }],
            };
            vec![crate::LlmResponse {
                parts,
                response_metadata: Default::default(),
                ..crate::LlmResponse::default()
            }]
        }),
        reaches_relay: true,
        process_registry: None,
        through_task_proxy: false,
    }
}

/// The RLM bridge's `Promise.all`: one cell whose leaves are awaited together,
/// which the bridge turns into exactly one `call_tool_batch`.
///
/// The cell source lives here rather than at the registration site so every
/// tier issues the byte-identical program; the caller supplies only the RLM
/// protocol plugin factory, which is the part this crate cannot construct.
pub fn rlm_promise_all_producer(
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) -> ToolBatchProducer {
    ToolBatchProducer {
        label: "rlm-promise-all".to_string(),
        factories,
        script: Arc::new(|plan| {
            vec![crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::Text {
                    text: rlm_promise_all_cell(plan),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..crate::LlmResponse::default()
            }]
        }),
        reaches_relay: true,
        process_registry: None,
        through_task_proxy: false,
    }
}

/// The dialect the RLM cell channel wraps a cell's source in. Named once here
/// so the opening and closing tags cannot drift apart.
const RLM_CELL_DIALECT: &str = "typescript";

/// The cell text [`rlm_promise_all_producer`] issues.
///
/// `finish` is the only statement that closes an RLM turn, so both entries end
/// in one: a cell whose last line is a bare expression leaves the driver asking
/// the provider again, and the batch would be re-issued rather than settled.
fn rlm_promise_all_cell(plan: &ToolBatchPlan) -> String {
    let body = match plan.via {
        ToolBatchEntry::Direct => {
            let calls = plan
                .leaves
                .iter()
                .enumerate()
                .map(|(position, leaf)| {
                    format!("  tools.{}({{ position: {position} }})", leaf.tool)
                })
                .collect::<Vec<_>>()
                .join(",\n");
            format!("finish(await Promise.all([\n{calls}\n]));")
        }
        ToolBatchEntry::Relay => format!(
            "finish(await tools.{}({}));",
            plan.relay_tool,
            plan.relay_args()
        ),
    };
    format!("<{RLM_CELL_DIALECT}>\n{body}\n</{RLM_CELL_DIALECT}>")
}

/// The process bridge's Lashlang aggregate: the same aggregate, but running
/// inside a started process rather than inside the turn.
///
/// The cell defines the aggregate as a process definition and starts it, so the
/// batch is issued by the process host bridge
/// (`lash-lashlang-runtime/src/process.rs`, its `call_tool_batch` site) and not
/// by the cell's own host bridge. That is a second, independently written
/// `call_tool_batch` caller, which is why it is a producer of this law rather
/// than a variation of `rlm_promise_all_producer`.
///
/// `registry` is the tier's process registry; the law binds the process work to
/// it and installs the engine contributions the producer's plugins declare.
///
/// `through_task_proxy` is declared here because this is the producer whose
/// process commands are the reason `EffectTaskController` exists: running the
/// scenario's turn under a borrowed scoped controller makes `processes.start`
/// and the attach that awaits it cross the proxy on the way to the real
/// controller, which is the path a group reached through the proxy had to
/// serve (FIG-3415).
pub fn lashlang_process_aggregate_producer(
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    registry: ToolBatchProcessRegistryFactory,
) -> ToolBatchProducer {
    ToolBatchProducer {
        label: "lashlang-process-aggregate".to_string(),
        factories,
        script: Arc::new(|plan| {
            vec![crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::Text {
                    text: lashlang_process_aggregate_cell(plan),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..crate::LlmResponse::default()
            }]
        }),
        reaches_relay: true,
        process_registry: Some(registry),
        through_task_proxy: true,
    }
}

/// The cell text [`lashlang_process_aggregate_producer`] issues.
///
/// The aggregate is the *process definition*: nothing is awaited in the cell
/// itself, so the whole width is issued by the process bridge. `finish` closes
/// the turn on the process's terminal value, for the reason given on
/// [`rlm_promise_all_cell`].
fn lashlang_process_aggregate_cell(plan: &ToolBatchPlan) -> String {
    let aggregate = match plan.via {
        ToolBatchEntry::Direct => {
            let calls = plan
                .leaves
                .iter()
                .enumerate()
                .map(|(position, leaf)| {
                    format!("    tools.{}({{ position: {position} }})", leaf.tool)
                })
                .collect::<Vec<_>>()
                .join(",\n");
            format!("  return await Promise.all([\n{calls}\n  ]);")
        }
        ToolBatchEntry::Relay => format!(
            "  return await tools.{}({});",
            plan.relay_tool,
            plan.relay_args()
        ),
    };
    let body = format!(
        "const batch = async () => {{\n{aggregate}\n}};\n\
         const handle = await processes.start({{ definition: batch }});\n\
         finish(await handle);"
    );
    format!("<{RLM_CELL_DIALECT}>\n{body}\n</{RLM_CELL_DIALECT}>")
}

/// The observation log every assertion in this law reads.
///
/// One record per leaf transition, appended under the rendezvous's single
/// lock so the order is the order the runtime produced, not the order a
/// reader happened to sample.
#[derive(Debug, Default)]
struct RendezvousLog {
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RendezvousEvent {
    Started(String),
    Answered(String),
}

/// What one lock guards together.
///
/// `started` and `events` are one critical section by construction: a leaf
/// counts as started for the waiters only in the same lock acquisition that
/// appends its `Started` event. Under a starved executor a leaf can be
/// preempted between two statements — if the two lived under separate locks
/// the log would show a sibling's `Answered` before the `Started` that
/// released it, and the law would fail on a schedule it never observed
/// (FIG-3423).
#[derive(Debug, Default)]
struct RendezvousShared {
    started: Vec<String>,
    events: Vec<RendezvousEvent>,
    /// Wall-clock stamps the law never reads. They exist for
    /// [`measure_tool_batch`] (FIG-3398): the batch window is first-start to
    /// last-answer, and stamping them inside this same critical section keeps
    /// the measurement as untearable as the log.
    started_at: Vec<Instant>,
    answered_at: Vec<Instant>,
}

/// The rendezvous every leaf of one scenario shares.
#[derive(Debug)]
struct Rendezvous {
    /// Every leaf the scenario plans to run, in plan order.
    expected: Vec<String>,
    shared: std::sync::Mutex<RendezvousShared>,
    notify: tokio::sync::watch::Sender<usize>,
    log: RendezvousLog,
    /// When false the leaves do not wait for one another at all. That is the
    /// serial-safe half of the differential: identical program, identical
    /// answers, no schedule requirement.
    gated: bool,
}

impl Rendezvous {
    fn new(expected: Vec<String>, gated: bool) -> Self {
        Self {
            expected,
            shared: std::sync::Mutex::new(RendezvousShared::default()),
            notify: tokio::sync::watch::channel(0).0,
            log: RendezvousLog::default(),
            gated,
        }
    }

    fn record_started(&self, leaf: &str) {
        let at = Instant::now();
        let started = {
            let mut shared = self.shared.lock_recover();
            shared.started.push(leaf.to_string());
            shared
                .events
                .push(RendezvousEvent::Started(leaf.to_string()));
            shared.started_at.push(at);
            // The in-flight counters ride in the same critical section: a
            // waiter can only observe a complete `started` while holding this
            // lock, so the peak is unreachable mid-release.
            let current = self.log.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.log.peak_in_flight.fetch_max(current, Ordering::SeqCst);
            shared.started.len()
        };
        let _ = self.notify.send(started);
    }

    fn record_answered(&self, leaf: &str) {
        let at = Instant::now();
        let mut shared = self.shared.lock_recover();
        shared
            .events
            .push(RendezvousEvent::Answered(leaf.to_string()));
        shared.answered_at.push(at);
        self.log.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    /// First leaf start to last leaf answer — the batch's own window, excluding
    /// the model round-trips that frame it. `None` when no leaf ran.
    fn leaf_window(&self) -> Option<(Instant, Instant)> {
        let shared = self.shared.lock_recover();
        let first = shared.started_at.iter().copied().min()?;
        let last = shared.answered_at.iter().copied().max()?;
        Some((first, last))
    }

    fn missing(&self, required: &[String]) -> Vec<String> {
        let started = self.shared.lock_recover().started.clone();
        required
            .iter()
            .filter(|leaf| !started.contains(leaf))
            .cloned()
            .collect()
    }

    /// The leaves that reported started, in the order they did.
    fn started(&self) -> Vec<String> {
        self.shared.lock_recover().started.clone()
    }

    /// Waits until every leaf in `required` has reported started.
    ///
    /// There is deliberately no clock here: a leaf whose task has not been
    /// scheduled yet is indistinguishable from one that can never run, and
    /// only the turn's deadlock budget may rule between them (FIG-3423). A
    /// serial tier therefore parks its first leaf until the turn bound fires.
    async fn wait_for(&self, required: &[String]) {
        if !self.gated {
            return;
        }
        let mut receiver = self.notify.subscribe();
        loop {
            if self.missing(required).is_empty() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    fn events(&self) -> Vec<RendezvousEvent> {
        self.shared.lock_recover().events.clone()
    }

    fn peak_in_flight(&self) -> usize {
        self.log.peak_in_flight.load(Ordering::SeqCst)
    }

    /// Every planned leaf that never reported started, in plan order. This is
    /// the message a serial tier fails with.
    fn never_started(&self) -> Vec<String> {
        self.missing(&self.expected)
    }
}

/// The per-scenario state the leaf provider and the relay tool share.
#[derive(Debug)]
struct ScenarioState {
    rendezvous: std::sync::Mutex<Arc<Rendezvous>>,
    /// Which leaves each leaf must wait for. An empty entry means "all of
    /// them"; a non-empty one is the reverse-dependency case.
    dependencies: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
}

impl ScenarioState {
    fn required_for(&self, leaf: &str, rendezvous: &Rendezvous) -> Vec<String> {
        self.dependencies
            .lock_recover()
            .get(leaf)
            .cloned()
            .unwrap_or_else(|| rendezvous.expected.clone())
    }
}

fn leaf_definition(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A rendezvous leaf: answers only once its whole batch has started.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_tool_binding(crate::ToolBinding::new(["tools"], name))
}

/// The leaf provider. Every plain, granted and deferred leaf of every scenario
/// is one of these tools; the orchestrating leaves live in the relay factory
/// below because orchestration is a separate registration lane.
struct RendezvousLeaves {
    names: Vec<String>,
    deferred: Vec<String>,
    state: Arc<ScenarioState>,
    effect_host: Arc<dyn crate::EffectHost>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for RendezvousLeaves {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.names
            .iter()
            .map(|name| leaf_definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.names
            .iter()
            .any(|leaf| leaf == name)
            .then(|| Arc::new(leaf_definition(name).contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        self.deferred
            .iter()
            .any(|name| leaf_definition(name).id() == tool_id)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let name = call.name().to_string();
        let rendezvous = Arc::clone(&self.state.rendezvous.lock_recover());
        rendezvous.record_started(&name);
        let required = self.state.required_for(&name, &rendezvous);

        if self.deferred.contains(&name) {
            // A deferred leaf parks on its completion key and settles out of
            // band. It joins the rendezvous exactly like a synchronous leaf:
            // its start is already recorded, and the out-of-band task holds its
            // in-flight slot until the whole width has started.
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(error) => {
                    return crate::ToolOutcome::failure(crate::ToolFailure::runtime(
                        crate::ToolFailureClass::Internal,
                        "no_completion_key",
                        format!(
                            "this tier issues no completion key, so the deferred \
                             route cannot be exercised on it: {error}"
                        ),
                    ))
                    .into();
                }
            };
            let effect_host = Arc::clone(&self.effect_host);
            crate::task::spawn(async move {
                rendezvous.wait_for(&required).await;
                rendezvous.record_answered(&name);
                let resolution = crate::Resolution::Ok(leaf_answer(&name));
                let _ = effect_host
                    .await_event_resolver()
                    .resolve_await_event(&key, resolution)
                    .await;
            });
            return crate::ToolAttemptOutcome::Pending(crate::PendingCompletion::new());
        }

        rendezvous.wait_for(&required).await;
        rendezvous.record_answered(&name);
        crate::ToolOutcome::ok(leaf_answer(&name)).into()
    }
}

/// The answer a leaf returns. It is deliberately identical whether or not the
/// leaf had to wait, so the serial-versus-concurrent differential compares like
/// with like: only the schedule differs between the two halves.
fn leaf_answer(name: &str) -> serde_json::Value {
    serde_json::json!({ "leaf": name })
}

/// The orchestrating relay: one call that dispatches the plan's leaves as one
/// batch through `OrchestrationContext::call_tool_batch`, attaching execution
/// grants to the leaves the plan marks granted.
///
/// This is how a producer that can only name a tool still reaches the granted,
/// orchestrating and deferred routes.
struct RendezvousRelay {
    name: String,
    state: Arc<ScenarioState>,
    /// The replies the relay observed, in the order its consumer received them.
    replies: Arc<std::sync::Mutex<Vec<String>>>,
}

fn relay_definition(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Dispatches a planned batch of rendezvous leaves as one parallel batch.",
        serde_json::json!({
            "type": "object",
            "properties": { "leaves": { "type": "array" } },
            "required": ["leaves"],
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_tool_binding(crate::ToolBinding::new(["tools"], name))
}

#[async_trait::async_trait]
impl crate::facade_support::OrchestratingToolImplementation for RendezvousRelay {
    fn manifest(&self) -> crate::ToolManifest {
        relay_definition(&self.name).manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(relay_definition(&self.name).contract())
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        context: &crate::facade_support::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        let Some(leaves) = args.get("leaves").and_then(|leaves| leaves.as_array()) else {
            return crate::ToolOutcome::err(serde_json::json!("the relay call carries no leaves"));
        };
        let mut invocations = Vec::new();
        for (position, leaf) in leaves.iter().enumerate() {
            let tool = leaf
                .get("tool")
                .and_then(|tool| tool.as_str())
                .unwrap_or("");
            let route = leaf
                .get("route")
                .and_then(|route| route.as_str())
                .unwrap_or("");
            let definition = if route == "Orchestrating" {
                orchestrating_leaf_definition(tool)
            } else {
                leaf_definition(tool)
            };
            let mut invocation = crate::ToolInvocation::new(
                format!("relay-call-{position}"),
                definition.id().clone(),
                serde_json::json!({ "position": position }),
            );
            if route == "Granted" {
                // A grant without a source id is refused by design: granted
                // authority names the source it executes through rather than
                // inferring one. The leaves live on a plugin-registered
                // provider, so that source is the plugin route.
                invocation = invocation.with_execution_grant(
                    crate::ToolExecutionGrant::from_definition(definition)
                        .with_source_id(crate::facade_support::PLUGIN_TOOL_SOURCE_ID),
                );
            }
            invocations.push(invocation);
        }
        let replies = context.call_tool_batch(invocations).await;
        let mut observed = Vec::new();
        let mut answers = Vec::new();
        for reply in replies {
            let value = reply.output.value_for_projection();
            // A reply that is not a leaf answer is recorded verbatim: when a
            // route refuses, the refusal is the finding, and a placeholder
            // would hide it behind a downstream ordering mismatch.
            observed.push(
                value
                    .get("leaf")
                    .and_then(|leaf| leaf.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string()),
            );
            answers.push(value);
        }
        *self.replies.lock_recover() = observed;
        let _ = &self.state;
        crate::ToolOutcome::ok(serde_json::json!({ "answers": answers }))
    }
}

fn orchestrating_leaf_definition(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "An orchestrating rendezvous leaf.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_tool_binding(crate::ToolBinding::new(["tools"], name))
}

/// An orchestrating body that rendezvouses like a leaf, so the orchestration
/// lane inside a group child's invocation driver is covered by the same law.
struct OrchestratingRendezvousLeaf {
    name: String,
    state: Arc<ScenarioState>,
}

#[async_trait::async_trait]
impl crate::facade_support::OrchestratingToolImplementation for OrchestratingRendezvousLeaf {
    fn manifest(&self) -> crate::ToolManifest {
        orchestrating_leaf_definition(&self.name).manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(orchestrating_leaf_definition(&self.name).contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        _context: &crate::facade_support::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        let rendezvous = Arc::clone(&self.state.rendezvous.lock_recover());
        rendezvous.record_started(&self.name);
        let required = self.state.required_for(&self.name, &rendezvous);
        rendezvous.wait_for(&required).await;
        rendezvous.record_answered(&self.name);
        crate::ToolOutcome::ok(leaf_answer(&self.name))
    }
}

/// The scenario's plugin factory: the leaf provider, the orchestrating relay,
/// and the orchestrating leaf the relay dispatches.
#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contracts it registers"
)]
fn rendezvous_plugin(
    names: Vec<String>,
    deferred: Vec<String>,
    orchestrating: Vec<String>,
    relay_name: String,
    state: Arc<ScenarioState>,
    effect_host: Arc<dyn crate::EffectHost>,
    relay_replies: Arc<std::sync::Mutex<Vec<String>>>,
) -> Arc<dyn crate::facade_support::PluginFactory> {
    let leaves: Arc<dyn crate::ToolProvider> = Arc::new(RendezvousLeaves {
        names,
        deferred,
        state: Arc::clone(&state),
        effect_host,
    });
    let mut spec = crate::facade_support::PluginSpec::new().with_tool_provider(leaves);
    spec = spec.with_orchestrating_tool(unsafe {
        crate::facade_support::OrchestratingToolDef::from_first_party(Arc::new(RendezvousRelay {
            name: relay_name,
            state: Arc::clone(&state),
            replies: relay_replies,
        }))
    });
    for name in orchestrating {
        spec = spec.with_orchestrating_tool(unsafe {
            crate::facade_support::OrchestratingToolDef::from_first_party(Arc::new(
                OrchestratingRendezvousLeaf {
                    name,
                    state: Arc::clone(&state),
                },
            ))
        });
    }
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-tool-batch-parallelism",
        spec,
    ))
}

/// One scenario's world: a runtime bound to the tier under test, carrying the
/// producer's factories and the law's own rendezvous leaves.
struct ScenarioWorld {
    state: Arc<ScenarioState>,
    relay_replies: Arc<std::sync::Mutex<Vec<String>>>,
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    model_calls: Arc<AtomicUsize>,
    effect_host: Arc<dyn crate::EffectHost>,
    /// The store set under test: the session catalog the turn commits to and
    /// the ports the runtime takes beside the effect host.
    stores: Arc<dyn crate::StoreSet>,
    session_id: lash_sansio::SessionId,
    /// The tier's process registry, present only for a producer that issues
    /// its batch from inside a process.
    process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the scenario's inputs are the law's parameters: the tier's host and store set, the producer, the plan and the schedule; a struct would only rename the list"
)]
async fn run_scenario(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    producer: &ToolBatchProducer,
    plan: &ToolBatchPlan,
    gated: bool,
    dependencies: BTreeMap<String, Vec<String>>,
) -> ScenarioObservations {
    // The two halves of the differential run the identical program, so they
    // must not land on the identical durable session: a journalling tier would
    // replay the first half's recorded outcomes and the second half would
    // observe no leaves at all. The discriminator is the session, never the
    // program.
    let schedule = if gated { "gated" } else { "serial-safe" };
    let session_id = lash_sansio::SessionId::from(format!(
        "{prefix}-{}-{}-{schedule}",
        producer.label, plan.scenario
    ));
    // The turn runs where the tier runs turns: the runner supplies the
    // controller admitted for the scenario's turn — the host's own in process,
    // a handler-bound one on Restate — and the observations come back over a
    // channel because the attempt owns everything it drives. Each execution
    // of the attempt (every replay, on Restate) runs the scenario afresh.
    let admitted = admit(crate::ExecutionScope::turn(
        &session_id,
        tool_batch_turn_id(&session_id),
    ));
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let producer = producer.clone();
    let plan = plan.clone();
    let stores = Arc::clone(stores);
    runner
        .run_turn(
            admitted,
            Arc::new(move |turn_controller| {
                let session_id = session_id.clone();
                let effect_host = Arc::clone(&effect_host);
                let stores = Arc::clone(&stores);
                let producer = producer.clone();
                let plan = plan.clone();
                let dependencies = dependencies.clone();
                let observed_tx = observed_tx.clone();
                Box::pin(async move {
                    let observed = run_scenario_on_session(
                        session_id,
                        effect_host,
                        stores,
                        Some(turn_controller),
                        &producer,
                        &plan,
                        gated,
                        dependencies,
                    )
                    .await;
                    let _ = observed_tx.send(observed);
                    // The observations carry the scenario's outcome.
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    observed_rx
        .recv()
        .await
        .expect("the tier's turn runner ran the scenario's turn")
}

/// The `run_scenario` body with the session and turn controller chosen by the
/// caller. A host whose `scoped()` already yields the right controller passes
/// `None` and lets `drive_turn` scope it; a handler-bound tier — Restate,
/// whose controller only exists inside the handler — scopes its controller to
/// [`tool_batch_turn_id`] itself and hands it in (FIG-3398).
#[expect(
    clippy::too_many_arguments,
    reason = "the scenario's inputs are the law's parameters: the tier's host and store set, the producer, the plan and the schedule; a struct would only rename the list"
)]
async fn run_scenario_on_session(
    session_id: lash_sansio::SessionId,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    turn_controller: Option<crate::ScopedEffectController<'_>>,
    producer: &ToolBatchProducer,
    plan: &ToolBatchPlan,
    gated: bool,
    dependencies: BTreeMap<String, Vec<String>>,
) -> ScenarioObservations {
    let leaf_names = plan
        .leaves
        .iter()
        .filter(|leaf| leaf.route != ToolBatchRoute::Orchestrating)
        .map(|leaf| leaf.tool.clone())
        .collect::<Vec<_>>();
    let deferred = plan
        .leaves
        .iter()
        .filter(|leaf| leaf.route == ToolBatchRoute::Deferred)
        .map(|leaf| leaf.tool.clone())
        .collect::<Vec<_>>();
    let orchestrating = plan
        .leaves
        .iter()
        .filter(|leaf| leaf.route == ToolBatchRoute::Orchestrating)
        .map(|leaf| leaf.tool.clone())
        .collect::<Vec<_>>();
    let rendezvous = Arc::new(Rendezvous::new(
        plan.leaves.iter().map(|leaf| leaf.tool.clone()).collect(),
        gated,
    ));
    let state = Arc::new(ScenarioState {
        rendezvous: std::sync::Mutex::new(Arc::clone(&rendezvous)),
        dependencies: std::sync::Mutex::new(dependencies),
    });
    let relay_replies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut factories = producer.factories.clone();
    factories.push(rendezvous_plugin(
        leaf_names,
        deferred,
        orchestrating,
        plan.relay_tool.clone(),
        Arc::clone(&state),
        Arc::clone(&effect_host),
        Arc::clone(&relay_replies),
    ));
    let world = ScenarioWorld {
        state,
        relay_replies,
        factories,
        model_calls: Arc::new(AtomicUsize::new(0)),
        effect_host,
        stores,
        session_id,
        process_registry: producer.process_registry.as_ref().map(|make| make()),
    };
    drive_turn(&world, producer, plan, turn_controller).await;
    ScenarioObservations {
        events: rendezvous.events(),
        peak_in_flight: rendezvous.peak_in_flight(),
        never_started: rendezvous.never_started(),
        leaf_window: rendezvous.leaf_window(),
        relay_replies: world.relay_replies.lock_recover().clone(),
        model_calls: world.model_calls.load(Ordering::SeqCst),
    }
}

/// Everything the assertions read back from one scenario run.
#[derive(Debug)]
struct ScenarioObservations {
    events: Vec<RendezvousEvent>,
    peak_in_flight: usize,
    never_started: Vec<String>,
    /// First leaf start to last leaf answer; `None` when no leaf ran. Read by
    /// [`measure_tool_batch`], ignored by the law's assertions.
    leaf_window: Option<(Instant, Instant)>,
    relay_replies: Vec<String>,
    model_calls: usize,
}

impl ScenarioObservations {
    /// The leaves that reported started, in the order they did.
    fn started(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                RendezvousEvent::Started(leaf) => Some(leaf.clone()),
                _ => None,
            })
            .collect()
    }

    /// The leaves that produced an answer, in the order they did. This is the
    /// settlement order the batch observed.
    fn answered(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                RendezvousEvent::Answered(leaf) => Some(leaf.clone()),
                _ => None,
            })
            .collect()
    }

    /// How many leaves started before the first answer was served. On a tier
    /// that overlaps a width-n batch this is n; on a serial tier it is 1.
    fn started_before_first_answer(&self) -> usize {
        let mut started = 0;
        for event in &self.events {
            match event {
                RendezvousEvent::Started(_) => started += 1,
                RendezvousEvent::Answered(_) => break,
            }
        }
        started
    }
}

/// The turn id every scenario binds its turn scope to. It is a pure function
/// of the session so a handler-bound tier can scope its own controller to the
/// same turn before handing it to [`run_scenario_on_session`].
pub fn tool_batch_turn_id(session_id: &lash_sansio::SessionId) -> lash_sansio::TurnId {
    lash_sansio::TurnId::from(format!("{session_id}-turn"))
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drive_turn(
    world: &ScenarioWorld,
    producer: &ToolBatchProducer,
    plan: &ToolBatchPlan,
    turn_controller: Option<crate::ScopedEffectController<'_>>,
) {
    let mut script = (producer.script)(plan);
    script.push(crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: "batch complete".to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..crate::LlmResponse::default()
    });
    let script = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        script,
    )));
    let model_calls = Arc::clone(&world.model_calls);
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_| {
            let script = Arc::clone(&script);
            let model_calls = Arc::clone(&model_calls);
            async move {
                model_calls.fetch_add(1, Ordering::SeqCst);
                let next = script.lock_recover().pop_front();
                Ok(next.unwrap_or_else(|| crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: "batch complete".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..crate::LlmResponse::default()
                }))
            }
        })
        .build();
    // The tier's host is the law backend's effect host rather than a field
    // overwritten later: `RuntimeHostConfig::new` installs the tool-child
    // resolver on the effect host it is given, and a later
    // `control.effect_host` swap would leave the resolver registered on the
    // discarded host.
    let mut law_backend =
        crate::LawBackend::over_stores(Arc::clone(&world.stores), Arc::clone(&world.effect_host));
    if let Some(registry) = world.process_registry.as_ref() {
        law_backend = law_backend.with_process_registry(Arc::clone(registry));
    }
    let mut host = law_backend.host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(world.session_id.clone());
    let state = crate::RuntimeSessionState {
        session_id: world.session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    // A producer whose batch runs inside a process needs four things this
    // runtime otherwise has no reason to own: the engines its own plugins
    // contribute, a process registry, process work bound to exactly that
    // registry, and a worker that drives the registry while the turn is parked
    // on the process. They are installed here and nowhere else, so a producer
    // that issues its batch from the turn still gets the plain one-turn
    // fixture.
    let plugin_host = crate::facade_support::PluginHost::new(world.factories.clone());
    let mut process_wiring = None;
    let mut process_worker = None;
    if let Some(registry) = world.process_registry.as_ref() {
        host = plugin_host
            .install_process_engine_contributions(host, true)
            .expect("install the producer's process-engine contributions");
        // One watch, two consumers: the runtime's process port and the worker
        // must observe the same registry handle, or the turn parks on a change
        // feed nothing publishes to.
        let watched = crate::facade_support::watch_process_registry(Arc::clone(registry));
        let port = Arc::new(crate::NativeProcessWork::for_registry(Arc::clone(
            watched.registry(),
        )));
        process_wiring = Some(crate::ProcessWorkWiring::new(watched.clone(), port));
        process_worker = Some(
            lash_core_worker::DurableProcessWorker::new(
                lash_core_worker::DurableProcessWorkerConfig::new(
                    Arc::new(crate::facade_support::PluginHost::new(
                        world.factories.clone(),
                    )),
                    host.clone(),
                    lash_core_worker::WorkerProcessWork::SelfNative(watched),
                    Arc::new(crate::NoQueuedWork::new()),
                    crate::testing::runtime_lease_owner(),
                )
                .with_session_policy(policy.clone()),
            )
            .expect("build the tool-batch parallelism process worker"),
        );
    }
    let mut builder = crate::LashRuntime::builder(host, crate::testing::runtime_lease_owner());
    if let Some(wiring) = process_wiring {
        builder = builder.with_process_work(wiring);
    }
    let mut runtime = Box::pin(
        builder
            .with_session_id(&world.session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_host(plugin_host)
            .with_store(
                crate::conformance::law_session_store(world.stores.as_ref(), &world.session_id)
                    .await,
            )
            .with_queued_work(Arc::new(crate::NoQueuedWork::new()))
            .build(),
    )
    .await
    .expect("build the tool-batch parallelism conformance runtime");
    let turn_id = tool_batch_turn_id(&world.session_id);
    let turn_scope = match turn_controller {
        Some(scoped) => scoped,
        None => world
            .effect_host
            .scoped(admit(crate::ExecutionScope::turn(
                &world.session_id,
                &turn_id,
            )))
            .expect("scope the tool-batch parallelism turn"),
    };
    // A producer declaring `through_task_proxy` must reach its controller the
    // way a scoped controller that cannot be held 'static is reached — the
    // shape a Restate-style host produces. The borrowed view makes
    // `to_static()` answer `None` everywhere, so every typed turn effect and
    // every `processes.*` command the scenario issues is wrapped in
    // `EffectTaskController` and driven over its request channel rather than
    // taking the 'static shortcut (FIG-3415).
    let turn_scope = if producer.through_task_proxy {
        crate::ScopedEffectController::borrowed(
            turn_scope.controller(),
            turn_scope.admitted_scope().clone(),
        )
        .expect("a borrowed view of the turn's scoped controller")
    } else {
        turn_scope
    };
    let mut input = crate::TurnInput::text("run the planned batch");
    input.trace_turn_id = Some(turn_id);
    // The worker is driven for as long as the turn runs. A process registered
    // mid-turn is admitted on the next sweep; the sweep is what turns
    // `processes.start` into a running process, and without it the turn parks
    // forever on a handle nothing will settle.
    let worker_driver = process_worker.map(|worker| {
        crate::task::spawn(async move {
            loop {
                let _ = worker.drive_pending_processes().await;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
    });

    // The turn is bounded, and this is the law's only clock: the leaves wait
    // on the rendezvous without a wall-clock bound, so this budget bounds a
    // true deadlock and nothing else (FIG-3423). Its expiry is the report a
    // serial tier gets — the leaves that never started — and it also catches
    // a producer whose batch never reached the leaves at all, a process that
    // is registered and never run, say.
    let turn = tokio::time::timeout(
        TURN_BUDGET,
        runtime.stream_turn(
            input,
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), turn_scope),
        ),
    )
    .await
    .unwrap_or_else(|_| {
        let rendezvous = Arc::clone(&world.state.rendezvous.lock_recover());
        panic!(
            "the batch did not overlap: the turn did not settle within \
             {TURN_BUDGET:?} for session `{}`. Leaves that never started: \
             {:?}; leaves that did start: {:?}",
            world.session_id,
            rendezvous.never_started(),
            rendezvous.started(),
        )
    })
    .expect("run the tool-batch parallelism conformance turn");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the batch turn must finish: {:?}; turn issues: {:?}",
        turn.outcome,
        turn.errors,
    );
    if let Some(driver) = worker_driver {
        driver.abort();
    }
    let _ = &world.state;
}

fn leaf_name(scenario: &str, position: usize) -> String {
    format!("rv_{scenario}_{position}")
}

fn plan(scenario: &str, routes: &[ToolBatchRoute], via: ToolBatchEntry) -> ToolBatchPlan {
    ToolBatchPlan {
        scenario: scenario.to_string(),
        leaves: routes
            .iter()
            .enumerate()
            .map(|(position, route)| ToolBatchLeaf {
                tool: leaf_name(scenario, position),
                route: *route,
            })
            .collect(),
        via,
        relay_tool: format!("rv_relay_{scenario}"),
    }
}

fn leaf_routes(width: usize) -> Vec<ToolBatchRoute> {
    vec![ToolBatchRoute::Leaf; width]
}

/// What one measured batch produced (FIG-3398's pre-cutover baseline).
///
/// The measurement shares the law's scenario machinery — the same producers,
/// leaves and turn fixture — but runs the serial-safe schedule: leaves answer
/// as soon as they run, so the window measures dispatch-to-settlement rather
/// than the rendezvous the law needs.
#[derive(Clone, Debug)]
pub struct ToolBatchMeasurement {
    /// The batch width that was issued.
    pub width: usize,
    /// Wall time of the whole scripted turn: model call, batch, and the
    /// closing model call.
    pub turn: Duration,
    /// First leaf start to last leaf answer, when at least one leaf ran. On a
    /// concurrent tier this approaches one leaf's latency; on a serial tier
    /// it is the sum of the leaves.
    pub leaf_window: Option<Duration>,
    pub leaves_started: usize,
    pub leaves_answered: usize,
    /// Observed peak in-flight leaves — the concurrency the tier actually
    /// reached (n on a concurrent tier, 1 on a serial one).
    pub peak_in_flight: usize,
    /// Model round-trips the turn took; sanity evidence that the batch ran
    /// inside one scripted turn.
    pub model_calls: usize,
}

/// Drives one width-`width` batch through `producer` on `effect_host` under
/// `session_id` and measures it (FIG-3398 baseline).
///
/// The plan is the plain one: every leaf takes the catalogue route and the
/// producer issues them directly, so the number measured is the batch itself,
/// not a relay or a deferred settle. `turn_controller` is `None` on hosts
/// whose `scoped()` yields the turn's controller; a handler-bound tier —
/// Restate, whose controller exists only inside a handler — scopes its own
/// controller to `ExecutionScope::turn(session_id, tool_batch_turn_id(..))`
/// and passes it in.
///
/// Panics, as the law's fixture does, if the turn does not finish: a tier
/// that cannot run the batch is a failed measurement, not a slow one.
pub async fn measure_tool_batch(
    session_id: lash_sansio::SessionId,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    turn_controller: Option<crate::ScopedEffectController<'_>>,
    producer: &ToolBatchProducer,
    width: usize,
) -> ToolBatchMeasurement {
    let scenario = format!("measure_w{width}");
    let plan = plan(&scenario, &leaf_routes(width), ToolBatchEntry::Direct);
    let started = Instant::now();
    let observed = run_scenario_on_session(
        session_id,
        effect_host,
        stores,
        turn_controller,
        producer,
        &plan,
        false,
        BTreeMap::new(),
    )
    .await;
    let turn = started.elapsed();
    ToolBatchMeasurement {
        width,
        turn,
        leaf_window: observed
            .leaf_window
            .map(|(first, last)| last.saturating_duration_since(first)),
        leaves_started: observed.started().len(),
        leaves_answered: observed.answered().len(),
        peak_in_flight: observed.peak_in_flight,
        model_calls: observed.model_calls,
    }
}

/// The position of the first event matching `predicate`, or a failure naming
/// the log so a missing transition is reported as itself rather than as a
/// downstream ordering mismatch.
fn position_of(
    events: &[RendezvousEvent],
    what: &str,
    predicate: impl Fn(&RendezvousEvent) -> bool,
) -> usize {
    events
        .iter()
        .position(predicate)
        .unwrap_or_else(|| panic!("no {what} in the rendezvous log: {events:?}"))
}

/// Fails with the message the law owes a serial tier: which leaves never
/// started, and which ones did.
fn assert_every_leaf_started(context: &str, plan: &ToolBatchPlan, observed: &ScenarioObservations) {
    assert!(
        observed.never_started.is_empty(),
        "{context}: the batch did not overlap. Leaves that never started: {:?}; \
         leaves that did start: {:?}; replies the consumer saw: {:?}. A \
         width-{} tool batch whose leaves each wait for the whole width can \
         only complete on a tier that runs them concurrently.",
        observed.never_started,
        observed.started(),
        observed.relay_replies,
        plan.width(),
    );
}

/// Asserts the activation shape: every one of the plan's `n` dispatches is
/// observed before any settlement is served, and every leaf answered exactly
/// once.
fn assert_activation_shape(context: &str, plan: &ToolBatchPlan, observed: &ScenarioObservations) {
    assert_every_leaf_started(context, plan, observed);
    let width = plan.width();
    assert_eq!(
        observed.started_before_first_answer(),
        width,
        "{context}: all {width} dispatches must be observed before the first \
         settlement is served; log: {:?}",
        observed.events,
    );
    assert_eq!(
        observed.peak_in_flight, width,
        "{context}: observed peak in-flight must equal the batch width; log: {:?}",
        observed.events,
    );
    let mut answered = observed.answered();
    answered.sort();
    let mut expected = plan
        .leaves
        .iter()
        .map(|leaf| leaf.tool.clone())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        answered, expected,
        "{context}: every planned leaf answers exactly once",
    );
}

/// The cross-tier tool-batch parallelism law.
///
/// `prefix` namespaces the sessions this law opens on the supplied tier;
/// `effect_host` is the tier under test; `producer` is the product surface that
/// issues the batch. See the module documentation for what is proven and why it
/// is proven by rendezvous rather than by wall time.
pub async fn tool_batch_cross_tier_parallelism(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: ToolBatchProducer,
) {
    let context = format!("{prefix}/{}", producer.label);

    // Width 2 and width 8, every leaf waiting for the whole width. A serial
    // executor cannot leave the first leaf, so it fails here with the named
    // leaves.
    for width in [2_usize, 8] {
        let plan = plan(
            &format!("width{width}"),
            &leaf_routes(width),
            ToolBatchEntry::Direct,
        );
        let observed = run_scenario(
            prefix,
            Arc::clone(&effect_host),
            &stores,
            &runner,
            &producer,
            &plan,
            true,
            BTreeMap::new(),
        )
        .await;
        assert_activation_shape(&format!("{context} width-{width}"), &plan, &observed);
    }

    // The reverse-dependency case: the input-first leaf answers only after the
    // input-last leaf has started. A loop that runs the leaves in input order
    // can never satisfy it, whatever its per-leaf latency.
    let reverse = plan("reverse", &leaf_routes(4), ToolBatchEntry::Direct);
    let observed = {
        let first = reverse.leaves[0].tool.clone();
        let last = reverse.leaves[3].tool.clone();
        let mut dependencies = BTreeMap::new();
        dependencies.insert(first.clone(), vec![last.clone()]);
        for leaf in &reverse.leaves[1..] {
            dependencies.insert(leaf.tool.clone(), Vec::new());
        }
        run_scenario(
            prefix,
            Arc::clone(&effect_host),
            &stores,
            &runner,
            &producer,
            &reverse,
            true,
            dependencies,
        )
        .await
    };
    assert_every_leaf_started(
        &format!("{context} reverse-dependency"),
        &reverse,
        &observed,
    );
    let first = reverse.leaves[0].tool.clone();
    let last = reverse.leaves[3].tool.clone();
    let first_answered = position_of(
        &observed.events,
        &format!("answer from {first}"),
        |event| matches!(event, RendezvousEvent::Answered(leaf) if *leaf == first),
    );
    let last_started = position_of(
        &observed.events,
        &format!("start of {last}"),
        |event| matches!(event, RendezvousEvent::Started(leaf) if *leaf == last),
    );
    assert!(
        first_answered > last_started,
        "{context}: the input-first leaf `{first}` must answer only after the \
         input-last leaf `{last}` has started; log: {:?}",
        observed.events,
    );

    // The granted, orchestrating and deferred routes, dispatched as one batch
    // through an orchestrating relay so every producer that can name a tool
    // covers them.
    if producer.reaches_relay {
        let routes = plan(
            "routes",
            &[
                ToolBatchRoute::Leaf,
                ToolBatchRoute::Granted,
                ToolBatchRoute::Orchestrating,
                ToolBatchRoute::Deferred,
                ToolBatchRoute::Leaf,
                ToolBatchRoute::Granted,
                ToolBatchRoute::Orchestrating,
                ToolBatchRoute::Deferred,
            ],
            ToolBatchEntry::Relay,
        );
        let observed = run_scenario(
            prefix,
            Arc::clone(&effect_host),
            &stores,
            &runner,
            &producer,
            &routes,
            true,
            BTreeMap::new(),
        )
        .await;
        assert_activation_shape(&format!("{context} routes"), &routes, &observed);
        assert_eq!(
            observed.relay_replies,
            routes
                .leaves
                .iter()
                .map(|leaf| leaf.tool.clone())
                .collect::<Vec<_>>(),
            "{context}: the consumer reads replies in call order however the \
             leaves settled",
        );
    }

    // The serial-versus-concurrent differential: the same width-8 program with
    // the rendezvous removed, which a serial executor could also run, returns
    // the identical answers. Only the schedule differs.
    let differential = plan("differential", &leaf_routes(8), ToolBatchEntry::Direct);
    let concurrent = run_scenario(
        prefix,
        Arc::clone(&effect_host),
        &stores,
        &runner,
        &producer,
        &differential,
        true,
        BTreeMap::new(),
    )
    .await;
    let serial_safe = run_scenario(
        prefix,
        Arc::clone(&effect_host),
        &stores,
        &runner,
        &producer,
        &differential,
        false,
        BTreeMap::new(),
    )
    .await;
    assert_activation_shape(
        &format!("{context} differential"),
        &differential,
        &concurrent,
    );
    let mut concurrent_answers = concurrent.answered();
    concurrent_answers.sort();
    let mut serial_answers = serial_safe.answered();
    serial_answers.sort();
    assert_eq!(
        concurrent_answers, serial_answers,
        "{context}: the rendezvous changes the schedule, not the answers",
    );
    assert!(
        serial_safe.model_calls >= 1,
        "{context}: the serial-safe half must have driven the producer",
    );
}
