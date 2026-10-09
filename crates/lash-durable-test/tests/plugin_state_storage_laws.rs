//! The storage laws of plugin state per namespace (FIG-5301).
//!
//! A session holds one immutable body per plugin namespace, by content
//! address; its head holds the map of them. A run records the namespaces it
//! changed as rows beside its turn, and its commit promotes them.
//!
//! - **Cost:** a session seeded with a 28 KB namespace that never changes
//!   again runs a turn whose sibling namespace changes at every phase. No
//!   commit of that turn carries the unchanged namespace: no run row names
//!   it, its head component is not rewritten, and none of its bytes is in
//!   any write the turn hands the store, encoded or bound. The uncut run
//!   prints the bytes each phase writes.
//! - **Crash cuts:** the same two turns, as native steps and as code cells
//!   whose quiet points prune their members' records, cut at every labelled
//!   write under fail-before, ack-hidden, zombie, abort and
//!   commit-then-abort. Each turn commits once; every accepted change is in
//!   the committed state exactly once, the reducer's count matching the
//!   calls; the unchanged namespace is intact; and a reducer never runs for
//!   a recorded resolution: raw commands are never replayed.
//! - **Fork:** a fork at an earlier revision copies a `copy` namespace as
//!   of that revision and starts a `reset` one from the plugin's initial
//!   state; later writes in either session leave the other unchanged.
//! - **Frontier:** however many turns publish, a head's namespace frontier
//!   holds every publication's ordinal and only the receipts of the turn
//!   that wrote it (FIG-5393).

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::ToolCall;
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::llm::types::LlmResponse;
use lash_core::runtime::durable::session::SessionActivation;
use lash_core_execution::StoreSet;
use lash_durable::domain::TurnWrite;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DomainWrite, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use dialect::Dialect;
use served::Tier;

const SESSION: &str = "plugin-storage-session";
/// The input of the turn that seeds both namespaces.
const SEED: &str = "plugin-storage-seed";
/// The input of the turn that changes only [`NOTES`].
const STEADY: &str = "plugin-storage-steady";
/// The plugin whose namespace is seeded once and never changes again; a
/// fork copies it.
const MEMORY: &str = "storage-memory";
/// The plugin whose namespace every call changes; a fork resets it.
const NOTES: &str = "storage-notes";
/// [`MEMORY`]'s tool, which sets its key to a [`MEMORY_BYTES`] value.
const REMEMBER: &str = "remember";
/// [`NOTES`]'s tool, which sets `last` and counts itself through [`INCR`].
const NOTE: &str = "note";
/// [`NOTES`]'s reducer: `count` plus the input.
const INCR: &str = "incr";
/// How large [`MEMORY`]'s value is.
const MEMORY_BYTES: usize = 28_000;
/// The repeated unit of a [`MEMORY`] value, distinct from anything else a
/// turn writes.
const UNIT: &str = "<memory-28k-unit>";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// The [`MEMORY_BYTES`] value [`REMEMBER`] sets for `label`.
fn remembered(label: &str) -> String {
    let unit = format!("{UNIT}{label}");
    let mut value = unit.repeat(MEMORY_BYTES / unit.len() + 1);
    value.truncate(MEMORY_BYTES);
    value
}

/// How the turns call the tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Native steps: the seed turn's one step of [`REMEMBER`] and [`NOTE`],
    /// the steady turn's one step of [`NOTE`].
    Step,
    /// Code cells: the seed turn's cell calls [`REMEMBER`] then [`NOTE`],
    /// the steady turn's [`NOTE`] twice, each call's records pruned by the
    /// quiet point after it.
    Cell,
}

impl Shape {
    fn seed(self) -> Vec<LlmResponse> {
        match self {
            Self::Step => vec![served::response(vec![
                served::call(
                    "call-remember",
                    REMEMBER,
                    serde_json::json!({ "label": "seed" }),
                ),
                served::call("call-note-s", NOTE, serde_json::json!({ "label": "s" })),
            ])],
            Self::Cell => vec![served::cell(&format!(
                "await tools.{REMEMBER}({{ label: \"seed\" }});\n\
                 await tools.{NOTE}({{ label: \"s\" }});"
            ))],
        }
    }

