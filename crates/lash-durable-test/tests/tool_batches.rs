//! The tool-group laws through a host's `send()` on a served node (ADR 0116
//! §7.1–7.2, FIG-4546; ported by FIG-5210 from the deleted
//! lash-conformance `tool_batch_parallelism` and `batch_sugar` suites).
//!
//! Every law runs real turns: a host creates a session on a core over the
//! tier's database and sends it an input, and the core's own node claims
//! the session and runs the turn on the durable path with the law's
//! scripted model and host tools. Nothing drives a turn in process.
//!
//! **Overlap is proven by rendezvous, never by wall time.** Every leaf of a
//! width-n group records that it started and refuses to answer until the
//! whole width has started. A node that ran the leaves one at a time could
//! not leave the first leaf, and the turn would hang until the watchdog
//! fails the law. A node that overlaps them leaves a log in which every
//! start precedes the first answer.
//!
//! The product surfaces that spell a group (the producers): parallel native
//! calls, one `batch` wrapper, `batch` wrappers beside native calls, and an
//! RLM cell's `Promise.all` and `Promise.allSettled`. A leaf takes one of
//! the routes the surface reaches: a listed catalog tool, a round member
//! that parks on its completion key and is resolved out of band, or (on a
//! cell) an unlisted tool a deferred resolver grants. A group issued from inside a
//! process body is FIG-5216's: the core's node serves no process yet.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::llm::types::LlmResponse;
use lash_core::{ToolCall, ToolOutcome};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, World};

/// The widths every producer runs at; 64 is `batch`'s ceiling.
const WIDTHS: [usize; 3] = [2, 8, 64];

/// The `max_tool_calls` the limit laws' sessions record.
const LIMIT: usize = 4;

/// The tool every listed leaf calls.
const LEAF: &str = "leaf";
/// The leaf that parks on its completion key.
const DEFERRED_LEAF: &str = "leaf_deferred";
/// The leaf no catalog lists: a cell reaches it only through a grant.
const GRANTED_LEAF: &str = "leaf_granted";

/// How a leaf is dispatched inside its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// A listed catalog tool.
    Leaf,
    /// A tool that parks on its completion key and settles out of band.
    Deferred,
    /// An unlisted tool the cell's deferred resolver grants.
    Granted,
}

impl Route {
    fn tool(self) -> &'static str {
        match self {
            Self::Leaf => LEAF,
            Self::Deferred => DEFERRED_LEAF,
            Self::Granted => GRANTED_LEAF,
        }
    }
}

/// What happened to a leaf, in the order it happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Started(usize),
    Answered(usize),
}

/// One scenario's rendezvous: its leaves, whether they wait for each other,
/// and the log of what they did.
struct Scenario {
    width: usize,
    gated: bool,
    /// A leaf's own wait list, in place of the whole width.
    waits: BTreeMap<usize, Vec<usize>>,
    events: Mutex<Vec<Event>>,
    changed: tokio::sync::watch::Sender<usize>,
}

impl Scenario {
    fn new(width: usize, gated: bool, waits: BTreeMap<usize, Vec<usize>>) -> Self {
        Self {
            width,
            gated,
            waits,
            events: Mutex::default(),
            changed: tokio::sync::watch::channel(0).0,
        }
    }

    fn events(&self) -> Vec<Event> {
        self.events.lock_recover().clone()
    }

    /// A leaf's start and its log entry are one critical section: a
    /// preempted leaf cannot let a waiter release on a start the log does
    /// not show yet.
    fn record(&self, event: Event) {
        self.events.lock_recover().push(event);
        self.changed.send_modify(|count| *count += 1);
    }

    fn required(&self, position: usize) -> Vec<usize> {
        if !self.gated {
            return Vec::new();
        }
        self.waits
            .get(&position)
            .cloned()
            .unwrap_or_else(|| (0..self.width).collect())
    }

    /// Wait until every leaf in `required` has started; no clock.
    async fn wait_for(&self, required: &[usize]) {
        let mut changed = self.changed.subscribe();
        loop {
            let events = self.events();
            if required
                .iter()
                .all(|leaf| events.contains(&Event::Started(*leaf)))
            {
                return;
            }
            changed
                .changed()
                .await
                .expect("the scenario outlives its leaves");
        }
    }

    fn started(&self) -> Vec<usize> {
        self.events()
            .iter()
            .filter_map(|event| match event {
                Event::Started(leaf) => Some(*leaf),
                Event::Answered(_) => None,
            })
            .collect()
    }

    fn answered(&self) -> Vec<usize> {
        self.events()
            .iter()
            .filter_map(|event| match event {
                Event::Answered(leaf) => Some(*leaf),
                Event::Started(_) => None,
            })
            .collect()
    }

    fn started_before_first_answer(&self) -> usize {
        self.events()
            .iter()
            .take_while(|event| matches!(event, Event::Started(_)))
            .count()
    }

    fn peak_in_flight(&self) -> usize {
        let mut in_flight = 0_usize;
        let mut peak = 0;
        for event in self.events() {
            match event {
                Event::Started(_) => in_flight += 1,
                Event::Answered(_) => in_flight = in_flight.saturating_sub(1),
            }
            peak = peak.max(in_flight);
        }
        peak
    }
}

/// Every scenario of one core, by name.
#[derive(Default)]
struct Rendezvous {
    scenarios: Mutex<BTreeMap<String, Arc<Scenario>>>,
}

impl Rendezvous {
    fn open(&self, name: &str, scenario: Scenario) -> Arc<Scenario> {
        let scenario = Arc::new(scenario);
        self.scenarios
            .lock_recover()
            .insert(name.to_owned(), Arc::clone(&scenario));
        scenario
    }

