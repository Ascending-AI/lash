//! The barrier laws: every member of a turn step's tool group really overlaps
//! (FIG-3400, ADR 0116 §7.1).
//!
//! Overlap is proven by *rendezvous*, never by wall time. Every leaf in a
//! width-n group reports that it started and then refuses to produce its
//! answer until the whole width has reported. A tier that runs the leaves one
//! at a time cannot get past the first leaf, so it deadlocks and fails on the
//! turn's deadlock budget, whose message names the leaves that never started;
//! a tier that overlaps them finishes and leaves behind an observation log in
//! which all n starts precede the first answer.
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
//! What follows from the same log is asserted here rather than re-derived by
//! each backend:
//!
//! * observed peak in-flight equals n (the counterpart of the
//!   `max_in_flight_tool_attempts` double in lash-core's runtime tests);
//! * the activation shape — all n dispatches are observed before any
//!   settlement is served — with the consumer's reply order asserted exactly;
//! * a serial-versus-concurrent differential: the same width-n program run with
//!   leaves that never rendezvous returns the identical answers, so the
//!   rendezvous changes the schedule and nothing else; and
//! * the negative control: the same law over a [`SerialGroupHost`], which lets
//!   a group child start only once the one before it settled, fails, naming
//!   the members that never started. A law that cannot fail proves nothing.
//!
//! The laws are parameterised over two axes. The *tier* arrives as an
//! [`crate::EffectHost`] and a [`crate::ConformanceTurnRunner`], so every tier
//! runs the identical assertions. The *producer* arrives as a
//! [`ToolBatchProducer`]: the product surface that spells a width-n group —
//! one `batch` wrapper, two wrappers beside native calls, parallel model tool
//! calls, `Promise.all` and `Promise.allSettled` on the RLM bridge, a Lashlang
//! aggregate on the process bridge. Every one of them is a child of one flat
//! group: `batch` is protocol sugar the driver expands into the step's group,
//! and no tool body dispatches tools (ADR 0116). A producer contributes its
//! plugin factories and the model script that issues the plan; everything
//! else is shared.
//!
//! One producer needs more than a script: an aggregate that is the body of a
//! started process is issued by the process host bridge, and reaching it
//! takes a process registry, process work bound to that registry, the engines
//! the producer's plugins contribute, and a worker driving the registry while
//! the turn is parked on the process. The law stands all four up when a
//! producer declares a registry, and a producer that issues its group from
//! the turn pays none of it.

use crate::admit;
use lash_core::testing::TestTurnDrive as _;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::ToolDefinitionBindingExt as _;
use lash_sansio::sync::MutexExt as _;

use pretty_assertions::assert_eq;

mod budget;

/// How long activation may stall while planned leaves have not started.
///
/// This is the law's only clock, and it is a deadlock budget, not a scheduling
/// assumption: a leaf that is merely slow to be scheduled must never fail the
/// law, so the leaves themselves wait without a wall-clock bound (FIG-3423).
/// A genuinely serial tier cannot leave its first leaf, so the turn outlasts
/// nothing useful — the budget's expiry reports the leaves that never started.
const TURN_BUDGET: Duration = Duration::from_secs(60);

/// The deadlock budget of the negative control. Its turn can never settle, so
/// the budget only bounds the run; the named-members message is what the
/// control asserts.
const SERIAL_BUDGET: Duration = Duration::from_secs(15);

/// The widths every producer is driven at (ADR 0116 §7.1). 64 is the `batch`
/// ceiling.
const WIDTHS: [usize; 3] = [2, 8, 64];

/// One leaf of a planned group.
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
    /// A leaf carrying a `ToolExecutionGrant`: a call-path the catalogue does
    /// not list, which the surface resolved to a grant (RLM's deferred tool
    /// resolution). Its name is [`granted_leaf_name`]'s.
    Granted,
    /// A leaf that parks on a completion key and settles out of band.
    Deferred,
}

/// What the producer is asked to issue: one group, this wide, over these
/// tools, in this order.
#[derive(Clone, Debug)]
pub struct ToolBatchPlan {
    /// A label naming the scenario, used in assertion messages.
    pub scenario: String,
    /// The leaves, in the order the producer must issue them.
    pub leaves: Vec<ToolBatchLeaf>,
    /// Tools the session's catalog carries that the batch never calls. The
    /// scaling guard pads every width's catalog to the largest width's, so
    /// its ratio measures what a child costs, not what a larger catalog
    /// costs every child that records it (FIG-4068).
    pub idle_tools: Vec<String>,
}

impl ToolBatchPlan {
    /// The plan's width.
    pub fn width(&self) -> usize {
        self.leaves.len()
    }

    fn tools(&self) -> Vec<String> {
        self.leaves.iter().map(|leaf| leaf.tool.clone()).collect()
    }
}

/// The model script a producer hands the law: the responses, in order, that
/// make the runtime issue one plan as one group.
pub type ToolBatchScript = Arc<dyn Fn(&ToolBatchPlan) -> Vec<crate::LlmResponse> + Send + Sync>;

/// A fresh process registry for one scenario, supplied by the tier.
///
/// Only a producer whose group runs *inside a process* needs one. It is a
/// factory rather than a handle because each scenario opens its own session,
/// and a durable registry must not carry the previous scenario's rows.
pub type ToolBatchProcessRegistryFactory =
    Arc<dyn Fn() -> Arc<dyn crate::ProcessRegistry> + Send + Sync>;