    fn steady(self) -> Vec<LlmResponse> {
        match self {
            Self::Step => vec![served::response(vec![served::call(
                "call-note-x",
                NOTE,
                serde_json::json!({ "label": "x" }),
            )])],
            Self::Cell => vec![served::cell(&format!(
                "await tools.{NOTE}({{ label: \"x\" }});\nawait tools.{NOTE}({{ label: \"y\" }});"
            ))],
        }
    }

    /// The committed [`NOTES`] namespace once both turns ended.
    fn notes(self) -> serde_json::Value {
        match self {
            Self::Step => serde_json::json!({ "count": 2, "last": "x" }),
            Self::Cell => serde_json::json!({ "count": 3, "last": "y" }),
        }
    }
}

/// What every node's tool bodies and reducers did: the outside world, which
/// no kill undoes.
#[derive(Default)]
struct World {
    /// How many times a [`NOTE`] body ran, across every execution.
    notes: AtomicUsize,
    /// How many times [`INCR`] ran.
    reductions: AtomicUsize,
    /// Store reads taken between the revert law's model calls.
    overlay_store: Mutex<Option<Arc<dyn DurableStore>>>,
    overlay_bodies: Mutex<Vec<Option<Arc<[u8]>>>>,
}

fn tool_definition(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Changes its plugin's namespace.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
    // A call a kill interrupted runs again at its ordinal.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

/// [`MEMORY`] or [`NOTES`], by `id`.
#[derive(Clone)]
struct StoragePlugin {
    id: &'static str,
    world: Arc<World>,
}

impl StoragePlugin {
    fn tool(&self) -> &'static str {
        if self.id == MEMORY { REMEMBER } else { NOTE }
    }
}

/// [`MEMORY`]'s factory: its namespace is copied by a fork.
#[derive(Clone)]
struct Memory(StoragePlugin);

/// [`NOTES`]'s factory: its namespace is reset by a fork.
#[derive(Clone)]
struct Notes(StoragePlugin);

impl lash::plugins::PluginDefinition for Memory {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(MEMORY)
    }
}

impl lash::plugins::PluginDefinition for Notes {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration {
            state_fork: lash::plugins::StateFork::Reset,
            ..lash::plugins::PluginDeclaration::initial(NOTES)
        }
    }
}

macro_rules! factory {
    ($factory:ident, $id:expr) => {
        impl lash::plugins::PluginFactory for $factory {
            fn id(&self) -> &'static str {
                $id
            }

            fn build(
                &self,
                _: &lash::plugins::PluginSessionContext,
            ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
                Ok(Arc::new(self.0.clone()))
            }
        }
    };
}

factory!(Memory, MEMORY);
factory!(Notes, NOTES);