    fn scenario(&self, name: &str) -> Arc<Scenario> {
        self.scenarios
            .lock_recover()
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("no scenario `{name}` is open"))
    }
}

fn leaf_definition(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    let definition = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A rendezvous leaf: answers once its whole group has started.",
        object.clone(),
        object,
    )
    .expect("the leaf's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name));
    if name == DEFERRED_LEAF {
        definition
            .with_declaration(
                lash_core::ToolDeclaration::deferring(),
                Some(lash_core::ParkBound::Within(
                    std::time::Duration::from_secs(120),
                )),
            )
            .expect("a deferring tool declares its park bound")
    } else {
        definition
    }
}

/// The leaf answer: the same whether or not the leaf waited, so a gated
/// run and an ungated one answer alike.
fn leaf_answer(position: usize) -> serde_json::Value {
    serde_json::json!({ "leaf": position })
}

/// The leaves of every scenario on one core.
struct Leaves {
    rendezvous: Arc<Rendezvous>,
    backend: lash::Backend,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Leaves {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        [LEAF, DEFERRED_LEAF]
            .into_iter()
            .map(|name| leaf_definition(name).manifest())
            .collect()
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        [LEAF, DEFERRED_LEAF, GRANTED_LEAF]
            .into_iter()
            .map(leaf_definition)
            .find(|definition| definition.id() == id)
            .map(|definition| definition.manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        [LEAF, DEFERRED_LEAF, GRANTED_LEAF]
            .contains(&name)
            .then(|| Arc::new(leaf_definition(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let name = call.args["scenario"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        // The kernel preserves a TypeScript number's Float spelling
        // (K-EFF-002); a whole numeric argument is still its leaf ordinal.
        let position = call.args["position"]
            .as_f64()
            .expect("a numeric leaf ordinal");
        assert!(position.fract() == 0.0 && (0.0..WIDTHS[2] as f64).contains(&position));
        let position = position as usize;
        let scenario = self.rendezvous.scenario(&name);
        scenario.record(Event::Started(position));
        let required = scenario.required(position);
        if call.name() == DEFERRED_LEAF {
            // It joins the rendezvous like any leaf: its start is recorded,
            // and the task that resolves it holds its slot until the width
            // has started.
            let key = call
                .context
                .completion_key()
                .expect("a deferring leaf's round pins its completion wait");
            let backend = self.backend.clone();
            tokio::spawn(async move {
                scenario.wait_for(&required).await;
                scenario.record(Event::Answered(position));
                lash_core::waits::resolve_host(
                    &backend,
                    key.as_str(),
                    lash_core::Resolution::Ok(leaf_answer(position)),
                )
                .await
                .expect("the deferred leaf's wait resolves");
            });
            return lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new());
        }
        scenario.wait_for(&required).await;
        scenario.record(Event::Answered(position));
        ToolOutcome::ok(leaf_answer(position)).into()
    }
}

/// Grants the unlisted leaf to a cell that names it.
struct GrantedLeaves;

#[async_trait::async_trait]
impl lash::tools::DeferredToolResolver for GrantedLeaves {
    async fn resolve(
        &self,
        _cx: &lash::tools::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash::tools::DeferredToolResolution> {
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == format!("tools.{GRANTED_LEAF}") {
                    lash::tools::DeferredToolResolution::Resolved(Box::new(
                        lash::tools::DeferredToolGrant::new(leaf_definition(GRANTED_LEAF))
                            .with_source_id(lash::tools::PLUGIN_TOOL_SOURCE_ID),
                    ))
                } else {
                    lash::tools::DeferredToolResolution::NotAvailable
                };
                ((*path).to_owned(), resolution)
            })
            .collect()
    }
}

/// A producer's world: the standard protocol (with `batch` at its default
/// ceiling) or the RLM protocol with the granted leaf resolvable, and the
/// leaves.
struct Producers {
    world: World,
    rendezvous: Arc<Rendezvous>,
}

impl Producers {
    async fn new(tier: Tier, code: bool) -> Option<Self> {
        let rendezvous = Arc::new(Rendezvous::default());
        let leaves = Arc::clone(&rendezvous);
        let world = World::new(tier, move |backend| {
            let builder = if code {
                lash::LashCore::rlm_builder(
                    backend.clone(),
                    served::rlm(
                        backend,
                        Some(Arc::new(GrantedLeaves)),
                        sim::untimed_workers(),
                    ),
                )
            } else {
                lash::LashCore::standard_builder(backend.clone())
            };
            builder.tools(Arc::new(Leaves {
                rendezvous: leaves,
                backend: backend.clone(),
            }))
        })
        .await?;
        Some(Self { world, rendezvous })
    }
}

/// A product surface that issues one group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Producer {
    /// One model response of n native calls.
    Native,
    /// One `batch` wrapper over the whole width.
    Batch,
    /// Two `batch` wrappers beside two native calls, in plan order.
    BatchBesideNative,
    /// One cell awaiting the width with `Promise.all`.
    PromiseAll,
    /// One cell awaiting the width with `Promise.allSettled`.
    PromiseAllSettled,
}

const STANDARD: [Producer; 3] = [
    Producer::Native,
    Producer::Batch,
    Producer::BatchBesideNative,
];
const CODE: [Producer; 2] = [Producer::PromiseAll, Producer::PromiseAllSettled];

fn leaf_args(name: &str, position: usize) -> serde_json::Value {
    serde_json::json!({ "scenario": name, "position": position })
}

fn native(name: &str, position: usize, route: Route) -> lash_core::LlmOutputPart {
    served::call(
        &format!("call-{position}"),
        route.tool(),
        leaf_args(name, position),
    )
}

fn wrapper(call_id: &str, name: &str, members: &[(usize, Route)]) -> lash_core::LlmOutputPart {
    served::call(
        call_id,
        "batch",
        serde_json::json!({
            "tool_calls": members
                .iter()
                .map(|(position, route)| serde_json::json!({
                    "tool": route.tool(),
                    "parameters": leaf_args(name, *position),
                }))
                .collect::<Vec<_>>()
        }),
    )
}

fn aggregate(name: &str, aggregate: &str, leaves: &[(usize, Route)]) -> String {
    let calls = leaves
        .iter()
        .map(|(position, route)| {
            format!(
                "  tools.{}({{ scenario: \"{name}\", position: {position} }})",
                route.tool()
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("await {aggregate}([\n{calls}\n])")
}

impl Producer {
    fn code(self) -> bool {
        matches!(self, Self::PromiseAll | Self::PromiseAllSettled)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Native => "parallel-native-calls",
            Self::Batch => "batch",
            Self::BatchBesideNative => "batch-beside-native-calls",
            Self::PromiseAll => "promise-all",
            Self::PromiseAllSettled => "promise-all-settled",
        }
    }

    fn aggregate(self) -> &'static str {
        match self {
            Self::PromiseAllSettled => "Promise.allSettled",
            _ => "Promise.all",
        }
    }

    /// Every route the surface reaches. A model names any listed tool, and
    /// a turn's round pins a deferring member's completion wait. A cell's
    /// resolver grants an unlisted tool; a cell's call pins no completion
    /// wait, so a deferring body there is refused typed (FIG-5174).
    fn routes(self) -> Vec<Route> {
        if self.code() {
            vec![Route::Leaf, Route::Granted]
        } else {
            vec![Route::Leaf, Route::Deferred]
        }
    }

    /// What the session's `max_tool_calls` counts on this surface.
    fn limit_scope(self) -> Scope {
        if self.code() {
            Scope::Cell
        } else {
            Scope::Step
        }
    }

    /// The script that issues `plan` as one group.
    fn script(self, name: &str, plan: &[Route]) -> Vec<LlmResponse> {
        let leaves = plan.iter().copied().enumerate().collect::<Vec<_>>();
        match self {
            Self::Native => vec![served::response(
                leaves
                    .iter()
                    .map(|(position, route)| native(name, *position, *route))
                    .collect(),
            )],
            Self::Batch => vec![served::response(vec![wrapper("batch-call", name, &leaves)])],
            Self::BatchBesideNative => {
                let Some((first, rest)) = leaves.split_first() else {
                    return vec![served::response(Vec::new())];
                };
                let mut parts = vec![native(name, first.0, first.1)];
                let (wrapper_a, tail) = rest.split_at(rest.len() / 2);
                if !wrapper_a.is_empty() {
                    parts.push(wrapper("batch-a", name, wrapper_a));
                }
                if let Some((second, wrapper_b)) = tail.split_first() {
                    parts.push(native(name, second.0, second.1));
                    if !wrapper_b.is_empty() {
                        parts.push(wrapper("batch-b", name, wrapper_b));
                    }
                }
                vec![served::response(parts)]
            }
            Self::PromiseAll | Self::PromiseAllSettled => vec![served::cell(&format!(
                "finish({});",
                aggregate(name, self.aggregate(), &leaves)
            ))],
        }
    }

    /// The script that issues `plan` as two groups in sequence, its first
    /// `first` leaves and then the rest: two steps of native calls, or two
    /// aggregates in one cell. `None` where the surface spells no sequence.
    fn staged(self, name: &str, plan: &[Route], first: usize) -> Option<Vec<LlmResponse>> {
        let leaves = plan.iter().copied().enumerate().collect::<Vec<_>>();
        let (head, tail) = leaves.split_at(first.min(leaves.len()));
        match self {
            Self::Native => {
                let step = |leaves: &[(usize, Route)]| {
                    served::response(
                        leaves
                            .iter()
                            .map(|(position, route)| native(name, *position, *route))
                            .collect(),
                    )
                };
                Some(vec![step(head), step(tail)])
            }
            Self::PromiseAll | Self::PromiseAllSettled => Some(vec![served::cell(&format!(
                "const first = {};\nconst rest = {};\nfinish([first, rest]);",
                aggregate(name, self.aggregate(), head),
                aggregate(name, self.aggregate(), tail),
            ))]),
            Self::Batch | Self::BatchBesideNative => None,
        }
    }
}

/// Every leaf answer in `value`, in document order.
fn leaves_in(value: &serde_json::Value, into: &mut Vec<usize>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(leaf) = map.get("leaf").and_then(serde_json::Value::as_u64) {
                into.push(leaf as usize);
            }
            for (key, value) in map {
                if key != "leaf" {
                    leaves_in(value, into);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                leaves_in(item, into);
            }
        }
        _ => {}
    }
}

/// The leaf answers the consumer read, in the order it read them: the
/// transcript's tool results (a wrapper's rows in member order) on the
/// standard protocol, the cell's final value on a cell.
fn replies(producer: Producer, output: &lash::TurnOutput) -> Vec<usize> {
    let mut replies = Vec::new();
    if producer.code() {
        let value = output
            .final_value()
            .cloned()
            .or_else(|| {
                output
                    .assistant_message()
                    .and_then(|message| serde_json::from_str(message).ok())
            })
            .unwrap_or(serde_json::Value::Null);
        leaves_in(&value, &mut replies);
    } else {
        for answered in served::results(output) {
            leaves_in(&answered.value(), &mut replies);
        }
    }
    replies
}

/// Run `plan` through `producer` as scenario `name`.
async fn run_group(
    producers: &Producers,
    producer: Producer,
    name: &str,
    plan: &[Route],
    gated: bool,
    waits: BTreeMap<usize, Vec<usize>>,
) -> (Arc<Scenario>, lash::TurnOutput) {
    let scenario = producers
        .rendezvous
        .open(name, Scenario::new(plan.len(), gated, waits));
    let output = producers
        .world
        .run(name, served::spec(1024), producer.script(name, plan))
        .await;
    (scenario, output)
}

/// Every leaf started, every start before the first answer, peak in-flight
/// equal to the width, every leaf answered once, and the consumer read the
/// answers in call order however they settled.
fn assert_activation_shape(
    context: &str,
    producer: Producer,
    width: usize,
    scenario: &Scenario,
    output: &lash::TurnOutput,
) {
    served::assert_answered(context, output);
    let started = scenario.started().into_iter().collect::<BTreeSet<_>>();
    let never = (0..width)
        .filter(|leaf| !started.contains(leaf))
        .collect::<Vec<_>>();
    assert!(
        never.is_empty(),
        "{context}: the group did not overlap: leaves {never:?} never started; started: {started:?}"
    );
    assert_eq!(
        scenario.started_before_first_answer(),
        width,
        "{context}: every one of the {width} leaves starts before the first answer; log: {:?}",
        scenario.events()
    );
    assert_eq!(
        scenario.peak_in_flight(),
        width,
        "{context}: peak in-flight equals the width; log: {:?}",
        scenario.events()
    );
    let mut answered = scenario.answered();
    answered.sort_unstable();
    assert_eq!(
        answered,
        (0..width).collect::<Vec<_>>(),
        "{context}: every leaf answers exactly once"
    );
    assert_eq!(
        replies(producer, output),
        (0..width).collect::<Vec<_>>(),
        "{context}: the consumer reads the answers in call order however they settled"
    );
}

fn producers_of(code: bool) -> Vec<Producer> {
    if code {
        CODE.to_vec()
    } else {
        STANDARD.to_vec()
    }
}

/// Every member of a width-n group starts before any finishes, for every
/// producer, at widths 2, 8 and 64 and over every route the producer
/// reaches; and the same width-8 group run without the rendezvous answers
/// alike, so the rendezvous changes the schedule and nothing else.
async fn tool_group_members_start_before_any_finishes(tier: Tier) {
    for code in [false, true] {
        let Some(producers) = Producers::new(tier, code).await else {
            return;
        };
        for producer in producers_of(code) {
            let label = producer.label();
            for width in WIDTHS {
                let name = format!("{label}-width-{width}");
                let plan = vec![Route::Leaf; width];
                let (scenario, output) =
                    run_group(&producers, producer, &name, &plan, true, BTreeMap::new()).await;
                assert_activation_shape(&name, producer, width, &scenario, &output);
            }

            // Every route the producer reaches, siblings of one group.
            let name = format!("{label}-routes");
            let routes = producer.routes();
            let plan = routes
                .iter()
                .copied()
                .cycle()
                .take(2 * routes.len())
                .collect::<Vec<_>>();
            let (scenario, output) =
                run_group(&producers, producer, &name, &plan, true, BTreeMap::new()).await;
            assert_activation_shape(&name, producer, plan.len(), &scenario, &output);

            // The differential: the same program without the rendezvous.
            let plan = vec![Route::Leaf; 8];
            let gated = format!("{label}-differential-gated");
            let (scenario, concurrent) =
                run_group(&producers, producer, &gated, &plan, true, BTreeMap::new()).await;
            assert_activation_shape(&gated, producer, 8, &scenario, &concurrent);
            let free = format!("{label}-differential-free");
            let (_, serial_safe) =
                run_group(&producers, producer, &free, &plan, false, BTreeMap::new()).await;
            served::assert_answered(&free, &serial_safe);
            assert_eq!(
                replies(producer, &concurrent),
                replies(producer, &serial_safe),
                "{label}: the rendezvous changes the schedule, not the answers"
            );
        }
        producers.world.shutdown().await;
    }
}

/// The reverse dependency: the input-first leaf answers only after the
/// input-last leaf started. A loop that runs a group in input order can
/// never get there, whatever its per-leaf latency.
async fn tool_group_reverse_dependency(tier: Tier) {
    for code in [false, true] {
        let Some(producers) = Producers::new(tier, code).await else {
            return;
        };
        for producer in producers_of(code) {
            let name = format!("{}-reverse", producer.label());
            let waits = BTreeMap::from([
                (0, vec![3]),
                (1, Vec::new()),
                (2, Vec::new()),
                (3, Vec::new()),
            ]);
            let (scenario, output) =
                run_group(&producers, producer, &name, &[Route::Leaf; 4], true, waits).await;
            served::assert_answered(&name, &output);
            let events = scenario.events();
            let first_answered = events
                .iter()
                .position(|event| *event == Event::Answered(0))
                .unwrap_or_else(|| panic!("{name}: leaf 0 never answered: {events:?}"));
            let last_started = events
                .iter()
                .position(|event| *event == Event::Started(3))
                .unwrap_or_else(|| panic!("{name}: leaf 3 never started: {events:?}"));
            assert!(
                first_answered > last_started,
                "{name}: the input-first leaf answers only after the input-last one started: {events:?}"
            );
        }
        producers.world.shutdown().await;
    }
}

/// What a session's `max_tool_calls` counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// One cell's calls, across its groups.
    Cell,
    /// One step's group, on a protocol without cells.
    Step,
}

/// The refusal sentence in `text`: from its opening words through the count
/// of the calls it refused.
fn refusal_in(text: &str) -> Option<String> {
    let start = text.find("tool call limit exceeded")?;
    let refusal = &text[start..];
    let end = refusal.find(" more")? + " more".len();
    Some(refusal[..end].to_owned())
}

/// The sentence a surface is refused with after `counted` calls, asking for
/// `requested` more. A step of a protocol without cells counts as a cell
/// does.
fn expected_refusal(counted: usize, requested: usize) -> String {
    let exceeded = lash::ToolCallLimitExceeded {
        scope: lash::ToolCallLimitScope::Cell,
        limit: lash::MaxToolCalls::new(LIMIT),
        counted,
        requested,
    };
    let sentence = refusal_in(&exceeded.to_string()).expect("the refusal words its sentence");
    assert!(sentence.contains(&format!("max_tool_calls = {LIMIT}")));
    sentence
}

/// Every refusal the model was shown in scenario `name`, in the order it
/// was first shown.
fn refusals(world: &World, name: &str) -> Vec<String> {
    let mut shown = Vec::new();
    for request in world.requests(name) {
        // The request is JSON: its strings are escaped once.
        let request = request.replace("\\\"", "\"");
        if let Some(refusal) = refusal_in(&request)
            && !shown.contains(&refusal)
        {
            shown.push(refusal);
        }
    }
    shown
}

/// A protocol round of exactly `max_tool_calls` calls runs, and a round
/// of one more is refused whole before any body starts. Kernel cells issue
/// individual effects, not protocol rounds: their cumulative limit is pinned
/// by `tool_call_limit_staged_calls`.
async fn tool_call_limit_admits_the_limit_and_refuses_the_group_past_it(tier: Tier) {
    let Some(producers) = Producers::new(tier, false).await else {
        return;
    };
    for producer in STANDARD {
        let label = producer.label();
        let at = format!("{label}-limit-at");
        let plan = vec![Route::Leaf; LIMIT];
        let scenario = producers
            .rendezvous
            .open(&at, Scenario::new(LIMIT, true, BTreeMap::new()));
        let output = producers
            .world
            .run(&at, served::spec(LIMIT), producer.script(&at, &plan))
            .await;
        assert_activation_shape(&at, producer, LIMIT, &scenario, &output);
        assert_eq!(
            refusals(&producers.world, &at),
            Vec::<String>::new(),
            "{at}: a round of exactly max_tool_calls calls is not refused"
        );

        let past = format!("{label}-limit-past");
        let plan = vec![Route::Leaf; LIMIT + 1];
        let scenario = producers
            .rendezvous
            .open(&past, Scenario::new(LIMIT + 1, false, BTreeMap::new()));
        producers
            .world
            .run(&past, served::spec(LIMIT), producer.script(&past, &plan))
            .await;
        assert_eq!(
            scenario.started(),
            Vec::<usize>::new(),
            "{past}: no call of a refused round runs"
        );
        assert_eq!(
            refusals(&producers.world, &past).first(),
            Some(&expected_refusal(0, LIMIT + 1)),
            "{past}: the model is shown the refusal, naming the limit"
        );
    }
    producers.world.shutdown().await;
}

/// Calls issued in sequence are counted the way the surface's scope says: a
/// cell's total (after `max_tool_calls` calls its next call is refused, and
/// the calls before it ran untouched), or a step's group (each step starts
/// its own count, so a second step of `max_tool_calls` calls runs and one of
/// a call more is refused).
async fn tool_call_limit_staged_calls(tier: Tier) {
    for code in [false, true] {
        let Some(producers) = Producers::new(tier, code).await else {
            return;
        };
        let staged = if code {
            CODE.to_vec()
        } else {
            vec![Producer::Native]
        };
        for producer in staged {
            let label = producer.label();
            let scope = producer.limit_scope();
            if scope == Scope::Step {
                // Twice the limit, half at a time: the count is not a total.
                let twice = format!("{label}-limit-twice");
                let plan = vec![Route::Leaf; 2 * LIMIT];
                let scenario = producers
                    .rendezvous
                    .open(&twice, Scenario::new(2 * LIMIT, false, BTreeMap::new()));
                let script = producer
                    .staged(&twice, &plan, LIMIT)
                    .expect("a staged script");
                let output = producers
                    .world
                    .run(&twice, served::spec(LIMIT), script)
                    .await;
                served::assert_answered(&twice, &output);
                let mut answered = scenario.answered();
                answered.sort_unstable();
                assert_eq!(
                    answered,
                    (0..2 * LIMIT).collect::<Vec<_>>(),
                    "{twice}: both groups of max_tool_calls calls run"
                );
                assert_eq!(
                    refusals(&producers.world, &twice),
                    Vec::<String>::new(),
                    "{twice}: two groups of max_tool_calls calls in sequence are not refused"
                );
            }

            // The first group fills the limit; the second asks past it.
            let (rest, counted) = match scope {
                Scope::Cell => (1, LIMIT),
                Scope::Step => (LIMIT + 1, 0),
            };
            let past = format!("{label}-limit-staged");
            let plan = vec![Route::Leaf; LIMIT + rest];
            let scenario = producers
                .rendezvous
                .open(&past, Scenario::new(LIMIT + rest, false, BTreeMap::new()));
            let script = producer
                .staged(&past, &plan, LIMIT)
                .expect("a staged script");
            producers
                .world
                .run(&past, served::spec(LIMIT), script)
                .await;
            let mut answered = scenario.answered();
            answered.sort_unstable();
            assert_eq!(
                answered,
                (0..LIMIT).collect::<Vec<_>>(),
                "{past}: the calls before the refused ones run untouched, and no refused call runs"
            );
            assert_eq!(
                scenario.started().len(),
                LIMIT,
                "{past}: no refused call starts"
            );
            assert_eq!(
                refusals(&producers.world, &past).first(),
                Some(&expected_refusal(counted, rest)),
                "{past}: the model is shown the refusal at the call past the limit"
            );
        }
        producers.world.shutdown().await;
    }
}

// --- The `batch` sugar laws (ADR 0116 §7.2) --------------------------------

/// `batch`'s ceiling: the most members one wrapper takes.
const CEILING: usize = 64;

/// One executed member body, as its tool saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Execution {
    tool: String,
    value: String,
    call_id: lash_core::ToolCallId,
}