/// A product surface that issues a width-n tool group.
///
/// The law owns the leaves, the runtime, the tier and every assertion; a
/// producer contributes only the plugin factories its surface needs and the
/// model script that makes the runtime issue `plan` as one group.
#[derive(Clone)]
pub struct ToolBatchProducer {
    /// Names the surface in assertion messages.
    pub label: String,
    /// Plugin factories beyond the law's own leaf provider.
    pub factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    /// The provider script that issues `plan` as one group. The law appends
    /// the terminal text response, so a script that ends after the group is
    /// enough.
    pub script: ToolBatchScript,
    /// Every route the surface can spell. A model spells a tool by name, so
    /// every surface reaches plain and deferred leaves; a granted leaf needs a
    /// surface whose calls carry a grant.
    pub routes: Vec<ToolBatchRoute>,
    /// Present only when the producer's group is issued from inside a
    /// process.
    ///
    /// The law then stands the process substrate up itself — the registry this
    /// factory yields, the process work bound to it, and the engine
    /// contributions the producer's own plugins declare. A producer that issues
    /// its group from the turn leaves this absent, so no tier has to supply a
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
            .field("routes", &self.routes)
            .field("runs_in_a_process", &self.process_registry.is_some())
            .field("through_task_proxy", &self.through_task_proxy)
            .finish()
    }
}

/// The routes a model can spell by naming a tool.
fn named_routes() -> Vec<ToolBatchRoute> {
    vec![ToolBatchRoute::Leaf, ToolBatchRoute::Deferred]
}

fn model_response(parts: Vec<crate::LlmOutputPart>) -> crate::LlmResponse {
    crate::LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..crate::LlmResponse::default()
    }
}

fn native_call(call_id: String, tool: &str, position: usize) -> crate::LlmOutputPart {
    crate::LlmOutputPart::ToolCall {
        call_id,
        tool_name: tool.to_string(),
        input_json: serde_json::json!({ "position": position }).to_string(),
        replay: None,
    }
}

/// One `batch` wrapper over `members`, each `(position, tool)`.
fn batch_call(call_id: &str, members: &[(usize, &ToolBatchLeaf)]) -> crate::LlmOutputPart {
    crate::LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: "batch".to_string(),
        input_json: serde_json::json!({
            "tool_calls": members
                .iter()
                .map(|(position, leaf)| serde_json::json!({
                    "tool": leaf.tool,
                    "parameters": { "position": position },
                }))
                .collect::<Vec<_>>()
        })
        .to_string(),
        replay: None,
    }
}

/// Parallel model tool calls on the standard protocol: one model response
/// carrying n `ToolCall` parts, which the turn driver dispatches as exactly one
/// group. `factories` is the standard protocol the registering crate hands in.
pub fn parallel_model_tool_calls_producer(
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) -> ToolBatchProducer {
    ToolBatchProducer {
        label: "parallel-model-tool-calls".to_string(),
        factories,
        script: Arc::new(|plan| {
            vec![model_response(
                plan.leaves
                    .iter()
                    .enumerate()
                    .map(|(position, leaf)| {
                        native_call(format!("parallel-call-{position}"), &leaf.tool, position)
                    })
                    .collect(),
            )]
        }),
        routes: named_routes(),
        process_registry: None,
        through_task_proxy: false,
    }
}

/// One `batch` wrapper holding the whole width: the standard protocol expands
/// it into the step's one group and folds the members back into one result.
pub fn batch_sugar_producer(
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) -> ToolBatchProducer {
    ToolBatchProducer {
        label: "batch-sugar".to_string(),
        factories,
        script: Arc::new(|plan| {
            let members = plan.leaves.iter().enumerate().collect::<Vec<_>>();
            vec![model_response(vec![batch_call("batch-call", &members)])]
        }),
        routes: named_routes(),
        process_registry: None,
        through_task_proxy: false,
    }
}

/// Two `batch` wrappers beside native calls in one response, sharing one
/// barrier: the first leaf is a native call, the rest split into a wrapper, a
/// native call and a second wrapper, in plan order. Every one of them is a
/// sibling in the step's one group.
pub fn batch_wrappers_beside_native_calls_producer(
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) -> ToolBatchProducer {
    ToolBatchProducer {
        label: "batch-wrappers-beside-native-calls".to_string(),
        factories,
        script: Arc::new(|plan| {
            let leaves = plan.leaves.iter().enumerate().collect::<Vec<_>>();
            let Some((first, rest)) = leaves.split_first() else {
                return vec![model_response(Vec::new())];
            };
            let mut parts = vec![native_call("native-0".to_string(), &first.1.tool, first.0)];
            let half = rest.len() / 2;
            let (wrapper_a, tail) = rest.split_at(half);
            if !wrapper_a.is_empty() {
                parts.push(batch_call("batch-a", wrapper_a));
            }
            if let Some((native, wrapper_b)) = tail.split_first() {
                parts.push(native_call(
                    "native-1".to_string(),
                    &native.1.tool,
                    native.0,
                ));
                if !wrapper_b.is_empty() {
                    parts.push(batch_call("batch-b", wrapper_b));
                }
            }
            vec![model_response(parts)]
        }),
        routes: named_routes(),
        process_registry: None,
        through_task_proxy: false,
    }
}

/// The routes an RLM producer reaches: every named route, and the granted
/// one when its factories resolve [`granted_leaf_name`]s through deferred
/// tool resolution.
fn rlm_routes(grants: bool) -> Vec<ToolBatchRoute> {
    let mut routes = named_routes();
    if grants {
        routes.push(ToolBatchRoute::Granted);
    }
    routes
}

/// The RLM bridge's `Promise.all`: one cell whose leaves are awaited together,
/// which the bridge turns into exactly one group.
///
/// The cell source lives here rather than at the registration site so every
/// tier issues the byte-identical program; the caller supplies only the RLM
/// protocol plugin factory, which is the part this crate cannot construct.
/// `grants` says whether that factory resolves granted leaves.
pub fn rlm_promise_all_producer(
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    grants: bool,
) -> ToolBatchProducer {
    ToolBatchProducer {
        label: "rlm-promise-all".to_string(),
        factories,
        script: Arc::new(|plan| rlm_cell_script(plan, "Promise.all")),
        routes: rlm_routes(grants),
        process_registry: None,
        through_task_proxy: false,
    }
}

