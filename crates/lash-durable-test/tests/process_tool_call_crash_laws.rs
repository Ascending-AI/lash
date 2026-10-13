//! The laws of a process body's tool calls a node kill must not break
//! (FIG-4546, ADR 0117; ported by FIG-5216 from the deleted lash-conformance
//! `tool_batch_parallelism/limit.rs` worker-kill law and
//! `tool_call_identity/admission.rs` replay law).
//!
//! A host sends an RLM turn whose cell starts a Lash VM process and awaits
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
//!
//! A process's `call_dynamic` call is two committed steps (P6, R10): its
//! resolution, whose committed outcome is the match, then the call of the
//! matched tool, admitted by the commit that consumes the resolution. Cut
//! at every write of the process's passes and step outcomes, a resumed
//! call runs the target the committed match names under one call id, and
//! the resolver never runs once a resolution committed: neither for a
//! resolved path (law 10, DYNAMIC-TWO-STEP) nor for a refused or
//! unavailable one, which the body catches typed.

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
    fn tool_manifests(&self, _config: &lash_core::OwnerConfigView) -> Vec<lash_core::ToolManifest> {
        vec![probe_definition().manifest()]
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
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
        "{body}\n\
         const held = await processes.start({{ definition: body }});\n\
         await control.finish(await held);"
    ))
}

/// The component that serves the probe to a process body.
const PROBE_COMPONENT: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("process-holding-probe", lash_core::FormatVersion::ONE);

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
                lash::LashCore::rlm_builder(backend.clone(), served::rlm(&backend, sim::workers(&clock)))
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1)).execution_budgets(lash::ExecutionBudgets::recommended()).delta_coalescing(lash::DeltaCoalescing::recommended())
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
                        },
                        lash_core::testing::run_policy_naming(&[
                            PROBE_COMPONENT,
                            lash::process_controls::PROCESS_CONTROLS_CONFIG,
                        ]),),
                    ))
                    // The body's `await` of the race links `processes.await`
                    // (P2): the env names the process controls that serve it.
                    .process_component(
                        Arc::new(lash::process_controls::ProcessControlsComponent::new(
                            |_| lash_core::Lifetime::Detached,
                            lash_core::testing::run_policy_naming(&[PROBE_COMPONENT]),
                        )),
                        lash::process_controls::PROCESS_CONTROLS_CONFIG,
                    )
                    .process_component(
                        lash_core::testing::tool_component(
                            PROBE_COMPONENT,
                            Arc::new(Probe {
                                world: Arc::clone(&self.world),
                            }),
                        ),
                        PROBE_COMPONENT,
                    )
                    .tools(lash::hook_key!("tools"), Arc::new(Probe {
                        world: Arc::clone(&self.world),
                    }))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(lash::persistence::LeaseOwnerId::new("process-holding-deployment"), lash::persistence::LeaseIncarnationId::new("process-holding-boot")))
                    .expect("the core builds")
            })
            .clone()
    }

    async fn registered(&self) -> Vec<lash_core::ProcessRecord> {
        lash_core::testing::process_roster_records_for_fixture(
            self.backend().process_registry().as_ref(),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            },
        )
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
            serves: lash::testing::node_serves(&self.core()),
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
/// lash_vm engine's two-minute step bound and past the session's idle
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
        "{body}\n\
         await processes.start({{ definition: body }});\n\
         await control.finish(\"started\");"
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

/// The component that serves a process's `call_dynamic` calls: its deferred
/// provider resolves them and executes what it matched.
const DYNAMIC_COMPONENT: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("process-dynamic", lash_core::FormatVersion::ONE);

/// The call path the resolver resolves, to `dyn_first` the first time it is
/// asked and to `dyn_second` every time after: a path resolved again names
/// another target.
const RESOLVED_PATH: &str = "search.run";
/// The call path the resolver refuses.
const REFUSED_PATH: &str = "search.refused";
/// A call path no tool answers.
const MISSING_PATH: &str = "search.missing";

/// What the dynamic laws observe of every node: the outside world.
#[derive(Default)]
struct Resolutions {
    /// Each resolver call: the path, and how many writes the trace held
    /// when it ran.
    resolved: Mutex<Vec<(String, usize)>>,
    /// Each body entry of a matched target: its tool and call id.
    entries: Mutex<Vec<(String, lash_core::ToolCallId)>>,
    /// The nodes whose trace the resolver reads.
    nodes: Mutex<Option<std::sync::Weak<SimNodes>>>,
}

