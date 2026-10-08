//! The laws of a process body's tool calls a node kill must not break
//! (FIG-4546, ADR 0117; ported by FIG-5216 from the deleted lash-conformance
//! `tool_batch_parallelism/limit.rs` worker-kill law and
//! `tool_call_identity/admission.rs` replay law).
//!
//! A host sends an RLM turn whose cell starts a Lashlang process and awaits
//! it. The process races `max_tool_calls` probe calls, whose loser never
//! answers, then asks for one more. The simulated nodes A and B run what
//! the core's node runs (`lash::testing::node_activation`). The matrix runs
//! the uncut deployment, then cuts every write of the process's passes,
//! step outcomes and terminal under fail-before, ack-hidden, zombie, abort
//! and commit-then-abort, recovers on the other node, and checks:
//!
//! - **Holding:** the call past the limit runs in no execution, and the
//!   process fails with the typed refusal counting the held race once,
//!   however many executions formed it.
//! - **Identity:** every body entry of one call sees one call id, and two
//!   calls are two ids. The probe is `Repeatable`, so a call a kill
//!   interrupted runs again under its id rather than settling interrupted.
//! - The turn committed once and ended, and a zombie's writes after its
//!   reap are refused.
//!
//! An aggregate's sleep is a durable wait, not a step body (FIG-5441): a
//! process that awaits `Promise.all([call, sleep(5 min)])`, longer than any
//! step bound, parks on the sleep's deadline once its call settles, its
//! actor released as `waiting`, and wakes there after node A is killed.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/matrix.rs"]
mod matrix;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use matrix::MatrixTestExt as _;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::{ToolCall, ToolOutcome};
use lash_core_execution::StoreSet;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
    WriteKind,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use dialect::Dialect;
use served::Tier;

const SESSION: &str = "process-holding-session";
const INPUT: &str = "process-holding-turn";
const PROBE: &str = "holding_probe";

/// The `max_tool_calls` the session records: the race's width.
const LIMIT: usize = 2;

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Every body entry of every probe call, across every node: the outside
/// world, which no kill undoes.
#[derive(Default)]
struct World {
    entries: Mutex<BTreeMap<String, Vec<lash_core::ToolCallId>>>,
}

fn probe_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{PROBE}"),
        PROBE,
        "Records each body entry's call id; a held call never answers.",
        object.clone(),
        object,
    )
    .expect("the probe's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], PROBE))
    // A call a kill interrupted runs again at its ordinal: the raced calls
    // stay in flight across the kill, so the race is still held when the
    // call past the limit asks.
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
        if call.args["hold"].as_bool().unwrap_or_default() {
            std::future::pending::<()>().await;
        }
        ToolOutcome::ok(serde_json::json!({ "label": label })).into()
    }
}

/// The turn's one cell: define the holding body, start it, and await it.
fn cell() -> lash_core::llm::types::LlmResponse {
    let body = format!(
        "const body = async () => {{\n  \
           const raced = await Promise.race([tools.{PROBE}({{ label: \"winner\" }}), \
           tools.{PROBE}({{ label: \"loser\", hold: true }})]);\n  \
           await tools.{PROBE}({{ label: \"refused\" }});\n  \
           return raced;\n}};"
    );
    served::cell(&format!(
        "const body = await processes.create({{ dialect: \"typescript\", source: `{body}` }});\n\
         const held = await processes.start({{ definition: body }});\n\
         finish(await held);"
    ))
}

/// The scenario, fresh for every matrix cell.
struct Holding {
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    /// The virtual clock the database was built on, which the core's VM
    /// worker calls hold.
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    processes: Mutex<BTreeSet<lash_core::ProcessId>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Holding {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self::running(dialect, postgres_url, cell())
    }