/// The RLM bridge's `Promise.allSettled`: the same group, answered as one
/// settlement record per leaf.
pub fn rlm_promise_all_settled_producer(
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    grants: bool,
) -> ToolBatchProducer {
    ToolBatchProducer {
        label: "rlm-promise-all-settled".to_string(),
        factories,
        script: Arc::new(|plan| rlm_cell_script(plan, "Promise.allSettled")),
        routes: rlm_routes(grants),
        process_registry: None,
        through_task_proxy: false,
    }
}

/// The dialect the RLM cell channel wraps a cell's source in. Named once here
/// so the opening and closing tags cannot drift apart.
const RLM_CELL_DIALECT: &str = "typescript";

fn aggregate_calls(plan: &ToolBatchPlan, indent: &str) -> String {
    plan.leaves
        .iter()
        .enumerate()
        .map(|(position, leaf)| format!("{indent}tools.{}({{ position: {position} }})", leaf.tool))
        .collect::<Vec<_>>()
        .join(",\n")
}

/// The cell `aggregate` issues over the plan.
///
/// `finish` is the only statement that closes an RLM turn: a cell whose last
/// line is a bare expression leaves the driver asking the provider again, and
/// the group would be re-issued rather than settled.
fn rlm_cell_script(plan: &ToolBatchPlan, aggregate: &str) -> Vec<crate::LlmResponse> {
    let body = format!(
        "finish(await {aggregate}([\n{}\n]));",
        aggregate_calls(plan, "  ")
    );
    vec![model_response(vec![crate::LlmOutputPart::Text {
        text: format!("<{RLM_CELL_DIALECT}>\n{body}\n</{RLM_CELL_DIALECT}>"),
        response_meta: None,
    }])]
}

/// The process bridge's Lashlang aggregate: the same aggregate, but running
/// inside a started process rather than inside the turn.
///
/// The cell defines the aggregate as a process definition and starts it, so the
/// group is issued by the process host bridge
/// (`lash-lashlang-runtime/src/process.rs`) and not by the cell's own host
/// bridge. That is a second, independently written group caller, which is why
/// it is a producer of this law rather than a variation of
/// `rlm_promise_all_producer`.
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
            vec![model_response(vec![crate::LlmOutputPart::Text {
                text: lashlang_process_aggregate_cell(plan),
                response_meta: None,
            }])]
        }),
        routes: named_routes(),
        process_registry: Some(registry),
        through_task_proxy: true,
    }
}

