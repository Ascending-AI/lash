//! The after-turn laws (FIG-5283): a plugin's after-turn callback runs on
//! the durable turn path once the turn's outcome is known, and what it
//! decides commits in the turn's own `turn.commit`.
//!
//! A host creates a session on a core and sends it an input; the simulated
//! nodes A and B run its turn with the core's turn services over the
//! production store. The turn's model answers in text. A plugin's
//! after-turn callback returns a record, a graph append fenced to the leaf
//! of the head it reads, and a state change, each carrying a value of its
//! own run. The matrix runs the uncut turn,
//! then cuts it at every labelled write under fail-before, ack-hidden,
//! zombie, abort and commit-then-abort, recovers on the other node, and
//! checks:
//!
//! - **Committed with the turn:** once the turn ended, the session's head
//!   holds exactly one of the callback's records and one of its graph
//!   appends, and its plugin state holds the callback's value: all three the
//!   value of one run, the run whose decision `turn.commit` recorded.
//! - **Once past its decision:** no run of the callback starts after a
//!   `turn.commit` committed; a cut before it runs the callback again over
//!   the same finished turn.
//! - The turn committed once and ended, and a zombie's writes after its
//!   reap are refused.
//!
//! **Compaction recovery:** the standard compaction plugin's after-turn
//! callback, on a core serving its own node, appends its pending recovery
//! record with the commit of a turn that stopped on a context overflow.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use lash_core::facade_support::{SessionGraphFacadeOps as _, SessionNodeProjection as _};
use lash_core::runtime::durable::session::SessionActivation;
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

const SESSION: &str = "after-turn-session";
const INPUT: &str = "after-turn-turn";
/// The plugin whose after-turn callback the laws watch.
const PLUGIN: &str = "after-turn-law";
/// The type of the callback's record.
const RECORD: &str = "after_turn_law.record";
/// The type of the node the callback's graph append adds.
const NOTE: &str = "after_turn_law.note";
/// The key the callback sets in its plugin's namespace.
const KEY: &str = "value";
/// The type of the standard compaction plugin's recovery record.
const OVERFLOW_RECOVERY: &str = "standard_compaction.overflow_recovery";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// One run of the callback.
#[derive(Clone, Debug)]
struct Run {
    /// The value it decided.
    value: String,
    /// The outcome it was handed.
    outcome: String,
    /// Whether a `turn.commit` had committed when it started.
    after_commit: bool,
}

/// Every run of the callback, across every node: the outside world, which
/// no kill undoes.
#[derive(Default)]
struct World {
    runs: Mutex<Vec<Run>>,
    /// The deployment, whose trace a run reads.
    nodes: OnceLock<Weak<SimNodes>>,
}

impl World {
    fn runs(&self) -> Vec<Run> {
        self.runs.lock_recover().clone()
    }

    /// Whether a `turn.commit` has committed by now: read from the trace at
    /// once, without yielding.
    fn turn_committed(&self) -> bool {
        self.nodes
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|nodes| {
                nodes
                    .script()
                    .trace()
                    .iter()
                    .any(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            })
    }

    /// Record a run handed `outcome`, and answer the value it decides: the
    /// run's count, so every run decides a value of its own.
    fn enter(&self, outcome: String) -> String {
        let after_commit = self.turn_committed();
        let mut runs = self.runs.lock_recover();
        let value = format!("H-{}", runs.len() + 1);
        runs.push(Run {
            value: value.clone(),
            outcome,
            after_commit,
        });
        value
    }
}

/// The plugin whose after-turn callback records, appends and sets its run's
/// value.
#[derive(Clone)]
struct AfterTurnPlugin {
    world: Arc<World>,
}

impl lash::plugins::PluginDefinition for AfterTurnPlugin {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PLUGIN)
    }
}