    /// The deployment whose turn runs `cell`.
    fn running(
        dialect: Dialect,
        postgres_url: Option<String>,
        cell: lash_core::llm::types::LlmResponse,
    ) -> Self {
        let scripts = Arc::new(served::Scripts::default());
        scripts.register(INPUT, vec![cell]);
        Self {
            dialect,
            postgres_url,
            world: Arc::default(),
            scripts,
            tripwire: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            processes: Mutex::default(),
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
    /// nodes run what its node runs.
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
                lash::LashCore::rlm_builder(backend.clone(), served::rlm(&backend, None, sim::workers(&clock)))
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .serve_test_llm_profile(
                        served::model(Arc::clone(&self.scripts)),
                        served::metadata(),
                    )
                    // Detached: a cell's operation is admitted `Once` until
                    // FIG-5174, so a kill inside the cell's start answers it
                    // interrupted and ends the turn; the process the start
                    // registered runs on regardless.
                    .plugin(Arc::new(
                        lash::process_controls::SessionProcessAdminPluginFactory::new(|_| {
                            lash_core::Lifetime::Detached
                        }),
                    ))
                    .tools(Arc::new(Probe {
                        world: Arc::clone(&self.world),
                    }))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "process-holding-deployment",
                        "process-holding-boot",
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    async fn registered(&self) -> Vec<lash_core::ProcessRecord> {
        self.backend()
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await
            .expect("the registry lists its processes")
    }

    /// The laws of one run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let trace = nodes.script().trace();
        let commits = trace
            .iter()
            .filter(|write| {
                write.point.label == CommitLabel::TURN_COMMIT
                    && write.committed()
                    && write.actor.as_ref() == Some(&actor())
            })
            .count();
        if commits != 1 {
            violations.push(format!("the turn committed {commits} times"));
        }
        match nodes.database().turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }

        // Every call ran under one call id, and the two raced calls are
        // two ids; the call past the limit ran in no execution.
        let entries = self.world.entries.lock_recover().clone();
        let mut ids = BTreeSet::new();
        for label in ["winner", "loser"] {
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
        if let Some(seen) = entries.get("refused") {
            violations.push(format!("the call past the limit ran {} times", seen.len()));
        }

        // The process failed with the refusal, the race counted once.
        let exceeded = lash_core::ToolCallLimitExceeded {
            scope: lash_core::ToolCallLimitScope::Process,
            limit: lash_core::MaxToolCalls::new(LIMIT),
            counted: LIMIT,
            requested: 1,
        };
        let processes = self.registered().await;
        if processes.len() != 1 {
            violations.push(format!("{} processes registered", processes.len()));
        }
        for process in &processes {
            let raw = match process.outcome() {
                Some(lash_core::ProcessAwaitOutput::Settled { output }) => match output.outcome {
                    lash_core::ToolCallOutcome::Failure(failure) => {
                        failure.raw.map(|raw| raw.to_json_value())
                    }
                    other => {
                        violations.push(format!("the process ended {other:?}"));
                        continue;
                    }
                },
                other => {
                    violations.push(format!("the process ended {other:?}"));
                    continue;
                }
            };
            if raw != Some(serde_json::json!({ "tool_call_limit": exceeded })) {
                violations.push(format!("the process's refusal was {raw:?}"));
            }
        }

        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        if !violations.is_empty() {
            violations.push(format!("body entries: {entries:?}"));
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for Holding {
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
            decodes: lash::testing::node_activation(&self.core(), Arc::new(lash_durable::NoProbe))
                .expect("the core's node activation")
                .0
                .formats()
                .decodes(),
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // The host is outside the deployment under test: its send is uncut.
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(served::spec(LIMIT)))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        session
            .send(lash::TurnInput::text(INPUT))
            .await
            .map(drop)
            .map_err(|error| format!("send the turn's input: {error}"))?;
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        let mut actors = vec![actor()];
        actors.extend(
            self.processes
                .lock_recover()
                .iter()
                .map(|process| ActorKey::process(process.as_str()).unwrap()),
        );
        actors
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let processes = self.registered().await;
        self.processes
            .lock_recover()
            .extend(processes.iter().map(|process| process.id.clone()));
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        ) && matches!(nodes.database().turn(&session()).await, Ok(None))
            && processes.iter().all(lash_core::ProcessRecord::is_terminal)
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

/// A process that holds `max_tool_calls` calls is refused one more while it
/// holds them, its held race counted once however many executions formed
/// it, and each call a process body issues keeps one name across a replay:
/// cut at every write of the process's passes, step outcomes and terminal.
async fn tool_call_limit_counts_what_a_process_holds_across_a_worker_kill(tier: Tier) {
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
    let labels = [
        CommitLabel::PROCESS_ADVANCE,
        CommitLabel::STEP_OUTCOME,
        CommitLabel::PROCESS_TERMINAL,
    ];
    let report = Matrix::new()
        .across_nodes()
        .labels(&labels)
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| Holding::new(dialect, postgres_url.clone()))
        .await;
    let cut: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "{dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        cut.len(),
        cut.join(", ")
    );
    report.assert_held();
    for label in labels {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// How long the aggregate's sleep lasts, in virtual milliseconds: past the
/// lashlang engine's two-minute step bound and past the session's idle
/// eviction.
const AGGREGATE_SLEEP_MS: u64 = 300_000;

/// The turn's one cell: start a detached process whose body awaits a call
/// and a sleep together, and end the turn.
fn sleeping_cell() -> lash_core::llm::types::LlmResponse {
    let body = format!(
        "const body = async () => {{\n  \
           const both = await Promise.all([tools.{PROBE}({{ label: \"quick\" }}), \
           sleep({AGGREGATE_SLEEP_MS})]);\n  \
           return both[1] === undefined ? \"woke\" : \"early\";\n}};"
    );
    served::cell(&format!(
        "const body = await processes.create({{ dialect: \"typescript\", source: `{body}` }});\n\
         await processes.start({{ definition: body }});\n\
         finish(\"started\");"
    ))
}

/// An aggregate's sleep longer than any step bound parks the process on a
/// durable wait: once its call settled, the process actor releases as
/// `waiting` with the sleep's deadline as its next due, owned by nobody.
/// Node A is killed during the sleep and B started; the process wakes at
/// the deadline it was admitted with, within a claim poll of it, and its
/// body answers that the sleep fulfilled.
async fn an_aggregate_sleep_past_every_step_bound_wakes_at_its_deadline_across_a_crash(tier: Tier) {
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
    let deployment = Holding::running(dialect, postgres_url, sleeping_cell());
    let clock = SimClock::new();
    let database = deployment.database(Arc::clone(&clock)).await;
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        deployment.config(),
        deployment.activation(),
    );
    let session = deployment
        .core()
        .session(session())
        .create(lash::SessionCreation::root(served::spec(LIMIT)))
        .await
        .expect("the session is created");
    session
        .send(lash::TurnInput::text(INPUT))
        .await
        .expect("the turn's input is sent");
    nodes.start("a");

    // Run A until the process parks on its sleep, released as `waiting`.
    let half = AGGREGATE_SLEEP_MS / 2;
    let (process, parked) = loop {
        if let [process] = deployment.registered().await.as_slice() {
            let actor = ActorKey::process(process.id.as_str()).unwrap();
            if let Some(snapshot) = database
                .actor(&actor)
                .await
                .expect("read the process actor")
                && snapshot.state == ActorState::Waiting
            {
                break (process.clone(), snapshot);
            }
            assert!(
                !process.is_terminal(),
                "the process ended at {} ms, before its sleep's deadline: {:?}",
                clock.logical_ms(),
                process.outcome()
            );
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < half,
            "the process never parked on its sleep by {half} ms:\n{}",
            nodes.script().rendered_trace()
        );
    };
    let parked_at = clock.logical_ms();
    let process_actor = ActorKey::process(process.id.as_str()).unwrap();
    let started_at = nodes
        .script()
        .trace()
        .iter()
        .find(|write| {
            write.point.label == CommitLabel::PROCESS_ADVANCE
                && write.committed()
                && write.actor.as_ref() == Some(&process_actor)
        })
        .map(|write| write.at_ms)
        .expect("the process advanced");
    let due = parked
        .next_due
        .expect("the parked process is next due at its sleep's deadline");
    let deadline = u64::try_from(due.0).unwrap() - SimClock::timestamp_ms_at(0);
    let mut violations = Vec::new();
    if parked.owner.is_some()
        || deadline < started_at + AGGREGATE_SLEEP_MS
        || deadline > parked_at + AGGREGATE_SLEEP_MS
    {
        violations.push(format!(
            "the process started at {started_at} parked at {parked_at} owned by {:?}, next due \
             at {deadline}, not {AGGREGATE_SLEEP_MS} ms after it slept",
            parked.owner
        ));
    }

    // Kill A during the sleep, and let B take the process over.
    while clock.logical_ms() < half {
        assert!(nodes.step().await.is_some(), "A stalled while sleeping");
    }
    nodes.kill("a");
    nodes.quiesce().await;
    nodes.start("b");
    let horizon = deadline + 60_000;
    let ended = loop {
        if let [process] = deployment.registered().await.as_slice()
            && process.is_terminal()
        {
            break process.clone();
        }
        assert!(
            clock.logical_ms() < horizon && nodes.step().await.is_some(),
            "the process is not done {} ms after its sleep's deadline:\n{}",
            clock.logical_ms().saturating_sub(deadline),
            nodes.script().rendered_trace()
        );
    };
    nodes.quiesce().await;
    let woke = nodes
        .script()
        .trace()
        .iter()
        .find(|write| {
            write.point.label == CommitLabel::PROCESS_TERMINAL
                && write.committed()
                && write.actor.as_ref() == Some(&process_actor)
        })
        .map(|write| (write.node.to_string(), write.at_ms));
    let poll = Matrix::test_lease().settings().claim_poll.as_millis() as u64;
    if !woke
        .as_ref()
        .is_some_and(|(node, at)| node == "b" && *at >= deadline && *at <= deadline + poll + 5_000)
    {
        violations.push(format!(
            "the process due at {deadline} ended at {woke:?}, not on B within a claim poll of it"
        ));
    }
    match ended.outcome() {
        Some(lash_core::ProcessAwaitOutput::Settled { output })
            if output.is_success()
                && output.value_for_projection() == serde_json::json!("woke") => {}
        other => violations.push(format!("the process answered {other:?}, not \"woke\"")),
    }
    let quick = deployment
        .world
        .entries
        .lock_recover()
        .get("quick")
        .cloned()
        .unwrap_or_default();
    if quick.len() != 1 {
        violations.push(format!("the call ran {} times: {quick:?}", quick.len()));
    }
    assert!(
        violations.is_empty(),
        "aggregate sleep across a crash on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

tiered_laws!(
    current_thread:
    tool_call_limit_counts_what_a_process_holds_across_a_worker_kill,
    an_aggregate_sleep_past_every_step_bound_wakes_at_its_deadline_across_a_crash,
);