/// What the sugar tools and their check saw.
#[derive(Default)]
struct Witness {
    executions: Mutex<Vec<Execution>>,
    /// Every tool name the before-tool check was asked about.
    hooked: Mutex<Vec<String>>,
}

impl Witness {
    fn executions(&self) -> Vec<Execution> {
        self.executions.lock_recover().clone()
    }

    fn executed(&self, tool: &str, value: &str) -> usize {
        self.executions()
            .iter()
            .filter(|execution| execution.tool == tool && execution.value == value)
            .count()
    }

    fn hooked(&self, tool: &str) -> bool {
        self.hooked.lock_recover().iter().any(|name| name == tool)
    }
}

const SUGAR_TOOLS: [&str; 2] = ["echo", "guarded"];

fn sugar_tool(name: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Records each execution and answers its value.",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("the sugar tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// `echo` answers at once; `guarded` is denied by the before-tool check.
struct SugarTools {
    witness: Arc<Witness>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SugarTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        SUGAR_TOOLS
            .iter()
            .map(|name| sugar_tool(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        SUGAR_TOOLS
            .contains(&name)
            .then(|| Arc::new(sugar_tool(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let value = call.args["value"].as_str().unwrap_or_default().to_owned();
        self.witness.executions.lock_recover().push(Execution {
            tool: call.name().to_owned(),
            value: value.clone(),
            call_id: call.context.call_id().clone(),
        });
        ToolOutcome::ok(serde_json::json!({ "tool": call.name(), "value": value })).into()
    }
}

/// The sugar tools and their before-check: it records every tool it is
/// asked about and denies `guarded`.
fn sugar_plugin(witness: Arc<Witness>) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    let hooked = Arc::clone(&witness);
    let spec = lash_core::facade_support::PluginSpec::new()
        .with_tool_provider(Arc::new(SugarTools { witness }))
        .with_tool_args_check(
            lash_core::hook_key!("guard"),
            Arc::new(move |input| {
                let tool = input.prepared.tool_name().to_owned();
                hooked.hooked.lock_recover().push(tool.clone());
                Box::pin(async move {
                    Ok(if tool == "guarded" {
                        lash_core::facade_support::BeforeToolDecision::Deny(
                            lash_core::ToolFailure::tool(
                                lash_core::ToolFailureClass::PermissionDenied,
                                "approval_denied",
                                "the law denies `guarded`",
                            ),
                        )
                    } else {
                        lash_core::facade_support::BeforeToolDecision::Allow
                    })
                })
            }),
        );
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("tool-semantics-batch-sugar"),
        spec,
    ))
}

/// A core whose standard protocol offers `batch` (at its default ceiling)
/// or withholds it, with the sugar tools.
async fn sugar_world(tier: Tier, batch: bool, witness: &Arc<Witness>) -> Option<World> {
    let plugin = sugar_plugin(Arc::clone(witness));
    World::new(tier, move |backend| {
        let config = if batch {
            lash::plugins::StandardProtocolConfig::default()
        } else {
            lash::plugins::StandardProtocolConfig::default()
                .batch(lash::plugins::BatchSugar::Disabled)
        };
        lash::LashCore::builder(backend.clone())
            .protocol_plugin(Arc::new(
                lash::plugins::StandardProtocolPluginFactory::with_config(config),
            ))
            .plugin(plugin)
    })
    .await
}

fn echo(call_id: &str, tool: &str, value: &str) -> lash_core::LlmOutputPart {
    served::call(call_id, tool, serde_json::json!({ "value": value }))
}

fn sugar(call_id: &str, members: serde_json::Value) -> lash_core::LlmOutputPart {
    lash_core::LlmOutputPart::ToolCall {
        call_id: call_id.to_owned(),
        tool_name: "batch".to_owned(),
        input_json: serde_json::json!({ "tool_calls": members }).to_string(),
        replay: Some(lash_core::llm::types::ProviderReplayMeta {
            item_id: Some(format!("provider-{call_id}")),
            ..lash_core::llm::types::ProviderReplayMeta::default()
        }),
    }
}

fn member(tool: &str, value: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "tool": tool, "parameters": { "value": value } })
}