/// A target the resolver matches: `Repeatable`, so a call a kill interrupted
/// runs again under its id.
fn dynamic_target(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Records each body entry's call id.",
        object.clone(),
        object,
    )
    .expect("the target's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

/// The deferred provider: it advertises nothing, resolves on demand, and
/// executes the targets it matched.
struct Resolver {
    seen: Arc<Resolutions>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Resolver {
    fn tool_manifests(&self, _config: &lash_core::OwnerConfigView) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        ["dyn_first", "dyn_second"]
            .contains(&name)
            .then(|| Arc::new(dynamic_target(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let tool = call.name().to_owned();
        self.seen
            .entries
            .lock_recover()
            .push((tool.clone(), call.context.call_id().clone()));
        ToolOutcome::ok(serde_json::json!({ "tool": tool })).into()
    }
}

#[async_trait::async_trait]
impl lash_core::DeferredToolProvider for Resolver {
    async fn resolve(
        &self,
        _cx: &lash_core::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash_core::DeferredOutcome> {
        let at = self
            .seen
            .nodes
            .lock_recover()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .map_or(0, |nodes| nodes.script().trace().len());
        let mut resolved = self.seen.resolved.lock_recover();
        let mut answers = BTreeMap::new();
        for path in paths {
            let earlier = resolved.iter().any(|(seen, _)| seen == path);
            resolved.push(((*path).to_owned(), at));
            let outcome = match *path {
                RESOLVED_PATH => lash_core::DeferredOutcome::Resolved {
                    provider: DYNAMIC_COMPONENT.tool_provider("deferred"),
                    definition: dynamic_target(if earlier { "dyn_second" } else { "dyn_first" }),
                    binding: None,
                },
                REFUSED_PATH => lash_core::DeferredOutcome::Refused {
                    reason: "not for this process".to_owned(),
                },
                _ => continue,
            };
            answers.insert((*path).to_owned(), outcome);
        }
        answers
    }
}

/// `dynamic()` calls the target `search.run` resolves to through
/// `call_dynamic`, and answers what it answered.
const RESOLVING: &str = r#"kernel 1
numbers float
effect lash.call_dynamic(input: Record{args?: Any, path: Text}) -> Any
entry dynamic() -> Any

fn dynamic() {
  let request = {path: "search.run", args: {q: "x"}}
  let got = perform lash.call_dynamic(request) as Any
  return got
}

main {
  finish null
}
"#;

/// `dynamic()` calls a refused path and a path no tool answers, catches
/// each failure, and answers both.
const DECIDING: &str = r#"kernel 1
numbers float
effect lash.call_dynamic(input: Record{args?: Any, path: Text}) -> Any
entry dynamic() -> Any

fn dynamic() {
  let refusing = {path: "search.refused", args: {}}
  let refused = null
  try {
    set refused = perform lash.call_dynamic(refusing) as Any
  } catch e {
    set refused = {kind: e.kind, data: e.data}
  }
  let looking = {path: "search.missing"}
  let missing = null
  try {
    set missing = perform lash.call_dynamic(looking) as Any
  } catch e {
    set missing = {kind: e.kind, data: e.data}
  }
  return [refused, missing]
}

main {
  finish null
}
"#;

/// A deployment whose host starts one kernel process running `document`'s
/// `dynamic()` under an env that links only `call_dynamic`.
struct DynamicCalls {
    dialect: Dialect,
    postgres_url: Option<String>,
    document: &'static str,
    seen: Arc<Resolutions>,
    backend: Mutex<Option<lash::Backend>>,
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<lash_core::ProcessId>>,
    tripwire: Arc<Tripwire>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl DynamicCalls {
    fn new(dialect: Dialect, postgres_url: Option<String>, document: &'static str) -> Self {
        Self {
            dialect,
            postgres_url,
            document,
            seen: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            process: Mutex::default(),
            tripwire: Arc::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let clock = self
            .clock
            .lock_recover()
            .clone()
            .expect("the database is built first");
        let resolver = Arc::new(Resolver {
            seen: Arc::clone(&self.seen),
        });
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::rlm_builder(
                    backend.clone(),
                    served::rlm(&backend, sim::workers(&clock)),
                )
                .serve_sessions(false)
                .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                .data_retention(lash::DataRetention::standard())
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                .execution_budgets(lash::ExecutionBudgets::recommended())
                .delta_coalescing(lash::DeltaCoalescing::recommended())
                .process_component(
                    Arc::new(
                        lash::plugins::StaticPluginFactory::new(
                            lash::plugins::PluginDeclaration::initial(
                                DYNAMIC_COMPONENT.component(),
                            ),
                            lash::plugins::PluginSpec::new()
                                .with_deferred_provider(lash::hook_key!("deferred"), resolver),
                        )
                        .as_component(DYNAMIC_COMPONENT.format())
                        .expect("a deferred provider is a process component"),
                    ),
                    DYNAMIC_COMPONENT,
                )
                .build(lash::persistence::LeaseOwnerIdentity::opaque(
                    lash::persistence::LeaseOwnerId::new("process-dynamic-deployment"),
                    lash::persistence::LeaseIncarnationId::new("process-dynamic-boot"),
                ))
                .expect("the core builds")
            })
            .clone()
    }

    async fn registered(&self) -> Vec<lash_core::ProcessRecord> {
        lash_core::testing::process_roster_records_for_fixture(
            self.backend().process_registry().as_ref(),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            },
        )
        .await
        .expect("the registry lists its processes")
    }

    /// The host's start of `dynamic()` under the env linking `call_dynamic`
    /// to the component's deferred provider.
    async fn start_process(&self) -> Result<lash_core::ProcessId, String> {
        let core = self.core();
        let document = lash::workflow::document::parse_document(self.document)
            .map_err(|error| format!("the document parses: {error}"))?;
        let identity = lash_vm_runtime::KernelDocuments::new(self.backend().module_artifacts())
            .publish(
                &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                    lash_core::HostArtifactPin::mint(),
                ))
                .expect("a host pin is unguarded"),
                &document,
            )
            .await
            .map_err(|error| format!("publish the document: {error}"))?;
        let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
            document: identity,
            entry: lash::workflow::document::Name::new("dynamic"),
            args: serde_json::Map::new(),
        })
        .expect("the input encodes");
        let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(LIMIT))
            .component(DYNAMIC_COMPONENT, &())
            .link_dynamic(DYNAMIC_COMPONENT.tool_provider("deferred"))
            .build()
            .map_err(|refusal| format!("the env builds: {refusal}"))?;
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&lash_core::HostArtifactPin::mint(), &env)
            .await
            .map_err(|error| format!("publish the env: {error}"))?;
        let kind = lash_vm_runtime::LASH_VM_ENGINE_KIND;
        let settings = core
            .processes()
            .engine_defaults(kind, &payload)
            .map_err(|error| format!("the kernel's settings: {error}"))?;
        core.processes()
            .start(
                lash_core::ProcessStartRequest::engine(
                    kind.to_owned(),
                    payload,
                    lash_core::EngineLaunch::new(env_ref, settings),
                    lash_core::ProcessOriginator::host(),
                    lash_core::LifetimeDecision::Detached,
                ),
                core.effect_host(),
            )
            .await
            .map(|receipt| receipt.process_id)
            .map_err(|error| format!("start the process: {error}"))
    }

    /// The resolver never ran for a path once a resolution of it committed:
    /// every resolver call of a path precedes the first step outcome the
    /// process committed after that path was first resolved.
    fn resolved_once(&self, trace: &[lash_durable_test::Write], process: &ActorKey) -> Vec<String> {
        let resolved = self.seen.resolved.lock_recover().clone();
        let mut violations = Vec::new();
        let paths: BTreeSet<&String> = resolved.iter().map(|(path, _)| path).collect();
        for path in paths {
            let calls: Vec<usize> = resolved
                .iter()
                .filter(|(seen, _)| seen == path)
                .map(|(_, at)| *at)
                .collect();
            let first = calls.iter().copied().min().unwrap_or_default();
            let Some(committed) = trace
                .iter()
                .enumerate()
                .skip(first)
                .find_map(|(at, write)| {
                    (write.point.label == CommitLabel::STEP_OUTCOME
                        && write.committed()
                        && write.actor.as_ref() == Some(process))
                    .then_some(at)
                })
            else {
                violations.push(format!("no resolution of `{path}` committed"));
                continue;
            };
            if let Some(late) = calls.iter().find(|at| **at > committed) {
                violations.push(format!(
                    "`{path}` was resolved again at write {late}, after its resolution \
                     committed at write {committed}"
                ));
            }
        }
        violations
    }

    /// The laws of one run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let trace = nodes.script().trace();
        let processes = self.registered().await;
        let [process] = processes.as_slice() else {
            return vec![format!("{} processes registered", processes.len())];
        };
        let actor = ActorKey::process(process.id.as_str()).unwrap();
        violations.extend(self.resolved_once(&trace, &actor));
        let answer = match process.outcome() {
            Some(lash_core::ProcessAwaitOutput::Settled { output }) if output.is_success() => {
                output.value_for_projection()
            }
            other => {
                violations.push(format!("the process ended {other:?}"));
                serde_json::Value::Null
            }
        };
        let entries = self.seen.entries.lock_recover().clone();
        if self.document == RESOLVING {
            // The target that ran is the one the committed match named,
            // under one call id across every entry.
            match entries.first() {
                None => violations.push("the matched target never ran".to_owned()),
                Some((tool, call)) => {
                    if entries
                        .iter()
                        .any(|entry| entry.0 != *tool || entry.1 != *call)
                    {
                        violations.push(format!(
                            "the target's body entries saw more than one target or call id: \
                             {entries:?}"
                        ));
                    }
                    if answer != serde_json::json!({ "tool": tool }) {
                        violations.push(format!(
                            "the process answered {answer}, not the target `{tool}` that ran"
                        ));
                    }
                }
            }
        } else {
            // Each decision is answered to the body as the typed failure it
            // recorded; no tool ran.
            if !entries.is_empty() {
                violations.push(format!("a refused or unavailable path ran: {entries:?}"));
            }
            for (index, cause, path) in [
                (0, "dynamic_call_refused", REFUSED_PATH),
                (1, "dynamic_call_unavailable", MISSING_PATH),
            ] {
                let caught = &answer[index];
                if caught["kind"] != lash_vm_runtime::TOOL_FAILED
                    || caught["data"]["cause"]["kind"] != cause
                    || caught["data"]["cause"]["path"] != path
                {
                    violations.push(format!(
                        "the body did not catch `{path}` as a tool failure caused by {cause}: \
                         {caught}"
                    ));
                }
            }
        }
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        if !violations.is_empty() {
            violations.push(format!(
                "resolutions: {:?}",
                self.seen.resolved.lock_recover()
            ));
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for DynamicCalls {
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
            serves: lash::testing::node_serves(&self.core()),
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        *self.seen.nodes.lock_recover() = Some(Arc::downgrade(nodes));
        // The host is outside the deployment under test: its start is uncut.
        let process = self.start_process().await?;
        *self.process.lock_recover() = Some(process);
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        self.process
            .lock_recover()
            .iter()
            .map(|process| ActorKey::process(process.as_str()).unwrap())
            .collect()
    }

    async fn done(&self, _nodes: &SimNodes) -> bool {
        let processes = self.registered().await;
        !processes.is_empty() && processes.iter().all(lash_core::ProcessRecord::is_terminal)
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        self.laws(nodes, cut).await
    }
}

