//! The before-turn and checkpoint callback laws (ADR 0133 §6, FIG-4921): a
//! callback's session contribution, a graph append, is the session's once
//! with its turn, and no callback runs again
//! once the phase that records its decision committed.
//!
//! A host creates a session on a core and sends it an input; the simulated
//! nodes A and B run its turn with the core's turn services over the
//! production store. The model answers in text. One plugin's callback,
//! before the turn or at its before-completion checkpoint, appends a node
//! carrying a value of its own run. The matrix runs the uncut turn, then cuts it at every
//! labelled write under fail-before, ack-hidden, zombie, abort and
//! commit-then-abort, recovers on the other node, and checks:
//!
//! - **Committed with the turn:** once the turn ended, the session's head
//!   holds exactly one of the callback's appends, the value of one run.
//! - **Once past its decision:** a before-turn decision is recorded with
//!   every phase's checkpoint, so no before-turn run starts after a
//!   `model.start` committed; a before-completion checkpoint's decision
//!   commits with `turn.commit`, so no checkpoint run starts after it.
//! - The turn committed once and ended, and a zombie's writes after its
//!   reap are refused.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

#[path = "support/failure_kind.rs"]
mod failure_kind;

use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use lash_core::facade_support::{SessionGraphFacadeOps as _, SessionNodeProjection as _};
use lash_core::runtime::durable::session::SessionActivation;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_execution_contracts::backend::StoreSet;
use lash_protocol_types::SessionId;
use lash_protocol_types::sync::MutexExt as _;

use dialect::Dialect;
use served::Tier;

const SESSION: &str = "turn-callback-session";
const INPUT: &str = "turn-callback-turn";
/// The plugin whose callback the laws watch.
const PLUGIN: &str = "turn-callback-law";
/// The type of the node the callback's graph append adds.
const NOTE: &str = "turn_callback_law.note";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which callback contributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    BeforeTurn,
    BeforeCompletion,
}

impl Phase {
    /// The commit that records the callback's decision: once it committed,
    /// nothing runs the callback again.
    fn decision(self) -> CommitLabel {
        match self {
            Self::BeforeTurn => CommitLabel::MODEL_START,
            Self::BeforeCompletion => CommitLabel::TURN_COMMIT,
        }
    }
}

/// One run of the callback.
#[derive(Clone, Debug)]
struct Run {
    /// The value it decided.
    value: String,
    /// Whether its decision's commit had committed when it started.
    after_decision: bool,
}

/// Every run of the callback and every model request, across every node:
/// the outside world, which no kill undoes.
struct World {
    phase: Phase,
    runs: Mutex<Vec<Run>>,
    /// How many model requests the deployment was sent.
    asked: std::sync::atomic::AtomicUsize,
    /// The deployment, whose trace a run reads.
    nodes: OnceLock<Weak<SimNodes>>,
}

impl World {
    fn new(phase: Phase) -> Self {
        Self {
            phase,
            runs: Mutex::default(),
            asked: std::sync::atomic::AtomicUsize::new(0),
            nodes: OnceLock::new(),
        }
    }

    /// Whether the decision's commit has committed by now: read from the
    /// trace at once, without yielding.
    fn decided(&self) -> bool {
        let label = self.phase.decision();
        self.nodes
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|nodes| {
                nodes
                    .script()
                    .trace()
                    .iter()
                    .any(|write| write.point.label == label && write.committed())
            })
    }

    /// Record a run, and answer the contributions it decides: its value is
    /// the run's count, so every run decides a value of its own.
    fn enter(&self) -> lash_core::plugin::SessionContributions {
        let after_decision = self.decided();
        let mut runs = self.runs.lock_recover();
        let value = format!("C-{}", runs.len() + 1);
        runs.push(Run {
            value: value.clone(),
            after_decision,
        });
        lash_core::plugin::SessionContributions {
            graph_appends: vec![lash_core::AppendSessionNodesRequest {
                operation_id: format!("turn-callback-law:{value}"),
                nodes: vec![lash_core::SessionAppendNode::plugin(
                    NOTE,
                    serde_json::json!({ "value": value }),
                )],
                requires_ancestor_node_id: None,
            }],
        }
    }
}

/// The plugin whose callback contributes in `world`'s phase.
#[derive(Clone)]
struct CallbackPlugin {
    world: Arc<World>,
}

impl CallbackPlugin {
    pub fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PLUGIN)
    }
}
impl lash::plugins::PluginMetadata for CallbackPlugin {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        Self::declaration()
    }
}

impl lash::plugins::PluginFactory for CallbackPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let mut reg = lash_core::plugin::PluginRegistrar::<CallbackPlugin>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        CallbackPlugin::declare(&mut reg, self.world.phase)?;
        let id = self.id();
        Ok(lash_core::plugin::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(self.as_ref().clone())),
        ))
    }
}