impl lash::plugins::PluginFactory for AfterTurnPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for AfterTurnPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        let world = Arc::clone(&self.world);
        reg.turn().after(
            lash::hook_key!("mark"),
            Arc::new(move |ctx| {
                let world = Arc::clone(&world);
                Box::pin(async move {
                    let outcome = match &ctx.turn.outcome {
                        lash_core::facade_support::TurnOutcome::Finished(_) => "finished",
                        _ => "other",
                    };
                    let value = world.enter(outcome.to_owned());
                    // Fenced to the leaf it read, as the workbench's note is:
                    // the head the turn started from.
                    let snapshot = ctx.sessions.snapshot_current().await?;
                    let leaf = lash::persistence::SessionReadView::from_snapshot(&snapshot)
                        .session_graph()
                        .leaf_node_id
                        .clone();
                    Ok(lash::plugins::AfterTurnContributions {
                        records: vec![lash::plugins::PluginRecordContribution {
                            plugin_type: RECORD.to_owned(),
                            body: serde_json::json!({ "value": value }),
                        }],
                        state: lash::plugins::StateCommands::new().set(KEY, value.clone().into()),
                        session: lash::plugins::SessionContributions {
                            graph_appends: vec![lash::plugins::AppendSessionNodesRequest {
                                operation_id: format!("after-turn-law:{value}"),
                                nodes: vec![lash::plugins::SessionAppendNode::plugin(
                                    NOTE,
                                    serde_json::json!({ "value": value }),
                                )],
                                requires_ancestor_node_id: leaf,
                            }],
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                })
            }),
        )
    }
}

/// What the session's committed head holds of the callback.
#[derive(Debug, Default)]
struct Committed {
    records: Vec<String>,
    notes: Vec<String>,
    state: Option<serde_json::Value>,
}

/// The values of every plugin node of `plugin_type` on `stores`' committed
/// head of `session`, in path order, and `plugin`'s `key`.
async fn committed(
    stores: &lash::Backend,
    session: &SessionId,
    plugin_types: [&str; 2],
) -> Result<(Vec<String>, Vec<String>, Option<serde_json::Value>), String> {
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
    let values = |plugin_type: &str| {
        loaded
            .state
            .session_graph
            .active_path_nodes()
            .into_iter()
            .filter_map(|node| {
                let (kind, body) = node.plugin()?;
                (kind == plugin_type).then(|| {
                    body.get("value")
                        .and_then(serde_json::Value::as_str)
                        .map_or_else(|| body.to_string(), str::to_owned)
                })
            })
            .collect::<Vec<_>>()
    };
    let state = loaded
        .state
        .plugin_state()
        .and_then(|state| state.plugins.get(PLUGIN))
        .and_then(|namespace| namespace.values.get(KEY))
        .cloned();
    Ok((values(plugin_types[0]), values(plugin_types[1]), state))
}

/// The scenario on one dialect, fresh for every matrix cell.
struct Crash {
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Crash {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            world: Arc::default(),
            scripts: Arc::default(),
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
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .serve_test_llm_profile(
                        served::model(Arc::clone(&self.scripts)),
                        served::metadata(),
                    )
                    .plugin(Arc::new(AfterTurnPlugin {
                        world: Arc::clone(&self.world),
                    }))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "after-turn-deployment",
                        "after-turn-boot",
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

        let runs = self.world.runs();
        if runs.is_empty() {
            violations.push("the after-turn callback never ran".to_owned());
        }
        for run in &runs {
            if run.after_commit {
                violations.push(format!(
                    "run {} of the callback started after its decision committed",
                    run.value
                ));
            }
            if run.outcome != "finished" {
                violations.push(format!(
                    "run {} was handed the outcome `{}`, not the finished turn's",
                    run.value, run.outcome
                ));
            }
        }

        match committed(&self.backend(), &session(), [RECORD, NOTE]).await {
            Err(error) => violations.push(error),
            Ok((records, notes, state)) => {
                let committed = Committed {
                    records,
                    notes,
                    state,
                };
                match (
                    &committed.records[..],
                    &committed.notes[..],
                    &committed.state,
                ) {
                    ([record], [note], Some(serde_json::Value::String(state)))
                        if record == note && note == state => {}
                    _ => violations.push(format!(
                        "the head does not hold one run's record, append and state: \
                         {committed:?}"
                    )),
                }
            }
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

/// The matrix over `tier`: the uncut turn alone with `cut: false`, and
/// otherwise cut at every labelled write of its uncut run, which must cut
/// the writes around `turn.commit`.
async fn prove(tier: Tier, cut: bool) {
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
    let matrix = if cut {
        Matrix::new()
    } else {
        Matrix::new().labels(&[])
    };
    let report = matrix
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run(|| Crash::new(dialect, postgres_url.clone()))
        .await;
    eprintln!(
        "after-turn on {dialect:?}: {} cells over {:?}",
        report.cells.len(),
        report.labels()
    );
    report.assert_held();
    if cut {
        for label in [CommitLabel::MODEL_START, CommitLabel::TURN_COMMIT] {
            assert!(
                report.labels().contains(&label),
                "the matrix never cut {label}"
            );
        }
    }
}

/// An after-turn callback's record, graph append and state change are the
/// session's once its turn commits: all three the value of the run whose
/// decision the commit recorded (FIG-5283).
async fn an_after_turn_callbacks_record_and_state_change_commit_with_its_turn(tier: Tier) {
    prove(tier, false).await;
}

/// Cut at every labelled write of the turn, those around `turn.commit`
/// among them (the model call's start before it, and the commit itself),
/// under every fault: the callback's effects are the session's exactly
/// once, the value of one run, and no run starts after its decision
/// committed (FIG-5283).
async fn an_after_turn_callbacks_effects_apply_once_however_its_turn_is_cut(tier: Tier) {
    prove(tier, true).await;
}

/// The standard compaction plugin's after-turn callback appends its pending
/// recovery record with the commit of a turn that stopped on a context
/// overflow, on a core serving its own node (FIG-2950, FIG-5283).
async fn compaction_recovery_records_a_context_overflow_with_its_turn(tier: Tier) {
    let Some(world) = served::World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone()).plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
    })
    .await
    else {
        return;
    };
    let name = "after-turn-overflow";
    let output = world
        .run(
            name,
            served::spec(8),
            vec![lash_core::llm::types::LlmResponse {
                terminal_reason: lash_core::LlmTerminalReason::ContextOverflow,
                terminal_diagnostic: Some("context window exceeded".to_owned()),
                ..Default::default()
            }],
        )
        .await;
    assert!(
        matches!(
            output.result.outcome,
            lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::ContextOverflow
            )
        ),
        "the turn stops on its context overflow: {:?}",
        output.result.outcome
    );
    let session = SessionId::try_from(name.to_owned()).unwrap();
    let (recoveries, _, _) = committed(&world.backend, &session, [OVERFLOW_RECOVERY, NOTE])
        .await
        .expect("the session's committed head");
    assert_eq!(
        recoveries.len(),
        1,
        "the overflow turn's commit holds one pending recovery record: {recoveries:?}"
    );
    assert!(
        recoveries[0].contains("pending"),
        "the recovery record is the pending marker: {recoveries:?}"
    );
    world.shutdown().await;
}

tiered_laws!(
    current_thread:
    an_after_turn_callbacks_record_and_state_change_commit_with_its_turn,
    an_after_turn_callbacks_effects_apply_once_however_its_turn_is_cut,
    compaction_recovery_records_a_context_overflow_with_its_turn,
);