/// The matrix of `document`'s process on `tier`: every write of its passes
/// and step outcomes cut under every fault, recovered on the other node.
async fn dynamic_matrix(tier: Tier, document: &'static str) {
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
    let labels = [CommitLabel::PROCESS_ADVANCE, CommitLabel::STEP_OUTCOME];
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
        .run_test(|| DynamicCalls::new(dialect, postgres_url.clone(), document))
        .await;
    eprintln!("{dialect:?}: {} cells", report.cells.len());
    report.assert_held();
    for label in labels {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// DYNAMIC-TWO-STEP (law 10; P6, R10): a process's `call_dynamic` call is
/// its resolution, whose committed outcome is the match, then the call of
/// the matched tool, which the commit consuming the resolution admits. A
/// crash anywhere between them resumes the same target under the same call
/// id, and the resolver, which names another target every time it is asked
/// again, never runs once the resolution committed.
async fn a_dynamic_call_resumes_its_matched_target_and_never_resolves_again(tier: Tier) {
    dynamic_matrix(tier, RESOLVING).await;
}

/// A refused path and a path no tool answers are each a durable decision
/// (P6, S6): the body catches each as its typed failure, no tool runs, and
/// a crash after a decision committed never resolves its path again.
async fn a_refused_or_unavailable_dynamic_call_is_decided_once_and_caught_typed(tier: Tier) {
    dynamic_matrix(tier, DECIDING).await;
}

tiered_laws!(
    current_thread:
    tool_call_limit_counts_what_a_process_holds_across_a_worker_kill,
    an_aggregate_sleep_past_every_step_bound_wakes_at_its_deadline_across_a_crash,
    a_dynamic_call_resumes_its_matched_target_and_never_resolves_again,
    a_refused_or_unavailable_dynamic_call_is_decided_once_and_caught_typed,
);