/// The cell text [`lashlang_process_aggregate_producer`] issues.
///
/// The aggregate is the *process definition*: nothing is awaited in the cell
/// itself, so the whole width is issued by the process bridge. `finish` closes
/// the turn on the process's terminal value, for the reason given on
/// [`rlm_cell_script`].
fn lashlang_process_aggregate_cell(plan: &ToolBatchPlan) -> String {
    let body = format!(
        "const group = async () => {{\n  return await Promise.all([\n{}\n  ]);\n}};\n\
         const handle = await processes.start({{ definition: group }});\n\
         finish(await handle);",
        aggregate_calls(plan, "    ")
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
    /// [`measure_tool_batch`] (FIG-3398): the group window is first-start to
    /// last-answer, and stamping them inside this same critical section keeps
    /// the measurement as untearable as the log.
    started_at: Vec<Instant>,
    answered_at: Vec<Instant>,
    /// Set once the scenario's deadlock budget expired: the budget, and the
    /// planned leaves that had not started when it did. Taken in the same
    /// critical section as the starts, before the waiters are let through, so
    /// a leaf that starts only because the budget released its siblings is
    /// still reported as never having started in time.
    expired: Option<ExpiredBudget>,
}

/// What the scenario looked like when its deadlock budget expired.
#[derive(Clone, Debug)]
struct ExpiredBudget {
    budget: Duration,
    never_started: Vec<String>,
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
    /// Set once a scenario that gave up releases its waiters, so a group that
    /// never overlapped still drains instead of parking its children forever.
    released: AtomicBool,
}

impl Rendezvous {
    fn new(expected: Vec<String>, gated: bool) -> Self {
        Self {
            expected,
            shared: std::sync::Mutex::new(RendezvousShared::default()),
            notify: tokio::sync::watch::channel(0).0,
            log: RendezvousLog::default(),
            gated,
            released: AtomicBool::new(false),
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

    /// Expires only if no start advanced the observed count and members are
    /// still missing. The check and missing-member snapshot share the start
    /// lock, so a start racing the timer cannot produce a false expiry.
    fn expire_if_activation_stalled(&self, started: usize, budget: Duration) -> bool {
        {
            let mut shared = self.shared.lock_recover();
            if shared.started.len() != started {
                return false;
            }
            let never_started: Vec<_> = self
                .expected
                .iter()
                .filter(|leaf| !shared.started.contains(leaf))
                .cloned()
                .collect();
            if never_started.is_empty() {
                return false;
            }
            if shared.expired.is_none() {
                shared.expired = Some(ExpiredBudget {
                    budget,
                    never_started,
                });
            }
        }
        self.release();
        true
    }

    /// How many leaf starts the log holds.
    fn started_count(&self) -> usize {
        self.shared.lock_recover().started.len()
    }

    fn expired(&self) -> Option<ExpiredBudget> {
        self.shared.lock_recover().expired.clone()
    }

    /// Lets every waiter through, now and from now on.
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.notify.send_modify(|count| *count += 1);
    }

    /// First leaf start to last leaf answer — the group's own window,
    /// excluding the model round-trips that frame it. `None` when no leaf ran.
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
            if self.released.load(Ordering::SeqCst) || self.missing(required).is_empty() {
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

    /// Every planned leaf that never reported started, in plan order — as of
    /// the budget's expiry, when it expired. This is the message a serial
    /// tier fails with.
    fn never_started(&self) -> Vec<String> {
        match self.expired() {
            Some(expired) => expired.never_started,
            None => self.missing(&self.expected),
        }
    }
}

/// The per-scenario state the leaf provider shares with the law.
///
/// One per scenario, never one per execution of its turn: a Restate handler
/// runs the turn again from the top on every replay, and each execution builds
/// its own runtime, but a group child borrows whichever execution's context is
/// live when it runs — or the one its group pinned. Leaves routed through
/// different executions must still meet at one rendezvous, and the model
/// calls an earlier execution made, which a replay reads from the journal
/// rather than making again, must still be counted (FIG-4070).
#[derive(Debug)]
struct ScenarioState {
    rendezvous: Arc<Rendezvous>,
    /// Which leaves each leaf must wait for. An absent entry means "all of
    /// them"; a present one is the reverse-dependency case.
    dependencies: BTreeMap<String, Vec<String>>,
    model_calls: AtomicUsize,
}

impl ScenarioState {
    fn new(
        plan: &ToolBatchPlan,
        schedule: Schedule,
        dependencies: BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            rendezvous: Arc::new(Rendezvous::new(plan.tools(), schedule.gated)),
            dependencies,
            model_calls: AtomicUsize::new(0),
        }
    }

    fn required_for(&self, leaf: &str) -> Vec<String> {
        self.dependencies
            .get(leaf)
            .cloned()
            .unwrap_or_else(|| self.rendezvous.expected.clone())
    }
}

/// The name a granted leaf carries: the catalogue does not list it, so a
/// surface that resolves unlisted call-paths to grants recognises it by this
/// shape (see [`tool_batch_granted_leaf`]).
fn granted_leaf_name(scenario: &str, position: usize) -> String {
    format!("{}_granted", leaf_name(scenario, position))
}

/// The definition a surface grants for the unlisted call-path `path`, when
/// `path` names one of this law's granted leaves, and the source route the
/// grant must execute through. An RLM registration's deferred tool resolver
/// answers with it, so its cells reach the granted route without the
/// catalogue listing the leaf.
pub fn tool_batch_granted_leaf(path: &str) -> Option<(crate::ToolDefinition, &'static str)> {
    let name = path.strip_prefix("tools.")?;
    (name.starts_with("rv_") && name.ends_with("_granted")).then(|| {
        (
            leaf_definition(name),
            crate::facade_support::PLUGIN_TOOL_SOURCE_ID,
        )
    })
}

fn leaf_definition(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A rendezvous leaf: answers only once its whole group has started.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_tool_binding(crate::ToolBinding::new(["tools"], name))
}

/// The leaf provider. Every plain, granted and deferred leaf of every scenario
/// is one of these tools. A granted leaf is not listed: it resolves only by
/// id, as the grant a surface resolved names it.
struct RendezvousLeaves {
    listed: Vec<String>,
    granted: Vec<String>,
    deferred: Vec<String>,
    state: Arc<ScenarioState>,
    effect_host: Arc<dyn crate::EffectHost>,
}

impl RendezvousLeaves {
    fn owns(&self, name: &str) -> bool {
        self.listed
            .iter()
            .chain(&self.granted)
            .any(|leaf| leaf == name)
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for RendezvousLeaves {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.listed
            .iter()
            .map(|name| leaf_definition(name).manifest())
            .collect()
    }

    fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<crate::ToolManifest> {
        self.listed
            .iter()
            .chain(&self.granted)
            .map(|name| leaf_definition(name))
            .find(|definition| definition.id() == id)
            .map(|definition| definition.manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.owns(name)
            .then(|| Arc::new(leaf_definition(name).contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        self.deferred
            .iter()
            .any(|name| leaf_definition(name).id() == tool_id)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let name = call.name().to_string();
        let rendezvous = Arc::clone(&self.state.rendezvous);
        rendezvous.record_started(&name);
        let required = self.state.required_for(&name);

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

/// The scenario's plugin factory: the leaf provider.
fn rendezvous_plugin(
    plan: &ToolBatchPlan,
    state: Arc<ScenarioState>,
    effect_host: Arc<dyn crate::EffectHost>,
) -> Arc<dyn crate::facade_support::PluginFactory> {
    let named = |route: ToolBatchRoute| {
        plan.leaves
            .iter()
            .filter(|leaf| leaf.route == route)
            .map(|leaf| leaf.tool.clone())
            .collect::<Vec<_>>()
    };
    let granted = named(ToolBatchRoute::Granted);
    let deferred = named(ToolBatchRoute::Deferred);
    let listed = plan
        .leaves
        .iter()
        .filter(|leaf| leaf.route != ToolBatchRoute::Granted)
        .map(|leaf| leaf.tool.clone())
        .chain(plan.idle_tools.iter().cloned())
        .collect();
    let leaves: Arc<dyn crate::ToolProvider> = Arc::new(RendezvousLeaves {
        listed,
        granted,
        deferred,
        state,
        effect_host,
    });
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-tool-batch-parallelism",
        crate::facade_support::PluginSpec::new().with_tool_provider(leaves),
    ))
}

/// One scenario's world: a runtime bound to the tier under test, carrying the
/// producer's factories and the law's own rendezvous leaves.
struct ScenarioWorld {
    state: Arc<ScenarioState>,
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    effect_host: Arc<dyn crate::EffectHost>,
    /// The store set under test: the session catalog the turn commits to and
    /// the ports the runtime takes beside the effect host.
    stores: Arc<dyn crate::StoreSet>,
    session_id: lash_sansio::SessionId,
    /// The tier's process registry, present only for a producer that issues
    /// its group from inside a process.
    process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
}

/// How one scenario's turn ended.
#[derive(Debug)]
enum ScenarioEnd {
    /// The turn finished; `replies` are the leaf answers its consumer read,
    /// in the order it read them.
    Finished { replies: Vec<String> },
    /// The turn did not settle within its budget.
    DeadlockBudgetExpired { budget: Duration },
}

/// The schedule one scenario runs under.
#[derive(Clone, Copy, Debug)]
struct Schedule {
    /// Whether the leaves rendezvous.
    gated: bool,
    /// The turn's deadlock budget.
    budget: Duration,
}

impl Schedule {
    const GATED: Self = Self {
        gated: true,
        budget: TURN_BUDGET,
    };
    const SERIAL_SAFE: Self = Self {
        gated: false,
        budget: TURN_BUDGET,
    };
    const NEGATIVE_CONTROL: Self = Self {
        gated: true,
        budget: SERIAL_BUDGET,
    };
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the scenario's inputs are the law's parameters: the tier's host, store set and runner, the producer, the plan and the schedule; a struct would only rename the list"
)]
async fn run_scenario(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    producer: &ToolBatchProducer,
    plan: &ToolBatchPlan,
    schedule: Schedule,
    dependencies: BTreeMap<String, Vec<String>>,
) -> ScenarioObservations {
    // The two halves of the differential run the identical program, so they
    // must not land on the identical durable session: a journalling tier would
    // replay the first half's recorded outcomes and the second half would
    // observe no leaves at all. The discriminator is the session, never the
    // program.
    let label = if schedule.gated {
        "gated"
    } else {
        "serial-safe"
    };
    let session_id = lash_sansio::SessionId::from(format!(
        "{prefix}-{}-{}-{label}",
        producer.label, plan.scenario
    ));
    // The turn runs where the tier runs turns: the runner supplies the
    // controller admitted for the scenario's turn — the host's own in process,
    // a handler-bound one on Restate — and the observations come back over a
    // channel because the attempt owns everything it drives. Each execution
    // of the attempt (every replay, on Restate) builds its runtime afresh,
    // but every one of them shares the scenario's one state: the leaves of
    // one group meet at one rendezvous whichever execution routed them.
    let admitted = admit(crate::ExecutionScope::turn(
        &session_id,
        tool_batch_turn_id(&session_id),
    ));
    let state = Arc::new(ScenarioState::new(plan, schedule, dependencies));
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let producer = producer.clone();
    let plan = plan.clone();
    let stores = Arc::clone(stores);
    let tier = Arc::clone(runner);
    let execution_state = Arc::clone(&state);
    let turn = runner.run_turn(
        admitted,
        Arc::new(move |turn_controller| {
            let tier = Arc::clone(&tier);
            let session_id = session_id.clone();
            let effect_host = Arc::clone(&effect_host);
            let stores = Arc::clone(&stores);
            let producer = producer.clone();
            let plan = plan.clone();
            let state = Arc::clone(&execution_state);
            let observed_tx = observed_tx.clone();
            Box::pin(async move {
                let observed = run_scenario_on_session(
                    session_id,
                    effect_host,
                    stores,
                    Some(&tier),
                    Some(turn_controller),
                    &producer,
                    &plan,
                    schedule,
                    state,
                )
                .await;
                let _ = observed_tx.send(observed);
                // The observations carry the scenario's outcome.
                crate::ConformanceTurnEnd::Settled
            })
        }),
    );
    // A suspended handler runs no clock, so its activation budget also runs
    // here. Both layers use the same progress rule. After expiry, the released
    // members drain and the handler returns the missing-member observation.
    tokio::pin!(turn);
    if budget::run_with_activation_budget(&mut turn, &state.rendezvous, schedule.budget)
        .await
        .is_none()
    {
        (&mut turn).await;
    }
    let observed = observed_rx
        .recv()
        .await
        .expect("the tier's turn runner ran the scenario's turn");
    runner.scenario_finished().await;
    observed
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
    runner: Option<&Arc<dyn crate::ConformanceTurnRunner>>,
    turn_controller: Option<crate::ScopedEffectController<'_>>,
    producer: &ToolBatchProducer,
    plan: &ToolBatchPlan,
    schedule: Schedule,
    state: Arc<ScenarioState>,
) -> ScenarioObservations {
    let rendezvous = Arc::clone(&state.rendezvous);
    let mut factories = producer.factories.clone();
    factories.push(rendezvous_plugin(
        plan,
        Arc::clone(&state),
        Arc::clone(&effect_host),
    ));
    let world = ScenarioWorld {
        state: Arc::clone(&state),
        factories,
        effect_host,
        stores,
        session_id,
        process_registry: producer.process_registry.as_ref().map(|make| make()),
    };
    let end = drive_turn(
        &world,
        runner,
        producer,
        plan,
        turn_controller,
        schedule.budget,
    )
    .await;
    // A budget that expired, here or from outside the turn, is how the
    // scenario ended, however the released turn finished afterwards.
    let end = match rendezvous.expired() {
        Some(expired) => ScenarioEnd::DeadlockBudgetExpired {
            budget: expired.budget,
        },
        None => end,
    };
    ScenarioObservations {
        events: rendezvous.events(),
        peak_in_flight: rendezvous.peak_in_flight(),
        never_started: rendezvous.never_started(),
        leaf_window: rendezvous.leaf_window(),
        end,
        model_calls: state.model_calls.load(Ordering::SeqCst),
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
    end: ScenarioEnd,
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
    /// settlement order the group observed.
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
    /// that overlaps a width-n group this is n; on a serial tier it is 1.
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

    /// The replies the consumer read, in the order it read them.
    fn replies(&self) -> &[String] {
        match &self.end {
            ScenarioEnd::Finished { replies } => replies,
            ScenarioEnd::DeadlockBudgetExpired { .. } => &[],
        }
    }
}

/// The turn id every scenario binds its turn scope to. It is a pure function
/// of the session so a handler-bound tier can scope its own controller to the
/// same turn before handing it to [`run_scenario_on_session`].
pub fn tool_batch_turn_id(session_id: &lash_sansio::SessionId) -> lash_sansio::TurnId {
    lash_sansio::TurnId::from(format!("{session_id}-turn"))
}

/// The first leaf name in `text`: a leaf's answer however its consumer
/// rendered it — a JSON object, a rendered value, a settlement record.
fn leaf_in(text: &str) -> Option<String> {
    let start = text.find("rv_")?;
    Some(
        text[start..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect(),
    )
}

/// The replies a finished turn's consumer read, in the order it read them: a
/// cell's final value is the aggregate's answer array, and a model's reply is
/// the tool results the transcript answers its calls with — a `batch`
/// wrapper's rows in member order.
fn consumer_replies(turn: &crate::AssembledTurn) -> Vec<String> {
    if let crate::TurnOutcome::Finished(crate::TurnFinish::FinalValue { value }) = &turn.outcome
        && let Some(values) = value.as_array()
    {
        return values
            .iter()
            .filter_map(|value| leaf_in(&value.to_string()))
            .collect();
    }
    let view = turn.state.read_view();
    let mut replies = Vec::new();
    for part in view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
    {
        if part.kind() != crate::PartKind::ToolResult {
            continue;
        }
        let content = part.content();
        match serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .and_then(|value| {
                value
                    .get("results")
                    .and_then(|rows| rows.as_array())
                    .cloned()
            }) {
            Some(rows) => replies.extend(rows.iter().filter_map(|row| leaf_in(&row.to_string()))),
            None => replies.extend(leaf_in(&content)),
        }
    }
    replies
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drive_turn(
    world: &ScenarioWorld,
    runner: Option<&Arc<dyn crate::ConformanceTurnRunner>>,
    producer: &ToolBatchProducer,
    plan: &ToolBatchPlan,
    turn_controller: Option<crate::ScopedEffectController<'_>>,
    budget: Duration,
) -> ScenarioEnd {
    let closing = || {
        model_response(vec![crate::LlmOutputPart::Text {
            text: "group complete".to_string(),
            response_meta: None,
        }])
    };
    // The model answers the turn's n-th step with the script's n-th response,
    // reading the step off the request rather than counting calls: every
    // execution builds this model afresh, and a replayed execution asks it
    // only for steps its journal does not hold, so a per-execution count
    // would answer a later step with the first response again.
    let script = Arc::new((producer.script)(plan));
    let state = Arc::clone(&world.state);
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let step = request
                .messages
                .iter()
                .filter(|message| matches!(message.role, lash_core::llm::types::LlmRole::Assistant))
                .count();
            let next = script.get(step).cloned();
            let state = Arc::clone(&state);
            async move {
                state.model_calls.fetch_add(1, Ordering::SeqCst);
                Ok(next.unwrap_or_else(closing))
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
    // A producer whose group runs inside a process needs four things this
    // runtime otherwise has no reason to own: the engines its own plugins
    // contribute, a process registry, and process work bound to exactly that
    // registry, whose segments the tier's engine serves with a worker built
    // here. They are installed here and nowhere else, so a producer that
    // issues its group from the turn still gets the plain one-turn fixture.
    let plugin_host = crate::facade_support::PluginHost::new(world.factories.clone());
    let mut process_wiring = None;
    if let Some(registry) = world.process_registry.as_ref() {
        host = plugin_host
            .install_process_engine_contributions(host, true)
            .expect("install the producer's process-engine contributions");
        // One watch, two consumers: the runtime's process port and the worker
        // must observe the same registry handle, or the turn parks on a change
        // feed nothing publishes to.
        let watched = crate::facade_support::watch_process_registry(Arc::clone(registry));
        let worker = lash_core_worker::DurableProcessWorker::new(
            lash_core_worker::DurableProcessWorkerConfig::new(
                Arc::new(crate::facade_support::PluginHost::new(
                    world.factories.clone(),
                )),
                host.clone(),
                crate::ProcessWorkWiring::new(
                    watched.clone(),
                    Arc::new(crate::NoProcessWork::new(&watched)),
                ),
                Arc::new(crate::NoSessionWork::new()),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_policy(policy.clone()),
        )
        .expect("build the tool-group parallelism process worker");
        let runner = runner.expect("a producer that runs in a process is run by the law's tier");
        process_wiring = Some(runner.process_work(watched, worker));
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
            .with_store(crate::conformance::helpers::session_view(
                &crate::conformance::law_session_store(world.stores.as_ref(), &world.session_id)
                    .await,
                world.session_id.clone(),
            ))
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the tool-group parallelism conformance runtime");
    let turn_id = tool_batch_turn_id(&world.session_id);
    let turn_scope = match turn_controller {
        Some(scoped) => scoped,
        None => world
            .effect_host
            .scoped(admit(crate::ExecutionScope::turn(
                &world.session_id,
                &turn_id,
            )))
            .expect("scope the tool-group parallelism turn"),
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
    let mut input = crate::TurnInput::text("run the planned group");
    input.trace_turn_id = Some(turn_id);
    // Bound stalled activation, preserving a progressing or fully activated
    // group even when its turn outlasts one budget. The suite runner owns the
    // absolute process timeout.
    let Some(turn) = budget::run_with_activation_budget(
        runtime.drive_turn(
            input,
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), turn_scope),
        ),
        &world.state.rendezvous,
        budget,
    )
    .await
    else {
        return ScenarioEnd::DeadlockBudgetExpired { budget };
    };
    let turn = turn.expect("run the tool-group parallelism conformance turn");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the group turn must finish: {:?}; turn issues: {:?}",
        turn.outcome,
        turn.errors,
    );
    ScenarioEnd::Finished {
        replies: consumer_replies(&turn),
    }
}

fn leaf_name(scenario: &str, position: usize) -> String {
    format!("rv_{scenario}_{position}")
}

fn plan(scenario: &str, routes: &[ToolBatchRoute]) -> ToolBatchPlan {
    ToolBatchPlan {
        scenario: scenario.to_string(),
        leaves: routes
            .iter()
            .enumerate()
            .map(|(position, route)| ToolBatchLeaf {
                tool: match route {
                    ToolBatchRoute::Granted => granted_leaf_name(scenario, position),
                    ToolBatchRoute::Leaf | ToolBatchRoute::Deferred => {
                        leaf_name(scenario, position)
                    }
                },
                route: *route,
            })
            .collect(),
        idle_tools: Vec::new(),
    }
}

fn leaf_routes(width: usize) -> Vec<ToolBatchRoute> {
    vec![ToolBatchRoute::Leaf; width]
}

/// Width 8 over every route `producer` reaches, cycled in plan order.
fn mixed_routes(producer: &ToolBatchProducer) -> Vec<ToolBatchRoute> {
    producer.routes.iter().copied().cycle().take(8).collect()
}

/// What one measured group produced (FIG-3398's pre-cutover baseline).
///
/// The measurement shares the law's scenario machinery — the same producers,
/// leaves and turn fixture — but runs the serial-safe schedule: leaves answer
/// as soon as they run, so the window measures dispatch-to-settlement rather
/// than the rendezvous the law needs.
#[derive(Clone, Debug)]
pub struct ToolBatchMeasurement {
    /// The group width that was issued.
    pub width: usize,
    /// Wall time of the whole scripted turn: model call, group, and the
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
    /// Model round-trips the turn took; sanity evidence that the group ran
    /// inside one scripted turn.
    pub model_calls: usize,
}

/// Drives one width-`width` group through `producer` on `effect_host` under
/// `session_id` and measures it (FIG-3398 baseline).
///
/// The plan is the plain one: every leaf takes the catalogue route, so the
/// number measured is the group itself, not a deferred settle.
/// `turn_controller` is `None` on hosts whose `scoped()` yields the turn's
/// controller; a handler-bound tier — Restate, whose controller exists only
/// inside a handler — scopes its own controller to
/// `ExecutionScope::turn(session_id, tool_batch_turn_id(..))` and passes it in.
///
/// Panics, as the law's fixture does, if the turn does not finish: a tier
/// that cannot run the group is a failed measurement, not a slow one.
pub async fn measure_tool_batch(
    session_id: lash_sansio::SessionId,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    turn_controller: Option<crate::ScopedEffectController<'_>>,
    producer: &ToolBatchProducer,
    width: usize,
) -> ToolBatchMeasurement {
    let scenario = format!("measure_w{width}");
    let plan = plan(&scenario, &leaf_routes(width));
    let started = Instant::now();
    let observed = run_scenario_on_session(
        session_id,
        effect_host,
        stores,
        None,
        turn_controller,
        producer,
        &plan,
        Schedule::SERIAL_SAFE,
        Arc::new(ScenarioState::new(
            &plan,
            Schedule::SERIAL_SAFE,
            BTreeMap::new(),
        )),
    )
    .await;
    let turn = started.elapsed();
    if let ScenarioEnd::DeadlockBudgetExpired { budget } = observed.end {
        panic!("the measured group did not settle within {budget:?}");
    }
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

/// Drives one gated width-`width` batch through `producer` on a tier's turn
/// runner, exactly as the law's width rows do, and measures it (FIG-4068).
///
/// Every leaf waits for the whole width before it answers, so all `width`
/// children are live at once: the shape whose time and memory must grow
/// linearly in the width. The session's catalog holds `catalog` tools
/// whatever the width (at least the width's own), so two widths differ only
/// in how many children run. The activation shape is asserted, so a tier
/// that serialises the batch fails here rather than reporting a fast number.
pub async fn measure_gated_tool_batch(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: &ToolBatchProducer,
    width: usize,
    catalog: usize,
) -> ToolBatchMeasurement {
    let scenario = format!("scaling_w{width}");
    let mut plan = plan(&scenario, &leaf_routes(width));
    plan.idle_tools = (width..catalog)
        .map(|position| leaf_name(&scenario, position))
        .collect();
    let started = Instant::now();
    let observed = run_scenario(
        prefix,
        effect_host,
        &stores,
        &runner,
        producer,
        &plan,
        Schedule::GATED,
        BTreeMap::new(),
    )
    .await;
    let turn = started.elapsed();
    assert_activation_shape(
        &format!("{prefix}/{} scaling width-{width}", producer.label),
        &plan,
        &observed,
    );
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

/// Fails with the message the law owes a serial tier: which members never
/// started, and which ones did.
fn assert_every_leaf_started(context: &str, plan: &ToolBatchPlan, observed: &ScenarioObservations) {
    let budget = match observed.end {
        ScenarioEnd::DeadlockBudgetExpired { budget } => format!(
            " The turn did not settle within {budget:?}; {} of {} members answered.",
            observed.answered().len(),
            plan.width()
        ),
        ScenarioEnd::Finished { .. } => String::new(),
    };
    assert!(
        observed.never_started.is_empty() && matches!(observed.end, ScenarioEnd::Finished { .. }),
        "{context}: the group did not overlap.{budget} Members that never started: {:?}; \
         members that did start: {:?}. A width-{} tool group whose members each wait for \
         the whole width can only complete on a tier that runs them concurrently.",
        observed.never_started,
        observed.started(),
        plan.width(),
    );
}

/// Asserts the activation shape: every one of the plan's `n` dispatches is
/// observed before any settlement is served, every leaf answered exactly
/// once, and the consumer read the replies in call order however the leaves
/// settled.
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
        "{context}: observed peak in-flight must equal the group width; log: {:?}",
        observed.events,
    );
    let mut answered = observed.answered();
    answered.sort();
    let mut expected = plan.tools();
    expected.sort();
    assert_eq!(
        answered, expected,
        "{context}: every planned leaf answers exactly once",
    );
    assert_eq!(
        observed.replies(),
        plan.tools().as_slice(),
        "{context}: the consumer reads replies in call order however the leaves settled",
    );
}

/// Every member of a width-n tool group starts before any finishes, for every
/// producer, at widths 2, 8 and 64 and over every route the producer reaches
/// (ADR 0116 §7.1). A serial-versus-concurrent differential shows the
/// rendezvous changes the schedule and not the answers.
///
/// `prefix` namespaces the sessions this law opens on the supplied tier;
/// `effect_host` is the tier under test; `producer` is the product surface
/// that issues the group. See the module documentation for what is proven and
/// why it is proven by rendezvous rather than by wall time.
pub async fn tool_group_members_start_before_any_finishes(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: ToolBatchProducer,
) {
    let context = format!("{prefix}/{}", producer.label);

    // Every width, every leaf waiting for the whole width. A serial executor
    // cannot leave the first leaf, so it fails here with the named members.
    for width in WIDTHS {
        let plan = plan(&format!("width{width}"), &leaf_routes(width));
        let observed = run_scenario(
            prefix,
            Arc::clone(&effect_host),
            &stores,
            &runner,
            &producer,
            &plan,
            Schedule::GATED,
            BTreeMap::new(),
        )
        .await;
        assert_activation_shape(&format!("{context} width-{width}"), &plan, &observed);
    }

    // Every route the producer reaches, as siblings of one group.
    let routes = plan("routes", &mixed_routes(&producer));
    let observed = run_scenario(
        prefix,
        Arc::clone(&effect_host),
        &stores,
        &runner,
        &producer,
        &routes,
        Schedule::GATED,
        BTreeMap::new(),
    )
    .await;
    assert_activation_shape(&format!("{context} routes"), &routes, &observed);

    // The serial-versus-concurrent differential: the same width-8 program with
    // the rendezvous removed, which a serial executor could also run, returns
    // the identical answers. Only the schedule differs.
    let differential = plan("differential", &leaf_routes(8));
    let concurrent = run_scenario(
        prefix,
        Arc::clone(&effect_host),
        &stores,
        &runner,
        &producer,
        &differential,
        Schedule::GATED,
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
        Schedule::SERIAL_SAFE,
        BTreeMap::new(),
    )
    .await;
    assert_activation_shape(
        &format!("{context} differential"),
        &differential,
        &concurrent,
    );
    assert_eq!(
        concurrent.replies(),
        serial_safe.replies(),
        "{context}: the rendezvous changes the schedule, not the answers",
    );
    assert!(
        serial_safe.model_calls >= 1,
        "{context}: the serial-safe half must have driven the producer",
    );
}

/// The reverse-dependency case: the input-first member answers only after the
/// input-last member started. A loop that runs the members in input order can
/// never satisfy it, whatever its per-member latency.
pub async fn tool_group_reverse_dependency(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: ToolBatchProducer,
) {
    let context = format!("{prefix}/{}", producer.label);
    let reverse = plan("reverse", &leaf_routes(4));
    let first = reverse.leaves[0].tool.clone();
    let last = reverse.leaves[3].tool.clone();
    let mut dependencies = BTreeMap::new();
    dependencies.insert(first.clone(), vec![last.clone()]);
    for leaf in &reverse.leaves[1..] {
        dependencies.insert(leaf.tool.clone(), Vec::new());
    }
    let observed = run_scenario(
        prefix,
        effect_host,
        &stores,
        &runner,
        &producer,
        &reverse,
        Schedule::GATED,
        dependencies,
    )
    .await;
    assert_every_leaf_started(
        &format!("{context} reverse-dependency"),
        &reverse,
        &observed,
    );
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
        "{context}: the input-first member `{first}` must answer only after the \
         input-last member `{last}` has started; log: {:?}",
        observed.events,
    );
}

/// A group host that lets a group child's tool attempt start only once the
/// attempt before it settled: the forced-serial negative control's host. It
/// layers the tier's own host, so everything else about the tier is
/// unchanged.
struct SerialGroupHost {
    turn: tokio::sync::Semaphore,
}

impl SerialGroupHost {
    fn over(host: Arc<dyn crate::EffectHost>) -> Arc<dyn crate::EffectHost> {
        Arc::new(crate::testing::LayeredEffectHost::new(
            host,
            Arc::new(Self {
                turn: tokio::sync::Semaphore::new(1),
            }),
        ))
    }
}

#[async_trait::async_trait]
impl crate::testing::EffectLayer for SerialGroupHost {
    async fn execute_effect(
        &self,
        inner: &dyn crate::RuntimeEffectController,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if !matches!(
            envelope.command,
            crate::RuntimeEffectCommand::ToolAttempt { .. }
        ) {
            return inner.execute_effect(envelope, local_executor).await;
        }
        let _one_at_a_time = self.turn.acquire().await;
        inner.execute_effect(envelope, local_executor).await
    }
}

/// The negative control: the barrier law over a [`SerialGroupHost`] must fail
/// for every producer, naming the members that never started. Proves the
/// barrier law can fail on this tier, so its passes mean something.
///
/// One serial host layers the tier's host for every producer: a host routes
/// the group children it opens, so it must outlive every scenario it served.
pub async fn forced_serial_host_fails_the_barrier(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producers: Vec<ToolBatchProducer>,
) {
    assert!(
        !producers.is_empty(),
        "a tier registers at least one product producer, or the law runs on nothing"
    );
    let serial_host = SerialGroupHost::over(effect_host);
    for producer in producers {
        let context = format!("{prefix}/{}", producer.label);
        let serial = plan("forced_serial", &leaf_routes(8));
        let observed = run_scenario(
            prefix,
            Arc::clone(&serial_host),
            &stores,
            &runner,
            &producer,
            &serial,
            Schedule::NEGATIVE_CONTROL,
            BTreeMap::new(),
        )
        .await;
        let Err(failure) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_activation_shape(&format!("{context} forced-serial"), &serial, &observed)
        })) else {
            panic!("the barrier law must fail over a host that runs group members one at a time");
        };
        let message = failure
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                failure
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
            })
            .unwrap_or_default();
        assert!(
            !observed.never_started.is_empty(),
            "{context}: a serial host leaves members unstarted; log: {:?}",
            observed.events,
        );
        assert!(
            message.contains("never started")
                && observed
                    .never_started
                    .iter()
                    .all(|member| message.contains(member.as_str())),
            "{context}: the failure must name the members that never started ({:?}): {message}",
            observed.never_started,
        );
    }
}