impl lash::plugins::SessionPlugin for StoragePlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        if self.id == NOTES {
            let world = Arc::clone(&self.world);
            reg.state_reducer(
                INCR,
                Arc::new(move |reduction: lash::plugins::StateReduction<'_>| {
                    world.reductions.fetch_add(1, Ordering::SeqCst);
                    let current = reduction
                        .current
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let step = reduction.input.as_u64().unwrap_or(0);
                    Ok(Some(serde_json::json!(current + step)))
                }),
            )?;
        }
        reg.tools().provider(Arc::new(self.clone()))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for StoragePlugin {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition(self.tool()).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.tool()).then(|| Arc::new(tool_definition(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        if self.id == MEMORY && label == "inspect-overlay" {
            let store = self.world.overlay_store.lock_recover().clone().unwrap();
            let session = SessionId::try_from("revert-overlay-session".to_owned()).unwrap();
            let run = store.turn(&session).await.unwrap().unwrap().run;
            let rows = store.turn_namespaces(&session, &run).await.unwrap();
            let row = rows.into_iter().find(|row| row.plugin == MEMORY).unwrap();
            self.world.overlay_bodies.lock_recover().push(row.body);
            return lash_core::ToolAttemptOutcome::Done {
                result: lash_core::ToolOutcomeDone::ok(serde_json::json!({ "inspected": true })),
                intents: lash_core::ToolIntents::default(),
            };
        }
        let commands = if self.id == MEMORY {
            lash::plugins::StateCommands::new().set("value", remembered(&label).into())
        } else {
            self.world.notes.fetch_add(1, Ordering::SeqCst);
            lash::plugins::StateCommands::new()
                .set("last", label.clone().into())
                .apply("count", INCR, serde_json::json!(1))
        };
        lash_core::ToolAttemptOutcome::Done {
            result: lash_core::ToolOutcomeDone::ok(serde_json::json!({ "done": label }))
                .with_state(commands),
            intents: lash_core::ToolIntents::default(),
        }
    }
}

/// What one owner commit of the steady turn hands the store.
#[derive(Clone, Copy, Debug, Default)]
struct Written {
    commits: usize,
    /// Run rows naming [`MEMORY`].
    memory_rows: usize,
    /// [`MEMORY`] value bytes in run rows and changed head components.
    memory_bytes: usize,
    /// Head components of [`MEMORY`] the commit rewrites.
    memory_components: usize,
    /// Writes holding any part of a [`MEMORY`] value, in any encoding.
    marker_writes: usize,
    /// Every namespace's value bytes in run rows and changed head
    /// components.
    value_bytes: usize,
}

/// Every owner commit after the steady turn was sent, by label.
#[derive(Default)]
struct Ledger {
    steady: AtomicBool,
    phases: Mutex<BTreeMap<&'static str, Written>>,
}

impl Ledger {
    fn observe(&self, label: CommitLabel, writes: &[DomainWrite]) {
        if !self.steady.load(Ordering::SeqCst) {
            return;
        }
        let measured = measure(writes);
        let mut phases = self.phases.lock_recover();
        let phase = phases.entry(label.as_str()).or_default();
        phase.commits += 1;
        phase.memory_rows += measured.memory_rows;
        phase.memory_bytes += measured.memory_bytes;
        phase.memory_components += measured.memory_components;
        phase.marker_writes += measured.marker_writes;
        phase.value_bytes += measured.value_bytes;
    }
}

/// What `writes` carry of any namespace, and of [`MEMORY`]'s.
fn measure(writes: &[DomainWrite]) -> Written {
    let unit = UNIT.as_bytes();
    // A body's bytes as a byte list renders them, debug and JSON.
    let listed = |separator: &str| {
        unit.iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(separator)
    };
    let (spaced, tight) = (listed(", "), listed(","));
    let mut written = Written::default();
    for write in writes {
        let rendered = format!("{write:?}");
        if rendered.contains(UNIT) || rendered.contains(&spaced) || rendered.contains(&tight) {
            written.marker_writes += 1;
        }
        match write {
            DomainWrite::Turn(TurnWrite::Namespaces { namespaces, .. }) => {
                for namespace in namespaces {
                    let bytes = namespace.values.body().map_or(0, <[u8]>::len);
                    written.value_bytes += bytes;
                    if namespace.plugin == MEMORY {
                        written.memory_rows += 1;
                        written.memory_bytes += bytes;
                    }
                }
            }
            DomainWrite::SessionCommit(commit) => {
                let commit = lash_core_store::store::decode_session_commit(&commit.commit_json)
                    .expect("the session commit decodes");
                for (key, component) in &commit.checkpoint.components {
                    let Some(plugin) = key.strip_prefix("plugin_state/") else {
                        continue;
                    };
                    if let lash_core_store::store::HydratedCheckpointComponent::Changed {
                        body,
                        ..
                    } = component
                    {
                        written.value_bytes += body.len();
                        if plugin == MEMORY {
                            written.memory_components += 1;
                            written.memory_bytes += body.len();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    written
}

/// The committed values of `plugin`'s namespace in `session`'s head.
async fn committed(
    backend: &lash::Backend,
    session: &SessionId,
    plugin: &str,
) -> Result<Option<BTreeMap<String, serde_json::Value>>, String> {
    let factory = backend.stores().session_store_factory();
    let view = lash_core_execution::store::SessionStore::new(factory, session.clone())
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
        .and_then(|state| state.plugins.get(plugin))
        .map(|namespace| (*namespace.values).clone()))
}

/// The publication frontier of `plugin`'s namespace in `session`'s head.
async fn committed_frontier(
    backend: &lash::Backend,
    session: &SessionId,
    plugin: &str,
) -> lash_core_execution::tool_run::StateFrontier {
    let factory = backend.stores().session_store_factory();
    let view = lash_core_execution::store::SessionStore::new(factory, session.clone())
        .expect("the session's store");
    let loaded = lash_core_execution::store::load_session_window_state(
        &view,
        lash_core_execution::store::WindowSelector::Current,
    )
    .await
    .expect("the session's committed state")
    .expect("the session has committed state");
    loaded
        .state
        .plugin_state()
        .and_then(|state| state.plugins.get(plugin))
        .map(|namespace| namespace.publication.clone())
        .expect("the namespace is committed")
}

/// The scenario on one shape and one dialect, fresh for every matrix cell.
struct Storage {
    shape: Shape,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    ledger: Arc<Ledger>,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    host: Mutex<Option<lash::DurableSession>>,
    sends: Mutex<Vec<lash::SendHandle>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Storage {
    fn new(shape: Shape, dialect: Dialect, postgres_url: Option<String>) -> Self {
        let scripts = Arc::new(served::Scripts::default());
        scripts.register(SEED, shape.seed());
        scripts.register(STEADY, shape.steady());
        Self {
            shape,
            dialect,
            postgres_url,
            world: Arc::default(),
            ledger: Arc::default(),
            scripts,
            tripwire: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            host: Mutex::default(),
            sends: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core: it serves no node of its own, the simulated
    /// nodes run its sessions' turns.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let clock = self
            .clock
            .lock_recover()
            .clone()
            .expect("the database is built first");
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let builder = match self.shape {
                    Shape::Cell => lash::LashCore::rlm_builder(
                        backend.clone(),
                        served::rlm(&backend, None, sim::workers(&clock)),
                    ),
                    Shape::Step => lash::LashCore::standard_builder(backend.clone()),
                };
                with_plugins(builder, &self.world)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(
                        served::model(Arc::clone(&self.scripts)),
                        served::metadata(),
                    )
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("plugin-storage-deployment"),
                        lash::persistence::LeaseIncarnationId::new("plugin-storage-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    async fn send(&self, input: &str) -> Result<(), String> {
        let session = self
            .host
            .lock_recover()
            .clone()
            .expect("the session is created first");
        let send = session
            .send(lash::TurnInput::text(input))
            .await
            .map_err(|error| format!("send `{input}`: {error}"))?;
        self.sends.lock_recover().push(send);
        Ok(())
    }

    /// Hand the released session to b before a can compete for the next
    /// input. A's runner remains available for recovery after b's claim,
    /// including a cut that prevents that claim from committing.
    async fn handoff(&self, nodes: &SimNodes) {
        nodes.quiesce().await;
        nodes.pause("a");
        let before = nodes.script().trace().len();
        self.send(STEADY).await.expect("the steady turn is sent");
        loop {
            nodes.quiesce().await;
            let claimed_or_cut = nodes.script().trace()[before..].iter().any(|write| {
                write.node.as_ref() == "b"
                    && write.point.label == CommitLabel::CLAIM
                    && (matches!(write.stored, Stored::Committed { effective: true })
                        || write.cut.is_some())
            });
            if claimed_or_cut || !nodes.serving("b") {
                break;
            }
            // These are the scenario's virtual timer steps, not another
            // send or claim attempt: b's existing runner owns the claim.
            assert!(nodes.step().await.is_some(), "b has an armed claim timer");
        }
        nodes.resume("a");
    }

    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let trace = nodes.script().trace();
        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits != 2 {
            violations.push(format!("the two turns committed {commits} times"));
        }
        let backend = self.backend();
        match committed(&backend, &session(), MEMORY).await {
            Ok(Some(values)) if values.get("value") == Some(&remembered("seed").into()) => {}
            Ok(other) => violations.push(format!(
                "the unchanged namespace is not the seeded value: {} keys",
                other.map_or(0, |values| values.len())
            )),
            Err(error) => violations.push(error),
        }
        match committed(&backend, &session(), NOTES).await {
            Ok(Some(values)) if serde_json::json!(values) == self.shape.notes() => {}
            other => violations.push(format!(
                "the changing namespace is {other:?}, not every accepted change once ({})",
                self.shape.notes()
            )),
        }
        let (notes, reductions) = (
            self.world.notes.load(Ordering::SeqCst),
            self.world.reductions.load(Ordering::SeqCst),
        );
        if reductions > notes {
            violations.push(format!(
                "the reducer ran {reductions} times for {notes} note bodies: a recorded \
                 resolution's commands were replayed"
            ));
        }
        let phases = self.ledger.phases.lock_recover().clone();
        let total = phases
            .values()
            .fold(Written::default(), |sum, phase| Written {
                commits: sum.commits + phase.commits,
                memory_rows: sum.memory_rows + phase.memory_rows,
                memory_bytes: sum.memory_bytes + phase.memory_bytes,
                memory_components: sum.memory_components + phase.memory_components,
                marker_writes: sum.marker_writes + phase.marker_writes,
                value_bytes: sum.value_bytes + phase.value_bytes,
            });
        if total.memory_rows + total.memory_bytes + total.memory_components + total.marker_writes
            != 0
        {
            violations.push(format!(
                "the steady turn rewrote the unchanged namespace: {phases:#?}"
            ));
        }
        if total.value_bytes == 0 {
            violations.push("the steady turn wrote no namespace at all".to_owned());
        }
        if cut.is_none() {
            eprintln!(
                "{:?} on {:?}, steady turn, bytes handed to the store per phase:",
                self.shape, self.dialect
            );
            eprintln!("  label            commits  value bytes  memory bytes  memory writes");
            for (label, phase) in &phases {
                eprintln!(
                    "  {label:<16} {:>7}  {:>11}  {:>12}  {:>13}",
                    phase.commits, phase.value_bytes, phase.memory_bytes, phase.marker_writes
                );
            }
        }
        violations
    }
}

/// `builder` with [`MEMORY`] and [`NOTES`] registered.
fn with_plugins(builder: lash::LashCoreBuilder, world: &Arc<World>) -> lash::LashCoreBuilder {
    let plugin = |id| StoragePlugin {
        id,
        world: Arc::clone(world),
    };
    builder
        .plugin(Arc::new(Memory(plugin(MEMORY))))
        .plugin(Arc::new(Notes(plugin(NOTES))))
}

#[async_trait::async_trait]
impl Scenario for Storage {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(served::configured_backend(
            stores,
            sim::settings(),
            Vec::new(),
        ));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
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
        let ledger = Arc::clone(&self.ledger);
        nodes
            .script()
            .observe_commits(Arc::new(move |label, writes| {
                ledger.observe(label, writes);
            }));
        // The host is outside the deployment under test: its sends are
        // uncut.
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                served::spec(64),
            ))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        *self.host.lock_recover() = Some(session);
        self.send(SEED).await?;
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    /// Once the seed turn committed and its session is idle the host sends
    /// the steady turn; done once that one committed too.
    async fn done(&self, nodes: &SimNodes) -> bool {
        let idle = matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        );
        if !idle || !matches!(nodes.database().turn(&session()).await, Ok(None)) {
            return false;
        }
        let commits = nodes
            .script()
            .trace()
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits == 0 {
            return false;
        }
        if !self.ledger.steady.swap(true, Ordering::SeqCst) {
            self.handoff(nodes).await;
            return false;
        }
        commits >= 2
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        self.laws(nodes, cut).await
    }
}

/// Cut both turns of `shape` on `tier` at every labelled write under every
/// fault.
async fn prove(shape: Shape, tier: Tier) {
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
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| Storage::new(shape, dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "{shape:?} on {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    let admissions: Vec<&str> = report
        .baseline
        .iter()
        .filter(|write| write.point.label == CommitLabel::TURN_ADMIT && write.committed())
        .map(|write| write.node.as_ref())
        .collect();
    assert_eq!(
        admissions,
        ["a", "b"],
        "each turn follows its scripted owner"
    );
    let mut expected = vec![
        CommitLabel::MODEL_START,
        CommitLabel::MODEL_DONE,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::TURN_COMMIT,
    ];
    if shape == Shape::Cell {
        expected.push(CommitLabel::CELL_SNAPSHOT);
    }
    for label in expected {
        assert!(
            report.labels().contains(&label),
            "{shape:?}: the matrix never cut {label}"
        );
    }
}

/// A 28 KB namespace that never changes after its seeding turn is written
/// by no commit of a later turn whose sibling namespace changes at every
/// phase, and every accepted change survives any cut exactly once.
async fn an_unchanged_namespace_is_never_rewritten_and_changes_survive_every_cut(tier: Tier) {
    prove(Shape::Step, tier).await;
}

/// The same law over code cells, whose quiet points prune each call's
/// records once it settled: the pruned call's change is still committed
/// once, never by replaying its commands.
async fn a_cells_pruned_calls_keep_their_changes_and_never_rewrite_an_unchanged_namespace(
    tier: Tier,
) {
    prove(Shape::Cell, tier).await;
}

/// FIG-5517: even when both claim loops have just polled the released,
/// empty session, the scripted handoff gives the steady turn to b.
#[tokio::test]
async fn a_released_storage_session_hands_the_steady_turn_to_b_after_empty_polls() {
    let storage = Storage::new(Shape::Cell, Dialect::SqliteMemory, None);
    let clock = SimClock::new();
    let database = storage.database(Arc::clone(&clock)).await;
    let nodes = Arc::new(SimNodes::new(
        database,
        clock,
        lash_durable_test::Script::new(),
        storage.config(),
        storage.activation(),
    ));
    storage.start(&nodes).await.unwrap();
    // Stop at the released seed, before done() sends the next input.
    loop {
        nodes.quiesce().await;
        if matches!(nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle)
            && nodes
                .script()
                .trace()
                .iter()
                .any(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
        {
            break;
        }
        assert!(nodes.step().await.is_some(), "the seed has an armed timer");
    }
    // Both fresh boots poll the idle actor now, then arm equal backoffs.
    // Without a scripted handoff their next contested claims prefer a.
    nodes.restart("a");
    nodes.restart("b");
    nodes.quiesce().await;
    while !storage.done(&nodes).await {
        assert!(
            nodes.step().await.is_some(),
            "the steady turn has an armed timer"
        );
    }
    let admissions: Vec<String> = nodes
        .script()
        .trace()
        .iter()
        .filter(|write| write.point.label == CommitLabel::TURN_ADMIT && write.committed())
        .map(|write| write.node.to_string())
        .collect();
    assert_eq!(
        admissions,
        ["a", "b"],
        "each turn follows its scripted owner"
    );
    assert!(storage.laws(&nodes, None).await.is_empty());
}

/// The session's head revision.
async fn head(world: &served::World, session: &lash::DurableSession) -> u64 {
    let factory = world.backend.session_store_factory();
    lash_core::SessionCommitStore::load_session_head_meta(factory.as_ref(), session.session_id())
        .await
        .expect("the head reads")
        .expect("the session has a head")
        .head_revision
}

/// A fork at an earlier revision copies the `copy` namespace as of that
/// revision and starts the `reset` one empty; later writes in either
/// session are the other's no more.
async fn a_fork_copies_and_resets_namespaces_as_declared_at_its_revision(tier: Tier) {
    let world_state = Arc::new(World::default());
    let Some(world) = served::World::new(tier, |backend| {
        with_plugins(
            lash::LashCore::standard_builder(backend.clone()),
            &world_state,
        )
    })
    .await
    else {
        return;
    };
    let remember = |label: &str| {
        served::call(
            &format!("call-remember-{label}"),
            REMEMBER,
            serde_json::json!({ "label": label }),
        )
    };
    let note = |label: &str| {
        served::call(
            &format!("call-note-{label}"),
            NOTE,
            serde_json::json!({ "label": label }),
        )
    };
    world.script(
        "fork-first",
        vec![served::response(vec![remember("one"), note("one")])],
    );
    world.script(
        "fork-second",
        vec![served::response(vec![remember("two"), note("two")])],
    );
    world.script(
        "fork-parent-after",
        vec![served::response(vec![note("parent")])],
    );
    world.script("fork-child", vec![served::response(vec![note("child")])]);

    let parent = world.session("fork-parent", served::spec(64)).await;
    served::assert_answered("first", &world.send(&parent, "fork-first").await);
    let revision = head(&world, &parent).await;
    served::assert_answered("second", &world.send(&parent, "fork-second").await);

    let child_id = lash::SessionId::try_from("fork-child-session".to_owned()).unwrap();
    world
        .core
        .fork_at(
            parent.session_id(),
            lash_core::Target::Revision(revision),
            lash::ForkRequest {
                session_id: child_id.clone(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: parent.session_id().clone(),
                    source_node_id: None,
                },
                observed_processes: Vec::new(),
            },
        )
        .await
        .expect("the fork is taken");
    let read = |session: SessionId, plugin: &'static str| {
        let backend = world.backend.clone();
        async move {
            committed(&backend, &session, plugin)
                .await
                .expect("the committed state reads")
        }
    };
    let memory = |label: &str| {
        Some(BTreeMap::from([(
            "value".to_owned(),
            remembered(label).into(),
        )]))
    };
    let notes = |count: u64, last: &str| {
        Some(BTreeMap::from([
            ("count".to_owned(), serde_json::json!(count)),
            ("last".to_owned(), serde_json::json!(last)),
        ]))
    };
    let parent_id = parent.session_id().clone();
    assert_eq!(
        read(child_id.clone(), MEMORY).await,
        memory("one"),
        "the fork copies the copy namespace as of its revision"
    );
    assert_eq!(
        read(child_id.clone(), NOTES).await,
        None,
        "the fork starts the reset namespace from the plugin's initial state"
    );

    let child = world
        .core
        .session(child_id.clone())
        .durable()
        .await
        .expect("the fork opens");
    served::assert_answered("child", &world.send(&child, "fork-child").await);
    served::assert_answered(
        "parent after",
        &world.send(&parent, "fork-parent-after").await,
    );
    assert_eq!(read(child_id.clone(), MEMORY).await, memory("one"));
    assert_eq!(read(child_id, NOTES).await, notes(1, "child"));
    assert_eq!(read(parent_id.clone(), MEMORY).await, memory("two"));
    assert_eq!(read(parent_id, NOTES).await, notes(3, "parent"));
    world.shutdown().await;
}

/// FIG-5393: a head's frontier holds every publication's ordinal but only
/// the receipts of the run that wrote it, so it does not grow with the
/// session: a run settles the frontier it begins from.
async fn a_heads_frontier_keeps_only_the_receipts_of_the_run_that_wrote_it(tier: Tier) {
    let world_state = Arc::new(World::default());
    let Some(world) = served::World::new(tier, |backend| {
        with_plugins(
            lash::LashCore::standard_builder(backend.clone()),
            &world_state,
        )
    })
    .await
    else {
        return;
    };
    let session = world.session("frontier", served::spec(64)).await;
    for turn in 1..=3_u64 {
        let label = format!("frontier-{turn}");
        world.script(
            &label,
            vec![served::response(vec![served::call(
                &format!("call-note-{turn}"),
                NOTE,
                serde_json::json!({ "label": label }),
            )])],
        );
        served::assert_answered(&label, &world.send(&session, &label).await);
        let frontier = committed_frontier(&world.backend, session.session_id(), NOTES).await;
        assert_eq!(
            frontier.applied().map(|applied| applied.0),
            Some(turn),
            "the head holds every publication"
        );
        assert_eq!(
            frontier.recent().len(),
            1,
            "the head keeps the receipts of the turn that wrote it, not the session's history"
        );
    }
    world.shutdown().await;
}

/// Returning to the run's base values clears the changed values body,
/// even though publication metadata still differs from the base head.
async fn a_run_reverting_to_base_clears_its_overlay_body(tier: Tier) {
    let state = Arc::new(World::default());
    let Some(world) = served::World::new(tier, |backend| {
        *state.overlay_store.lock_recover() = Some(Arc::clone(backend.durable()));
        with_plugins(lash::LashCore::standard_builder(backend.clone()), &state)
    })
    .await
    else {
        return;
    };
    let remember = |id: &str, label: &str| {
        served::response(vec![served::call(
            id,
            REMEMBER,
            serde_json::json!({ "label": label }),
        )])
    };
    world.script("overlay-seed", vec![remember("seed", "base")]);
    world.script(
        "overlay-revert",
        vec![
            remember("change", "changed"),
            remember("inspect-changed", "inspect-overlay"),
            remember("revert", "base"),
            remember("inspect-base", "inspect-overlay"),
        ],
    );
    let session = world
        .session("revert-overlay-session", served::spec(64))
        .await;
    served::assert_answered("seed", &world.send(&session, "overlay-seed").await);
    served::assert_answered("revert", &world.send(&session, "overlay-revert").await);
    let bodies = state.overlay_bodies.lock_recover().clone();
    assert_eq!(bodies.len(), 2, "both committed overlays were inspected");
    assert!(bodies[0].is_some(), "changed values have a run body");
    assert!(
        bodies[1].is_none(),
        "base values must clear the earlier run body"
    );
    assert_eq!(
        committed(&world.backend, session.session_id(), MEMORY)
            .await
            .unwrap(),
        Some(BTreeMap::from([(
            "value".to_owned(),
            remembered("base").into()
        )]))
    );
    world.shutdown().await;
}

tiered_laws!(
    current_thread:
    a_run_reverting_to_base_clears_its_overlay_body,
    an_unchanged_namespace_is_never_rewritten_and_changes_survive_every_cut,
    a_cells_pruned_calls_keep_their_changes_and_never_rewrite_an_unchanged_namespace,
    a_fork_copies_and_resets_namespaces_as_declared_at_its_revision,
    a_heads_frontier_keeps_only_the_receipts_of_the_run_that_wrote_it,
);