impl lash::plugins::SessionPlugin for CallbackPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }
}
impl CallbackPlugin {
    /// Declare the one callback that contributes in `phase`: the other
    /// phase's callback is not installed, so it never runs.
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
        phase: Phase,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match phase {
            Phase::BeforeTurn => {
                reg.turn()
                    .before(lash::hook_key!("contribute"), move |this, _state| {
                        let world = Arc::clone(&this.world);
                        Ok(Arc::new(move |_| {
                            let session = world.enter();
                            Box::pin(async move {
                                Ok(lash_core::plugin::TurnContributions {
                                    session,
                                    ..Default::default()
                                })
                            })
                        }))
                    })
            }
            Phase::BeforeCompletion => {
                reg.turn()
                    .checkpoint(lash::hook_key!("contribute"), move |this, _state| {
                        let world = Arc::clone(&this.world);
                        Ok(Arc::new(move |ctx| {
                            let session =
                                if ctx.checkpoint == lash_core::CheckpointKind::BeforeCompletion {
                                    world.enter()
                                } else {
                                    Default::default()
                                };
                            Box::pin(async move {
                                Ok(lash_core::plugin::TurnContributions {
                                    session,
                                    ..Default::default()
                                })
                            })
                        }))
                    })
            }
        }
    }
}

/// The model: it answers every request in text, and counts it.
fn model(world: Arc<World>) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("turn-callback-law")
        .requires_streaming(true)
        .complete(move |request: lash_core::llm::types::LlmRequest| {
            world
                .asked
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let answer = served::text(&request, "done");
            async move { Ok(answer) }
        })
        .build()
        .into_handle()
}

/// The values of the callback's appends on `stores`' committed head of
/// `session`, in path order.
async fn committed(stores: &lash::Backend, session: &SessionId) -> Result<Vec<String>, String> {
    let factory = stores.stores().session_store_factory();
    let view = lash_core_execution::store::SessionStore::new(factory, session.clone())
        .map_err(|error| format!("the session's store: {error}"))?;
    let loaded = lash_core_execution::store::load_session_window_state(
        &view,
        lash_core_execution::store::WindowSelector::Current,
    )
    .await
    .map_err(|error| format!("the session's committed state: {error}"))?
    .ok_or("the session has no committed state")?;
    let notes = loaded
        .state
        .session_graph
        .active_path_nodes()
        .into_iter()
        .filter_map(|node| {
            let (kind, body) = node.plugin()?;
            (kind == NOTE).then(|| {
                body.get("value")
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(|| body.to_string(), str::to_owned)
            })
        })
        .collect();
    Ok(notes)
}

/// The scenario in one phase on one dialect, fresh for every matrix cell.
struct Crash {
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Crash {
    fn new(phase: Phase, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            world: Arc::new(World::new(phase)),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
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
                lash::LashCore::standard_builder(backend)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(model(Arc::clone(&self.world)), served::metadata())
                    .plugin(Arc::new(CallbackPlugin {
                        world: Arc::clone(&self.world),
                    }))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("turn-callback-deployment"),
                        lash::persistence::LeaseIncarnationId::new("turn-callback-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// The laws of one run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let trace = nodes.script().trace();
        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits != 1 {
            violations.push(format!("the turn committed {commits} times"));
        }
        match nodes.database().turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }

        let runs = self.world.runs.lock_recover().clone();
        if runs.is_empty() {
            violations.push("the callback never ran".to_owned());
        }
        for run in runs.iter().filter(|run| run.after_decision) {
            violations.push(format!(
                "run {} of the callback started after its decision's {} committed",
                run.value,
                self.world.phase.decision()
            ));
        }
        if self.world.asked.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            violations.push("the model was never asked".to_owned());
        }

        match committed(&self.backend(), &session()).await {
            Err(error) => violations.push(error),
            Ok(notes) => match &notes[..] {
                [note] if runs.iter().any(|run| &run.value == note) => {}
                _ => violations.push(format!(
                    "the head does not hold one run's append: {notes:?}"
                )),
            },
        }

        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        if !violations.is_empty() {
            violations.push(format!("callback runs: {runs:?}"));
        }
        violations
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
            serves: lash::testing::node_serves(&self.core()),
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
            .create(lash::SessionCreation::root(served::spec(8)))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        session
            .send(lash::TurnInput::text(INPUT))
            .await
            .map_err(|error| format!("send the turn's input: {error}"))?;
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

/// The matrix in `phase` over `tier`, cut at every labelled write of its
/// uncut run, which must cut the decision's own commit.
async fn prove(phase: Phase, tier: Tier) {
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
        .run_test(|| Crash::new(phase, dialect, postgres_url.clone()))
        .await;
    eprintln!(
        "turn callback {phase:?} on {dialect:?}: {} cells over {:?}",
        report.cells.len(),
        report.labels()
    );
    report.assert_held();
    assert!(
        report.labels().contains(&phase.decision()),
        "the matrix never cut {}",
        phase.decision()
    );
}

/// A before-turn callback's graph append is the session's once with its
/// turn, whatever write a crash cuts, and no run of the callback starts
/// once a phase recorded its decision (FIG-4921, ADR 0133 §6).
async fn accepted_before_turn_appends_survive_every_cut(tier: Tier) {
    prove(Phase::BeforeTurn, tier).await;
}

/// A before-completion checkpoint's graph append is the session's once
/// with its turn, whatever write a crash cuts, and no run of the checkpoint
/// starts once `turn.commit` committed (FIG-4921).
async fn accepted_checkpoint_appends_survive_every_cut(tier: Tier) {
    prove(Phase::BeforeCompletion, tier).await;
}

tiered_laws!(
    current_thread:
    accepted_before_turn_appends_survive_every_cut,
    accepted_checkpoint_appends_survive_every_cut,
);
