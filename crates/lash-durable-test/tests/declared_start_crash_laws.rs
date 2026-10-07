//! The declared-start laws a node kill must not break (ADR 0116 §7.3;
//! ported by FIG-5216 from the deleted lash-conformance `declared_start.rs`
//! crash, re-arm, retention and prune laws).
//!
//! A host creates a session on a core and sends it an input whose model
//! calls `spawn_agent`; the core serves no node of its own here, the
//! simulated nodes A and B run what its node runs
//! (`lash::testing::node_activation`): the parent's session turn, each
//! child's `SessionTurn` process and the child session's turn. The matrix
//! runs the uncut deployment, then cuts it at every labelled write under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks the laws:
//!
//! - **One child per call:** each spawn registers one process, which
//!   completes, and whose child session commits its turn once.
//! - **One answer:** every parent request that reads the spawns' results
//!   reads each child's reply once and never a child's handle.
//! - The parent's turn committed once and ended, and a zombie's writes after
//!   its reap are refused.
//!
//! A redrive of a parked call re-pins the same wait, which the launch
//! matrix reaches. A child cannot end before its parent parks on it: the
//! child's registration commits in the fenced transaction that records the
//! call's park (FIG-5223). The retention and prune laws cut one write each,
//! and survey the registry while every node is down.
//!
//! The migrated-tools law (FIG-1293; ported from the deleted
//! lash-conformance `migrated_tools_redrive.rs`) runs the same parent with
//! `cancel_process` of a held host process and a `batch` of two echoes in
//! its spawn's step, cut at its turn's commit: the redrive answers every
//! literal outcome once without asking the model again.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::facade_support::process_child_session_id;
use lash_core_execution::{
    ProcessId, ProcessListFilter, ProcessRecord, ProcessStatus, ProcessStatusFilter,
    ProjectionWatermark, StoreSet,
};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore, LeaseConfig};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
    WriteKind,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use dialect::Dialect;
use served::Tier;

const SESSION: &str = "declared-start-parent";
const PARENT: &str = "declared-start law: spawn the children";
const PARENT_DONE: &str = "declared-start parent done";

fn child_task(index: usize) -> String {
    format!("declared-start child {index}: answer your literal")
}

fn child_reply(index: usize) -> String {
    format!("declared-start child {index} literal")
}

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn parent_actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

fn process_actor(process: &ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).unwrap()
}

fn child_session_actor(process: &ProcessId) -> ActorKey {
    ActorKey::session(process_child_session_id(process).as_str()).unwrap()
}

/// The subagent plugin: one `default` capability, whose children live
/// until their starter ends.
fn subagents() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash::subagents::SubagentsPluginFactory::new(
        Arc::new(lash::subagents::CapabilityRegistry::new().with(Arc::new(
            lash::subagents::StaticCapability::new(
                "default",
                lash_core::facade_support::SessionSpec::inherit(),
            ),
        ))),
        lash_core::lifetime::starter,
    ))
}

/// The parent's script: one step of `width` parallel spawns, then its
/// answer; each child answers its literal.
fn scripts(width: usize) -> Arc<served::Scripts> {
    let scripts = Arc::new(served::Scripts::default());
    scripts.register(
        PARENT,
        vec![
            served::response(
                (0..width)
                    .map(|index| {
                        served::call(
                            &format!("declared-start-spawn-{index}"),
                            "spawn_agent",
                            serde_json::json!({
                                "task": child_task(index),
                                "capability": "default",
                            }),
                        )
                    })
                    .collect(),
            ),
            served::response(vec![lash_core::LlmOutputPart::Text {
                text: PARENT_DONE.to_owned(),
                response_meta: None,
            }]),
        ],
    );
    for index in 0..width {
        scripts.register(
            &child_task(index),
            vec![served::response(vec![lash_core::LlmOutputPart::Text {
                text: child_reply(index),
                response_meta: None,
            }])],
        );
    }
    scripts
}

/// The echo the migrated-tools law's `batch` calls.
const ECHO: &str = "migrated_echo";

/// The values the migrated-tools law's `batch` echoes.
const ECHOED: [&str; 2] = ["migrated-alpha", "migrated-beta"];