fn members(tool: &str, values: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        values
            .iter()
            .map(|value| member(tool, serde_json::json!(value)))
            .collect(),
    )
}

/// The results answered under `provider_call_id`.
fn answered_under(output: &lash::TurnOutput, provider_call_id: &str) -> Vec<served::Answered> {
    served::results(output)
        .into_iter()
        .filter(|answered| answered.provider_call_id == provider_call_id)
        .collect()
}

/// A wrapper result's rows, as `(index, tool, success)`.
fn rows(answered: &served::Answered) -> Vec<(u64, String, bool)> {
    answered.value()["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    (
                        row["index"].as_u64().unwrap_or(u64::MAX),
                        row["tool"].as_str().unwrap_or_default().to_owned(),
                        row["success"].as_bool().unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A result is its call's failure: a refusal the transcript carries as a
/// failure outcome, or no JSON answer at all.
fn failed(answered: &served::Answered) -> bool {
    let value = answered.value();
    value.is_null() || value.pointer("/outcome/status") == Some(&serde_json::json!("failure"))
}

/// Every call the transcript holds is one the model issued, and every
/// result pairs with one: a member is named under its wrapper and is no
/// call of its own.
fn assert_no_member_is_a_call(context: &str, output: &lash::TurnOutput) {
    let uncorrelated = served::calls(output)
        .iter()
        .filter(|call| call.provider_call_id.is_empty())
        .map(|call| format!("call of {}", call.tool))
        .chain(
            served::results(output)
                .into_iter()
                .filter(|answered| answered.provider_call_id.is_empty())
                .map(|answered| format!("result of {}", answered.tool)),
        )
        .collect::<Vec<_>>();
    assert!(
        uncorrelated.is_empty(),
        "{context}: no member is a call of its own: {uncorrelated:?}"
    );
}

/// 64 members run; 65 refuse the whole wrapper and start nothing; an empty
/// or malformed list refuses the wrapper; a nested `batch`, an unavailable
/// tool, a schema failure and a denied approval are refused rows; the
/// wrapper runs no hook, so its approval grants nothing; repeated model call
/// ids, identical arguments and a reused frame alias no member; and a
/// disabled `batch` is an unknown tool.
async fn batch_admission_and_identity_contract(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = sugar_world(tier, true, &witness).await else {
        return;
    };
    let full = (0..CEILING)
        .map(|index| format!("m{index}"))
        .collect::<Vec<_>>();
    let full = full.iter().map(String::as_str).collect::<Vec<_>>();
    let over = vec!["over"; CEILING + 1];
    let script = vec![
        served::response(vec![
            echo("same", "echo", "native-1"),
            echo("same", "echo", "native-2"),
            sugar("wfull", members("echo", &full)),
            sugar("wover", members("echo", &over)),
            sugar("wempty", serde_json::json!([])),
            sugar("wmalformed", serde_json::json!("echo")),
            sugar(
                "wmixed",
                serde_json::json!([
                    { "tool": "batch", "parameters": { "tool_calls": [] } },
                    member("ghost", serde_json::json!("ghost")),
                    member("echo", serde_json::json!(5)),
                    member("guarded", serde_json::json!("guarded")),
                    member("echo", serde_json::json!("twin")),
                    member("echo", serde_json::json!("twin")),
                ]),
            ),
        ]),
        // A later step reuses the wrapper's provider call id.
        served::response(vec![sugar("wmixed", members("echo", &["again"]))]),
    ];
    let context = "batch-admission";
    let output = world.run(context, served::spec(1024), script).await;
    served::assert_answered(context, &output);

    for value in &full {
        assert_eq!(
            witness.executed("echo", value),
            1,
            "{context}: member `{value}` runs once"
        );
    }
    assert_eq!(
        witness.executed("echo", "over"),
        0,
        "{context}: a wrapper over {CEILING} starts nothing"
    );
    for value in ["native-1", "native-2"] {
        assert_eq!(
            witness.executed("echo", value),
            1,
            "{context}: a repeated call id aliases nothing"
        );
    }
    assert_eq!(
        witness.executed("echo", "twin"),
        2,
        "{context}: identical arguments alias no member"
    );
    assert_eq!(
        witness.executed("echo", "again"),
        1,
        "{context}: a reused frame aliases no member"
    );
    assert_eq!(
        witness.executed("guarded", "guarded"),
        0,
        "{context}: a denied member never runs"
    );
    assert!(
        witness
            .executions()
            .iter()
            .all(|execution| execution.value != "5"),
        "{context}: a schema failure never runs"
    );
    assert!(
        !witness.hooked("batch"),
        "{context}: the wrapper is not a tool invocation and runs no hook, so it grants nothing"
    );
    assert!(
        witness.hooked("guarded"),
        "{context}: every member is admitted on its own"
    );

    let wfull = answered_under(&output, "wfull");
    assert_eq!(wfull.len(), 1, "{context}: one result per wrapper");
    let wfull_rows = rows(&wfull[0]);
    assert_eq!(wfull_rows.len(), CEILING);
    assert!(
        wfull_rows
            .iter()
            .enumerate()
            .all(|(index, (row, tool, ok))| *row == index as u64 && tool == "echo" && *ok),
        "{context}: every full row answers in member order: {wfull_rows:?}"
    );
    for refused in ["wover", "wempty", "wmalformed"] {
        let wrapper = answered_under(&output, refused);
        assert_eq!(wrapper.len(), 1, "{context}: `{refused}` answers once");
        assert!(
            failed(&wrapper[0]),
            "{context}: `{refused}` is refused whole: {:?}",
            wrapper[0]
        );
    }
    let mixed = answered_under(&output, "wmixed");
    assert_eq!(
        mixed.len(),
        2,
        "{context}: each step's wrapper answers on its own"
    );
    assert_eq!(
        rows(&mixed[0]),
        vec![
            (0, "batch".to_owned(), false),
            (1, "ghost".to_owned(), false),
            (2, "echo".to_owned(), false),
            (3, "guarded".to_owned(), false),
            (4, "echo".to_owned(), true),
            (5, "echo".to_owned(), true),
        ],
        "{context}: refused members are rows at their index, in member order, and the wrapper succeeds"
    );
    assert_eq!(rows(&mixed[1]), vec![(0, "echo".to_owned(), true)]);
    // ADR 0117 §8: a provider id repeated within one response is repaired at
    // the boundary: the first call keeps it, the later takes a correlation
    // id, and both are recorded.
    let same = ["native-1", "native-2"].map(|value| {
        let called = served::calls(&output)
            .into_iter()
            .filter(|call| call.tool == "echo" && call.args["value"] == value)
            .collect::<Vec<_>>();
        assert_eq!(called.len(), 1, "{context}: `{value}` is recorded once");
        called[0].provider_call_id.clone()
    });
    assert_eq!(
        same[0], "same",
        "{context}: the first call keeps its provider id"
    );
    assert!(
        same[1].starts_with("lashcall_"),
        "{context}: the repeated provider id is repaired to a correlation id: {same:?}"
    );
    assert_no_member_is_a_call(context, &output);
    world.shutdown().await;

    // Withheld, `batch` is an unknown tool: nothing expands, nothing runs.
    let witness = Arc::new(Witness::default());
    let Some(world) = sugar_world(tier, false, &witness).await else {
        return;
    };
    let context = "batch-disabled";
    let output = world
        .run(
            context,
            served::spec(1024),
            vec![served::response(vec![sugar(
                "w",
                members("echo", &["hidden"]),
            )])],
        )
        .await;
    served::assert_answered(context, &output);
    assert_eq!(
        witness.executed("echo", "hidden"),
        0,
        "{context}: nothing expands"
    );
    let wrapper = answered_under(&output, "w");
    assert_eq!(wrapper.len(), 1);
    assert!(
        failed(&wrapper[0]) && rows(&wrapper[0]).is_empty(),
        "{context}: a disabled `batch` is an ordinary unknown tool: {:?}",
        wrapper[0]
    );
    world.shutdown().await;
}

/// Singleton rounds and expanded batches keep their logical calls and
/// source slots while running in the turn's own round: a member's identity
/// is its wrapper's call id at its original member index.
async fn standard_rounds_and_batches_use_the_run(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = sugar_world(tier, true, &witness).await else {
        return;
    };
    let context = "standard-run-route";
    let output = world
        .run(
            context,
            served::spec(1024),
            vec![
                served::response(vec![echo("scalar", "echo", "scalar")]),
                served::response(vec![
                    echo("native", "echo", "native"),
                    sugar(
                        "batch",
                        serde_json::json!([
                            member("echo", serde_json::json!("first")),
                            member("echo", serde_json::json!(5)),
                            member("guarded", serde_json::json!("denied")),
                            member("echo", serde_json::json!("twin")),
                            member("echo", serde_json::json!("twin")),
                        ]),
                    ),
                ]),
            ],
        )
        .await;
    served::assert_answered(context, &output);
    for value in ["scalar", "native", "first"] {
        assert_eq!(witness.executed("echo", value), 1, "{context}: {value}");
    }
    assert_eq!(witness.executed("echo", "twin"), 2);
    assert_eq!(witness.executed("guarded", "denied"), 0);
    let executions = witness.executions();
    let identities = executions
        .iter()
        .map(|execution| &execution.call_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        identities.len(),
        5,
        "{context}: equal operands remain distinct calls"
    );
    let batch = served::calls(&output)
        .into_iter()
        .find(|call| call.provider_call_id == "batch")
        .expect("the wrapper's call is in the transcript");
    let batch_id = batch.call_id.expect("the wrapper's call id");
    for (value, indices) in [("first", vec![0]), ("twin", vec![3, 4])] {
        let actual = executions
            .iter()
            .filter(|execution| execution.value == value)
            .map(|execution| execution.call_id.clone())
            .collect::<BTreeSet<_>>();
        let expected = indices
            .into_iter()
            .map(|index| batch_id.child(index))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual, expected,
            "{context}: a leaf's identity uses its original member index"
        );
    }
    let wrapper = answered_under(&output, "batch");
    assert_eq!(wrapper.len(), 1);
    assert_eq!(
        rows(&wrapper[0]),
        vec![
            (0, "echo".to_owned(), true),
            (1, "echo".to_owned(), false),
            (2, "guarded".to_owned(), false),
            (3, "echo".to_owned(), true),
            (4, "echo".to_owned(), true),
        ],
        "{context}: preparation failures keep their original source slots"
    );
    assert_no_member_is_a_call(context, &output);
    world.shutdown().await;
}

/// The transcript shows one call and one result per wrapper, under the
/// provider's call id and replay metadata, and no member call.
async fn batch_folds_to_one_transcript_call(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = sugar_world(tier, true, &witness).await else {
        return;
    };
    let context = "batch-transcript";
    let output = world
        .run(
            context,
            served::spec(1024),
            vec![served::response(vec![
                echo("native", "echo", "native"),
                sugar("wrapper", members("echo", &["a", "b", "c"])),
            ])],
        )
        .await;
    served::assert_answered(context, &output);
    assert_eq!(
        served::calls(&output)
            .into_iter()
            .map(|call| (call.provider_call_id, call.tool, call.replay_item))
            .collect::<Vec<_>>(),
        vec![
            ("native".to_owned(), "echo".to_owned(), None),
            (
                "wrapper".to_owned(),
                "batch".to_owned(),
                Some("provider-wrapper".to_owned())
            ),
        ],
        "{context}: the assistant turn keeps the provider's calls and replay metadata"
    );
    let results = served::results(&output);
    assert_eq!(
        results
            .iter()
            .map(|answered| (answered.provider_call_id.clone(), answered.tool.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("native".to_owned(), "echo".to_owned()),
            ("wrapper".to_owned(), "batch".to_owned()),
        ],
        "{context}: one result per call, the wrapper's under its own id"
    );
    let presented = results[1].value();
    let values = presented["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row["result"]["value"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(
        values,
        vec!["a", "b", "c"],
        "{context}: rows in member order"
    );
    assert_no_member_is_a_call(context, &output);
    world.shutdown().await;
}

tiered_laws!(
    tool_group_members_start_before_any_finishes,
    tool_group_reverse_dependency,
    tool_call_limit_admits_the_limit_and_refuses_the_group_past_it,
    tool_call_limit_staged_calls,
    batch_admission_and_identity_contract,
    standard_rounds_and_batches_use_the_run,
    batch_folds_to_one_transcript_call,
);