fn echo_definition() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{ECHO}"),
        ECHO,
        "Answers the value it is given.",
        object.clone(),
        object,
    )
    .expect("the echo's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], ECHO))
}

/// Counts the echo bodies it runs, across every node.
struct Echo(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl lash_core::ToolProvider for Echo {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![echo_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == ECHO).then(|| Arc::new(echo_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!({ "echo": call.args["value"] })).into()
    }
}

/// The parent's step in the migrated-tools law: `cancel_process` of
/// `target`, one spawn and a `batch` of two echoes, then its answer.
fn migrated_parent(target: &ProcessId) -> Vec<lash_core::LlmResponse> {
    vec![
        served::response(vec![
            served::call(
                "migrated-cancel",
                "cancel_process",
                serde_json::json!({ "process_id": target.to_string() }),
            ),
            served::call(
                "declared-start-spawn-0",
                "spawn_agent",
                serde_json::json!({ "task": child_task(0), "capability": "default" }),
            ),
            served::call(
                "migrated-batch",
                "batch",
                serde_json::json!({
                    "tool_calls": ECHOED
                        .iter()
                        .map(|value| serde_json::json!({ "tool": ECHO, "parameters": { "value": value } }))
                        .collect::<Vec<_>>(),
                }),
            ),
        ]),
        served::response(vec![lash_core::LlmOutputPart::Text {
            text: PARENT_DONE.to_owned(),
            response_meta: None,
        }]),
    ]
}

/// Every process the deployment registered, whatever its status.
async fn registered(backend: &lash::Backend) -> Vec<ProcessRecord> {
    backend
        .process_registry()
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("the registry lists its processes")
}

/// One parent turn spawning `width` children, fresh for every matrix cell.
struct Spawn {
    width: usize,
    dialect: Dialect,
    postgres_url: Option<String>,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    /// The children the run registered, once read: the actors a paused
    /// node must lose besides the parent.
    children: Mutex<BTreeSet<ProcessId>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    /// The migrated-tools law: the parent also cancels a held host process
    /// and runs a `batch`.
    migrated: bool,
    /// The held process the migrated-tools parent cancels, once started.
    target: Mutex<Option<ProcessId>>,
    /// The echo bodies the migrated-tools `batch` ran.
    echoes: Arc<AtomicUsize>,
}

/// What a request reading the migrated-tools step's results misread: an
/// echoed value is read in the `batch` call that asks for it and in its one
/// answer, the child's reply only in its answer, and the cancel's status.
fn literal_violations(request: &str) -> Vec<String> {
    let reply = child_reply(0);
    let mut violations: Vec<String> = ECHOED
        .iter()
        .map(|value| (*value, 2))
        .chain([(reply.as_str(), 1)])
        .filter_map(|(literal, times)| {
            let seen = request.matches(literal).count();
            (seen != times)
                .then(|| format!("a parent request read `{literal}` {seen} times, not {times}"))
        })
        .collect();
    if !request.contains("cancelled") {
        violations.push("a parent request did not read the cancel".to_owned());
    }
    violations
}

impl Spawn {
    fn new(width: usize, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            width,
            dialect,
            postgres_url,
            scripts: scripts(width),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            children: Mutex::default(),
            keep: Mutex::default(),
            migrated: false,
            target: Mutex::default(),
            echoes: Arc::default(),
        }
    }

    /// The migrated-tools law's parent, which spawns one child.
    fn migrated(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            migrated: true,
            ..Self::new(1, dialect, postgres_url)
        }
    }

    fn target(&self) -> Option<ProcessId> {
        self.target.lock_recover().clone()
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core: it serves no node of its own, the simulated
    /// nodes run what its node runs.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let builder = lash::LashCore::standard_builder(backend)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .serve_test_llm_profile(
                        served::model(Arc::clone(&self.scripts)),
                        served::metadata(),
                    )
                    .plugin(subagents());
                let builder = if self.migrated {
                    builder
                        .plugin(Arc::new(
                            lash::process_controls::SessionProcessAdminPluginFactory::new(
                                lash_core::lifetime::starter,
                            ),
                        ))
                        .tools(Arc::new(Echo(Arc::clone(&self.echoes))))
                } else {
                    builder
                };
                builder
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "declared-start-deployment",
                        "declared-start-boot",
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// Create the parent session and send it its input: the host is outside
    /// the deployment under test, so its send is uncut.
    async fn send(&self) -> Result<(), String> {
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(served::spec(16)))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        if self.migrated {
            let target = self.start_target().await?;
            self.scripts.register(PARENT, migrated_parent(&target));
            *self.target.lock_recover() = Some(target);
        }
        session
            .send(lash::TurnInput::text(PARENT))
            .await
            .map(drop)
            .map_err(|error| format!("send the turn's input: {error}"))
    }

    /// Start the held host process the migrated-tools parent cancels,
    /// observed by the parent's session.
    async fn start_target(&self) -> Result<ProcessId, String> {
        let core = self.core();
        let env_ref = core
            .host_artifacts()
            .publish_process_env(
                &lash_core::HostArtifactPin::mint(),
                &lash_core::ProcessExecutionEnvSpec::new(
                    lash_core::AdmittedPluginConfig::default(),
                    lash_core::SessionPolicy::new(
                        lash::TurnBudget::Unbounded,
                        lash::MaxToolCalls::new(16),
                    ),
                ),
            )
            .await
            .map_err(|error| format!("publish the target's environment: {error}"))?;
        let mut request = lash_core::ProcessStartRequest::new(
            lash_core_execution::testing::held_engine_input(serde_json::json!({
                "fixture": "migrated-tools",
            })),
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_env_ref(env_ref);
        request.observers = vec![session()];
        core.processes()
            .start(request, core.effect_host())
            .await
            .map(|started| started.process_id)
            .map_err(|error| format!("start the target: {error}"))
    }

    /// The migrated-tools laws: the target ended cancelled, each echo ran
    /// once, the parent's tool step was asked once, and every request that
    /// reads the step's results reads the cancel, the child's reply and each
    /// echo once. The answer step may be asked again: its response commits
    /// with the turn, so a cut there loses it.
    fn migrated_laws(&self, registered: &[ProcessRecord]) -> Vec<String> {
        let mut violations = Vec::new();
        let Some(target) = self.target() else {
            return vec!["the target never started".to_owned()];
        };
        match registered.iter().find(|process| process.id == target) {
            Some(process) if process.status() == ProcessStatus::Cancelled => {}
            other => violations.push(format!(
                "the target ended {:?}",
                other.map(ProcessRecord::status)
            )),
        }
        let echoes = self.echoes.load(Ordering::SeqCst);
        if echoes != ECHOED.len() {
            violations.push(format!("the batch ran {echoes} echoes"));
        }
        let (answering, asking): (Vec<String>, Vec<String>) = self
            .scripts
            .requests(PARENT)
            .into_iter()
            .partition(|request| request.contains("ToolResult"));
        if asking.len() != 1 {
            violations.push(format!(
                "the parent's tool step was asked {} times",
                asking.len()
            ));
        }
        if answering.is_empty() {
            violations.push("the parent never read its step's results".to_owned());
        }
        for request in &answering {
            violations.extend(literal_violations(request));
        }
        violations
    }
    /// The laws of one run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();
        let commits = |actor: &ActorKey| {
            trace
                .iter()
                .filter(|write| {
                    write.point.label == CommitLabel::TURN_COMMIT
                        && write.committed()
                        && write.actor.as_ref() == Some(actor)
                })
                .count()
        };

        let parent = commits(&parent_actor());
        if parent != 1 {
            violations.push(format!("the parent's turn committed {parent} times"));
        }
        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the parent's turn did not end: {other:?}")),
        }

        // One child per spawn, each completed while it is retained, its
        // session's turn committed once however its process and the parent
        // were cut. A child is every process an owner wrote for, so a
        // pruned one still counts.
        let target = self.target();
        let children: BTreeSet<ProcessId> = trace
            .iter()
            .filter_map(|write| write.actor.as_ref())
            .filter(|actor| actor.kind() == lash_durable::ActorKind::Process)
            .map(|actor| ProcessId::parse(actor.id()).expect("a process actor names a process"))
            .filter(|process| Some(process) != target.as_ref())
            .collect();
        if children.len() != self.width {
            violations.push(format!(
                "{} spawns ran {} children: {children:?}",
                self.width,
                children.len()
            ));
        }
        let processes = registered(&self.backend()).await;
        if self.migrated {
            violations.extend(self.migrated_laws(&processes));
        }
        for child in processes
            .iter()
            .filter(|process| Some(&process.id) != target.as_ref())
        {
            if child.status() != ProcessStatus::Completed {
                violations.push(format!("child {} ended {:?}", child.id, child.status()));
            }
        }
        for child in &children {
            let turns = commits(&child_session_actor(child));
            if turns != 1 {
                violations.push(format!("child {child}'s session committed {turns} turns"));
            }
        }

        // Every parent request that reads the spawns' results reads each
        // child's reply once, and never a child's handle.
        let answered: Vec<String> = self
            .scripts
            .requests(PARENT)
            .into_iter()
            .filter(|request| (0..self.width).any(|index| request.contains(&child_reply(index))))
            .collect();
        if answered.is_empty() {
            violations.push("the parent never read its children's replies".to_owned());
        }
        for request in &answered {
            for index in 0..self.width {
                let seen = request.matches(&child_reply(index)).count();
                if seen != 1 {
                    violations.push(format!(
                        "a parent request read child {index}'s reply {seen} times"
                    ));
                }
            }
            for child in &children {
                if request.contains(child.as_str()) {
                    violations.push(format!("a parent request read child {child}'s handle"));
                }
            }
        }

        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        if !violations.is_empty()
            && let Some(last) = self.scripts.requests(PARENT).last()
        {
            violations.push(format!("the parent's last request: {last}"));
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for Spawn {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        let engines: Vec<Arc<dyn lash_core::ProcessEngine>> = if self.migrated {
            vec![Arc::new(lash_core_execution::testing::HeldProcessEngine)]
        } else {
            Vec::new()
        };
        *self.backend.lock_recover() = Some(served::backend_with(stores, engines));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: LeaseConfig::default(),
            decodes: self.backend().formats().decodes(),
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        self.send().await?;
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        let mut actors = vec![parent_actor()];
        actors.extend(self.target().map(|target| process_actor(&target)));
        for child in self.children.lock_recover().iter() {
            actors.push(process_actor(child));
            actors.push(child_session_actor(child));
        }
        actors
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let children = registered(&self.backend()).await;
        let target = self.target();
        self.children.lock_recover().extend(
            children
                .iter()
                .map(|child| child.id.clone())
                .filter(|child| Some(child) != target.as_ref()),
        );
        matches!(
            nodes.database().actor(&parent_actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        ) && matches!(nodes.database().turn(&session()).await, Ok(None))
            && children.iter().all(ProcessRecord::is_terminal)
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

/// The dialect and server of `tier`, or `None` for a PostgreSQL leg the
/// run was handed no server for.
fn dialect_of(tier: Tier) -> Option<(Dialect, Option<String>)> {
    match tier {
        Tier::SqliteMemory => Some((Dialect::SqliteMemory, None)),
        Tier::SqliteFile => Some((Dialect::SqliteFile, None)),
        Tier::Postgres => match dialect::postgres_url() {
            Some(url) => Some((Dialect::Postgres, Some(url))),
            None => {
                eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
                None
            }
        },
    }
}

/// Where the parent launches its child: the model step that declares the
/// spawn, then the call's park and settle.
const LAUNCH_LABELS: &[CommitLabel] = &[CommitLabel::MODEL_DONE, CommitLabel::ROUND_OUTCOME];

/// Where the parent goes on once its child ended: the result's presentation
/// with the next model step, and the turn's commit.
const SETTLED_LABELS: &[CommitLabel] = &[
    CommitLabel::ROUND_PRESENT_MODEL_START,
    CommitLabel::TURN_COMMIT,
];

/// The children's labels: each child's process passes, its terminal and its
/// cascade.
const CHILD_LABELS: &[CommitLabel] = &[
    CommitLabel::PROCESS_ADVANCE,
    CommitLabel::PROCESS_TERMINAL,
    CommitLabel::CASCADE_BATCH,
];

/// Where a batch of spawns has settled: the presentation of both children's
/// answers with the parent's next model step. The batch's park and settle
/// commits (`round.outcome`) are not cut here: the lifecycle commits them
/// as its members finish, so their number varies from run to run and a cut
/// of the uncut run's last one need not recur. The width-1 launch matrix
/// cuts every `round.outcome`.
const BATCH_LABELS: &[CommitLabel] = &[CommitLabel::ROUND_PRESENT_MODEL_START];

/// Cut a parent spawning `width` children at every write under `labels` of
/// its uncut run. A node's lease writes are the runner's, which its own
/// matrices cut.
async fn prove(width: usize, labels: &[CommitLabel], tier: Tier) {
    let Some((dialect, postgres_url)) = dialect_of(tier) else {
        return;
    };
    let report = Matrix::new()
        .across_nodes()
        .labels(labels)
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run(|| Spawn::new(width, dialect, postgres_url.clone()))
        .await;
    let cut: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "width {width} on {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        cut.len(),
        cut.join(", "),
    );
    report.assert_held();
    for label in labels {
        assert!(
            report.labels().contains(label),
            "width {width}: the matrix never cut {label}"
        );
    }
}

/// A parent cut at any write of a `spawn_agent` call redrives to one
/// process, one child session turn and one answer: the start's key gives a
/// rerun the child it registered, and a redrive re-pins the same wait.
async fn declared_start_crash_at_every_launch_boundary(tier: Tier) {
    prove(1, LAUNCH_LABELS, tier).await;
}

/// A parent cut after its child ended, before its turn committed, presents
/// the child's one answer and commits once.
async fn declared_start_crash_after_the_child_ended(tier: Tier) {
    prove(1, SETTLED_LABELS, tier).await;
}

/// A child cut at any write of its process redrives to its one session
/// turn, and its parent's call answers it once.
async fn declared_start_child_crash_at_every_process_boundary(tier: Tier) {
    prove(1, CHILD_LABELS, tier).await;
}

/// A parent cut once a batch of spawns settled is redriven onto the same
/// children: each ran once and answers its own call once.
async fn batch_of_spawns_crash_redriven(tier: Tier) {
    prove(2, BATCH_LABELS, tier).await;
}

// --- one cut, surveyed while every node is down ------------------------------

/// Run `spawn` with `script`'s one cut: node A runs the deployment until the
/// cut kills it, `survey` reads the registry with every node down, then B
/// recovers and runs the deployment to its end. Answers the survey's
/// finding, the laws' violations and the scenario, which holds its database.
async fn run_cut<T>(
    spawn: Spawn,
    script: Script,
    survey: impl AsyncFnOnce(&lash::Backend) -> T,
) -> (T, Vec<String>, Spawn) {
    let clock = SimClock::new();
    let database = spawn.database(Arc::clone(&clock)).await;
    let nodes = Arc::new(SimNodes::new(
        database,
        Arc::clone(&clock),
        script,
        spawn.config(),
        spawn.activation(),
    ));
    spawn
        .send()
        .await
        .expect("the host sends the parent's input");
    nodes.start("a");
    nodes.quiesce().await;
    let horizon = clock.logical_ms() + 600_000;
    while nodes.script().cuts().is_empty() {
        assert!(
            clock.logical_ms() < horizon && nodes.step().await.is_some(),
            "the cut never fired on A: {}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    let found = survey(&spawn.backend()).await;
    nodes.start("b");
    while !spawn.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon,
            "not done after ten minutes of virtual time: {}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "stalled: {}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    let violations = spawn.laws(&nodes, None).await;
    // The scenario holds the database: a survey after the run reads it.
    (found, violations, spawn)
}

/// The processes a prune would take now, whatever their age.
async fn prunable(backend: &lash::Backend) -> Vec<ProcessId> {
    backend
        .process_registry()
        .prunable_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("survey the prunable processes")
}

/// A child its call has not consumed is held: it ended, and nothing but the
/// call's hold keeps it, yet a prune leaves it. Once the call consumed its
/// terminal and released the hold, a prune may take it.
///
/// The child's terminal is the cut: A commits it and dies before the
/// parent's parked call consumes it, so the survey reads a terminal child
/// whose hold stands.
async fn declared_start_retention_hold_blocks_prune_until_consumed(tier: Tier) {
    let Some((dialect, postgres_url)) = dialect_of(tier) else {
        return;
    };
    let spawn = Spawn::new(1, dialect, postgres_url);
    let script = Script::new();
    script.cut_on(
        "a",
        CommitLabel::PROCESS_TERMINAL,
        1,
        Fault::CommitThenAbort,
    );
    let ((child, held), violations, spawn) = run_cut(spawn, script, async |backend| {
        let children = registered(backend).await;
        assert_eq!(children.len(), 1, "one child: {children:#?}");
        assert!(
            children[0].is_terminal(),
            "the cut left the child ended: {children:#?}"
        );
        (children[0].id.clone(), prunable(backend).await)
    })
    .await;
    assert!(violations.is_empty(), "{violations:#?}");
    assert!(
        !held.contains(&child),
        "a terminal child whose hold stands is not prunable: {held:?}"
    );
    let released = prunable(&spawn.backend()).await;
    assert!(
        released.contains(&child),
        "once the call consumed it, the child is prunable: {released:?}"
    );
}

/// A parked call's child can be pruned once the call released its hold,
/// before the call settled, and a redrive still answers the child's
/// recorded terminal: the wait's committed winner answers it, and the
/// redrive neither registers nor runs the child again.
///
/// The parent's settle of the resolved call is the cut: its hold is
/// released, A dies before the settle commits, and the survey prunes the
/// child.
async fn declared_start_prune_after_hold_release_before_settlement_replays_terminal(tier: Tier) {
    let Some((dialect, postgres_url)) = dialect_of(tier) else {
        return;
    };
    let spawn = Spawn::new(1, dialect, postgres_url);
    let script = Script::new();
    script.cut_on("a", CommitLabel::ROUND_OUTCOME, 2, Fault::Abort);
    let ((child, pruned, after), violations, _) = run_cut(spawn, script, async |backend| {
        let children = registered(backend).await;
        assert_eq!(children.len(), 1, "one child: {children:#?}");
        assert!(
            children[0].is_terminal(),
            "the call released its hold on an ended child: {children:#?}"
        );
        let registry = backend.process_registry();
        let report = registry
            .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
            .await
            .expect("prune the ended processes");
        let after = registry.get_process(&children[0].id).await;
        (children[0].id.clone(), report.pruned_processes, after)
    })
    .await;
    assert!(
        pruned >= 1 && !matches!(after, Ok(Some(_))),
        "the prune took the released child {child}: {pruned} pruned, then read {after:?}"
    );
    assert!(violations.is_empty(), "{violations:#?}");
}

/// The migrated public tools and a `batch` settle once and redrive to
/// their literal outcomes (FIG-1293): one step calls `cancel_process`,
/// `spawn_agent` and `batch`, and a cut at the turn's commit, under every
/// fault, redrives to the cancel, the child's reply and both echoes, read
/// once each, without asking the model again.
async fn public_migrated_tools_redrive_to_literal_outcomes(tier: Tier) {
    let Some((dialect, postgres_url)) = dialect_of(tier) else {
        return;
    };
    let report = Matrix::new()
        .across_nodes()
        .labels(&[CommitLabel::TURN_COMMIT])
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run(|| Spawn::migrated(dialect, postgres_url.clone()))
        .await;
    eprintln!(
        "migrated tools on {dialect:?}: {} cells",
        report.cells.len()
    );
    report.assert_held();
    assert!(
        report.labels().contains(&CommitLabel::TURN_COMMIT),
        "the matrix never cut the turn's commit"
    );
}

tiered_laws!(
    current_thread:
    public_migrated_tools_redrive_to_literal_outcomes,
    declared_start_crash_at_every_launch_boundary,
    declared_start_crash_after_the_child_ended,
    declared_start_child_crash_at_every_process_boundary,
    batch_of_spawns_crash_redriven,
    declared_start_retention_hold_blocks_prune_until_consumed,
    declared_start_prune_after_hold_release_before_settlement_replays_terminal,
);
