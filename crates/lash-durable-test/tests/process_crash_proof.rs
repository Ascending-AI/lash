//! L6 (FIG-5175): the process actor's crash proof, ADR 0132 §2, §3 and §10.
//!
//! Two scenarios run on the production process activation over the
//! production SQLite store, on simulated nodes A and B.
//!
//! The run scenario seeds a detached process H that idles, a root process R
//! and three processes living `Until` R. R's engine runs a `Once` step and a
//! `Repeatable` step, then awaits H with a one-second deadline, and ends when
//! that wait times out; its terminal cascades over its children in batches
//! of two. The matrix cuts the uncut run at every labelled write under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks:
//!
//! - S1, state before action: every step body found its admission
//!   committed when it was entered;
//! - S2: `advance` is called again only with the same state and event;
//! - NR-1 and NR-2: the `Once` step's body ran exactly once when its
//!   outcome is `Completed`, at most once otherwise, and then it settled
//!   `Interrupted`;
//! - NR-3: a `Repeatable` step runs again only at its own admitted ordinal;
//! - NR-4: no outcome is looked up for re-running code, and no committed
//!   ordinal is emitted again;
//! - R ended with its timeout, and every child ended `ParentEnded`;
//! - a zombie's writes after its reap are refused.
//!
//! The cascade scenario cancels the root of a tree three levels deep, every
//! scope with three `Until` children and the cascade batch two, and cuts
//! every `cascade.batch` commit with a crash before and after it lands: the
//! whole tree ends, every descendant `ParentEnded`. A run always makes the
//! same batches, but which node makes each depends on which claims an actor
//! first, so the cuts are counted across both nodes.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::actor::round::{
    self, BodyOutput, PolicyView, Recovery, ToolBody,
};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    Ancestry, Backend, BackendParts, CancelOrigin, CompletionKeySecrets, DurableSettings,
    EngineAction, EngineEvent, EngineState, EngineStateFormat, LifetimeDecision,
    NoProjectionProviders, ProcessEngine, ProcessId, ProcessInfraError, ProcessInput,
    ProcessOutcome, ProcessProvenance, ProcessRecord, ProcessRegistration, ScopeGrant, ScopeId,
    StepName, StepRequest, ToolCallId, ToolCallOutput, ToolCancellation,
};
use lash_core_store::tool_run::{
    AttemptOutcome, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole,
};
use lash_durable::domain::{OwnerKey, PROCESS_FORMATS, RunRecordKind};
use lash_durable::runner::Activation;
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, FormatSet, LeaseConfig,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy, ToolId};
use serde_json::{Value, json};

const KIND: &str = "crash-proof";
const ONCE: &str = "proof_once";
const AGAIN: &str = "proof_again";
const WAIT_MS: u64 = 1_000;
const CASCADE_BATCH: usize = 2;
const WIDTH: usize = CASCADE_BATCH + 1;

// --- the engine and its steps ----------------------------------------------

/// Every `advance` call: the calling process's tag, the state it was
/// handed, and the event.
type AdvanceLog = Arc<Mutex<Vec<(String, Vec<u8>, String)>>>;

/// The proof engine. Its start payload is its state; `act` says what it
/// does: `root` runs both steps, then awaits `await`, and ends when the
/// wait times out; `hold` idles until cancelled. A counter makes every
/// committed state distinct.
struct ProofEngine {
    log: AdvanceLog,
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core_execution::PluginError::Session(error.to_string()))
}

fn step(name: &str, tool: &str) -> StepRequest {
    StepRequest::Tool {
        step: StepName(name.to_owned()),
        tool: ToolId::new(tool),
        input: json!({ "step": name }),
    }
}

#[async_trait::async_trait]
impl ProcessEngine for ProofEngine {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &Value,
    ) -> Option<lash_core_execution::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core_execution::ProcessExecutionEnvSpec,
    ) -> Result<Option<Value>, lash_core_execution::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let mut script: Value = match &event {
            EngineEvent::Started { payload } => payload.clone(),
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        self.log.lock_recover().push((
            script["tag"].as_str().unwrap_or_default().to_owned(),
            state.bytes.clone(),
            format!("{event:?}"),
        ));
        script["n"] = json!(script["n"].as_u64().unwrap_or(0) + 1);
        let root = script["act"] == "root";
        let action = match event {
            EngineEvent::Started { .. } if root => {
                EngineAction::Steps(vec![step("once", ONCE), step("again", AGAIN)])
            }
            EngineEvent::StepSettled { step, .. } if root => {
                let settled = script["settled"].as_u64().unwrap_or(0) + 1;
                script["settled"] = json!(settled);
                let _ = step;
                if settled == 2 {
                    EngineAction::AwaitProcess {
                        process: ProcessId::parse(script["await"].as_str().unwrap_or_default())
                            .map_err(infra)?,
                        deadline: Some(Duration::from_millis(WAIT_MS)),
                    }
                } else {
                    EngineAction::Idle
                }
            }
            EngineEvent::ProcessWaitTimedOut { .. } if root => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::success(
                    json!({ "timed_out": true }),
                )))
            }
            EngineEvent::Cancelled { origin, .. } => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the proof engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            _ => EngineAction::Idle,
        };
        let bytes = serde_json::to_vec(&script).map_err(infra)?;
        Ok((
            EngineState {
                format: self.state_format(),
                bytes,
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<Vec<lash_core_execution::ArtifactName>, lash_core_execution::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core_execution::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core_execution::PluginError> {
        Err(lash_core_execution::PluginError::Session(format!(
            "the proof engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        _reference: &lash_core_execution::ProcessDefinitionRef,
    ) -> Result<
        lash_core_execution::ProcessDefinitionResolution,
        lash_core_execution::ProcessDefinitionRefusal,
    > {
        Ok(lash_core_execution::ProcessDefinitionResolution::new(
            lash_core_execution::ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

/// Every step body's entry, with its call and tool, noted as the body is
/// entered; and every body that found no committed admission.
#[derive(Default)]
struct World {
    entries: Mutex<Vec<(ToolCallId, String)>>,
    unadmitted: Mutex<Vec<(ToolCallId, String)>>,
}

impl World {
    fn entries(&self) -> Vec<(ToolCallId, String)> {
        self.entries.lock_recover().clone()
    }
}

struct ProofSteps {
    world: Arc<World>,
    database: Arc<dyn DurableStore>,
}

fn policy(tool: &ToolId) -> ExecutionPolicy {
    if tool.as_str() == AGAIN {
        ExecutionPolicy::repeatable(std::num::NonZeroU32::new(3).expect("nonzero"), 0, 0)
    } else {
        ExecutionPolicy::Once
    }
}

impl ProcessSteps for ProofSteps {
    fn admit(
        &self,
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Ok(StepAdmission {
            policy: policy(&step.admitted_tool(KIND)),
            limit: ExecutionLimit::starting_at(
                now_ms,
                Duration::from_secs(60),
                Duration::from_secs(60),
            ),
        })
    }

    fn body(&self, process: &ProcessRecord, step: &StepRequest, call: &ToolCallId) -> ToolBody {
        let world = Arc::clone(&self.world);
        let database = Arc::clone(&self.database);
        let process = process.id.clone();
        let tool = step.admitted_tool(KIND).as_str().to_owned();
        let call = call.clone();
        Box::new(move |_token| {
            // Entered: noted before anything can stop the body.
            world
                .entries
                .lock_recover()
                .push((call.clone(), tool.clone()));
            Box::pin(async move {
                let admitted = database
                    .run_records(&OwnerKey::Process(process.clone()))
                    .await
                    .is_ok_and(|rows| rows.iter().any(|row| row.call.as_ref() == Some(&call)));
                if !admitted {
                    world.unadmitted.lock_recover().push((call.clone(), tool));
                }
                let output = json!({ "ok": true }).to_string();
                let material = MaterialPayload::new(
                    MaterialOwner::Process {
                        process_id: process,
                    },
                    MaterialRole::AttemptOutput,
                    None,
                    output.clone(),
                )
                .reference(MaterialLocation::JournalLocal)
                .expect("a step's output encodes");
                BodyOutput {
                    outcome: AttemptOutcome::Completed(material),
                    material: Some(output),
                }
            })
        })
    }
}

// --- the scenarios ----------------------------------------------------------

fn settings() -> DurableSettings {
    DurableSettings {
        cascade_batch: CASCADE_BATCH,
        ..DurableSettings::default()
    }
}

fn registration(payload: Value, parent: Option<&ProcessId>) -> ProcessRegistration {
    let lifetime = match parent {
        Some(parent) => LifetimeDecision::Until {
            scope: ScopeId::process(parent.clone()),
            grant: ScopeGrant::Ancestor,
        },
        None => LifetimeDecision::Detached,
    };
    let mut registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload,
        },
        ProcessProvenance::host(),
        lifetime,
    )
    .with_execution_env_ref(Some(
        lash_core_execution::testing::process_execution_env_fixture_ref(),
    ));
    if let Some(parent) = parent {
        registration.ancestry = Ancestry::from_scopes([ScopeId::process(parent.clone())]);
    }
    registration
}

fn hold(tag: &str) -> Value {
    json!({ "tag": tag, "act": "hold" })
}

/// The processes a run seeded: the root, then each level below it.
#[derive(Clone, Default)]
struct Seeded {
    root: Option<ProcessId>,
    below: Vec<Vec<ProcessId>>,
    others: Vec<ProcessId>,
}

impl Seeded {
    fn descendants(&self) -> impl Iterator<Item = &ProcessId> {
        self.below.iter().flatten()
    }

    fn tree(&self) -> impl Iterator<Item = &ProcessId> {
        self.root.iter().chain(self.descendants())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// R's steps, its await of H and its cascade over three children.
    Run,
    /// A cancelled tree three levels deep.
    Cascade,
}

struct Proof {
    shape: Shape,
    log: AdvanceLog,
    world: Arc<World>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    database: Mutex<Option<Arc<dyn DurableStore>>>,
    seeded: Mutex<Seeded>,
}

impl Proof {
    fn new(shape: Shape) -> Self {
        Self {
            shape,
            log: Arc::default(),
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            database: Mutex::default(),
            seeded: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn seeded(&self) -> Seeded {
        self.seeded.lock_recover().clone()
    }

    async fn register(&self, registration: ProcessRegistration) -> Result<ProcessId, String> {
        self.backend()
            .process_registry()
            .register_process(registration)
            .await
            .map(|record| record.id)
            .map_err(|error| error.to_string())
    }

    async fn outcome(&self, process: &ProcessId) -> Option<Value> {
        let record = self
            .backend()
            .process_registry()
            .get_process(process)
            .await
            .ok()??;
        record
            .terminal()
            .and_then(|terminal| serde_json::to_value(terminal.clone().into_await_output()).ok())
    }
}

fn actor(process: &ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).expect("a process actor key")
}

/// The first value under `key` anywhere in `value`.
fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(entries) => entries
            .get(key)
            .or_else(|| entries.values().find_map(|nested| find(nested, key))),
        Value::Array(items) => items.iter().find_map(|nested| find(nested, key)),
        _ => None,
    }
}

#[async_trait::async_trait]
impl Scenario for Proof {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
            .await
            .expect("an in-memory store set opens");
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let backend = Backend::assemble(BackendParts {
            stores: Arc::new(stores),
            settings: settings(),
            secrets: Some(CompletionKeySecrets::for_testing()),
            engines: vec![Arc::new(ProofEngine {
                log: Arc::clone(&self.log),
            })],
            providers: Arc::new(NoProjectionProviders),
        })
        .expect("the proof backend assembles");
        *self.backend.lock_recover() = Some(backend);
        *self.database.lock_recover() = Some(Arc::clone(&database));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: LeaseConfig::default(),
            decodes: vec![FormatSet::new(PROCESS_FORMATS)],
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(ProcessActivation::new(
            self.backend(),
            Arc::new(ProofSteps {
                world: Arc::clone(&self.world),
                database: self
                    .database
                    .lock_recover()
                    .clone()
                    .expect("the database is built first"),
            }),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // The producers are outside the deployment under test: the
        // registrations and the cancel are seeded straight into the
        // database, uncut.
        let mut seeded = Seeded::default();
        match self.shape {
            Shape::Run => {
                let held = self.register(registration(hold("h"), None)).await?;
                let root = json!({ "tag": "r", "act": "root", "await": held.as_str() });
                let root = self.register(registration(root, None)).await?;
                let mut children = Vec::new();
                for index in 0..WIDTH {
                    let tag = format!("c{index}");
                    children.push(self.register(registration(hold(&tag), Some(&root))).await?);
                }
                seeded.others.push(held);
                seeded.root = Some(root);
                seeded.below.push(children);
            }
            Shape::Cascade => {
                let root = self.register(registration(hold("t"), None)).await?;
                let mut parents = vec![root.clone()];
                for depth in 0..3 {
                    let mut level = Vec::new();
                    for (at, parent) in parents.iter().enumerate() {
                        for index in 0..WIDTH {
                            let tag = format!("t{depth}.{at}.{index}");
                            level.push(
                                self.register(registration(hold(&tag), Some(parent)))
                                    .await?,
                            );
                        }
                    }
                    seeded.below.push(level.clone());
                    parents = level;
                }
                self.backend()
                    .process_registry()
                    .request_process_cancel(
                        &root,
                        CancelOrigin::OperatorRequested,
                        "proof".to_owned(),
                        None,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                seeded.root = Some(root);
            }
        }
        *self.seeded.lock_recover() = seeded;
        // A starts and claims first, B once A is settled, as in V0.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        let seeded = self.seeded();
        seeded
            .tree()
            .chain(seeded.others.iter())
            .map(actor)
            .collect()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        for process in self.seeded().tree() {
            match nodes.database().actor(&actor(process)).await {
                Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal => {}
                _ => return false,
            }
        }
        true
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let seeded = self.seeded();
        let Some(root) = seeded.root.clone() else {
            return vec!["nothing was seeded".to_owned()];
        };

        // Every descendant ended ParentEnded; the root as its shape says.
        for process in seeded.descendants() {
            let end = self.outcome(process).await.unwrap_or_default();
            if find(&end, "origin").and_then(Value::as_str) != Some("parent_ended") {
                violations.push(format!(
                    "descendant {process} did not end ParentEnded: {end}"
                ));
            }
        }
        let end = self.outcome(&root).await.unwrap_or_default();
        let expected = match self.shape {
            Shape::Run => find(&end, "timed_out") == Some(&json!(true)),
            Shape::Cascade => {
                find(&end, "origin").and_then(Value::as_str) == Some("operator_requested")
            }
        };
        if !expected {
            violations.push(format!("the root did not end as its shape says: {end}"));
        }

        // S1: no step body ran without its committed admission.
        let entries = self.world.entries();
        for (call, tool) in self.world.unadmitted.lock_recover().iter() {
            violations.push(format!(
                "S1: {tool} body {call} ran before its admission committed"
            ));
        }

        // S2: advance is called again only with the same state and event.
        let mut seen: BTreeMap<(String, Vec<u8>), BTreeSet<String>> = BTreeMap::new();
        for (tag, state, event) in self.log.lock_recover().iter() {
            seen.entry((tag.clone(), state.clone()))
                .or_default()
                .insert(event.clone());
        }
        for ((tag, _), events) in &seen {
            if events.len() > 1 {
                violations.push(format!(
                    "S2: {tag} was advanced from one state with {events:?}"
                ));
            }
        }

        if self.shape == Shape::Run {
            violations.extend(self.step_laws(nodes.database(), &root, &entries).await);
        }
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &nodes.script().trace()));
        }
        violations
    }
}

impl Proof {
    /// NR-1 to NR-4 over R's two steps.
    async fn step_laws(
        &self,
        database: &Arc<dyn DurableStore>,
        root: &ProcessId,
        entries: &[(ToolCallId, String)],
    ) -> Vec<String> {
        let mut violations = Vec::new();
        let rows = database
            .run_records(&OwnerKey::Process(root.clone()))
            .await
            .unwrap_or_default();
        let policies = PolicyView::new([
            (ToolId::new(ONCE), policy(&ToolId::new(ONCE))),
            (ToolId::new(AGAIN), policy(&ToolId::new(AGAIN))),
        ]);
        let fold = match round::fold(&rows, &policies) {
            Ok(fold) => fold,
            Err(refusal) => return vec![format!("R's run records do not fold: {refusal}")],
        };
        let counts = self.tripwire.counts();
        let tools: BTreeMap<&ToolCallId, &str> = entries
            .iter()
            .map(|(call, tool)| (call, tool.as_str()))
            .collect();
        if fold.recoveries().len() != 2 {
            violations.push(format!(
                "R admitted {} steps, not 2",
                fold.recoveries().len()
            ));
        }
        for (id, recovery) in fold.recoveries() {
            let runs = counts.bodies.get(id).copied().unwrap_or(0);
            let tool = rows
                .iter()
                .find(|row| {
                    row.kind == RunRecordKind::XStart
                        && row.run == id.run
                        && row.ordinal == id.ordinal
                })
                .and_then(|row| row.call.as_ref())
                .and_then(|call| tools.get(call).copied());
            match recovery {
                // NR-1: a completed outcome came from exactly one run, or
                // from the reruns of a Repeatable at its own ordinal.
                Recovery::Settled(AttemptOutcome::Completed(_)) => {
                    if runs == 0 || (runs > 1 && tool != Some(AGAIN)) {
                        violations.push(format!("NR-1: {tool:?} completed after {runs} runs"));
                    }
                }
                // NR-2: an interrupted Once ran at most the once it started.
                Recovery::Settled(AttemptOutcome::Interrupted) => {
                    if runs > 1 {
                        violations.push(format!("NR-2: interrupted {tool:?} ran {runs} times"));
                    }
                }
                Recovery::Settled(other) => {
                    violations.push(format!("{tool:?} settled {other:?}"));
                }
                other => violations.push(format!("{tool:?} did not settle: {other:?}")),
            }
        }
        // NR-3: every body entry of one call is at one admitted ordinal.
        let calls: BTreeSet<&ToolCallId> = entries.iter().map(|(call, _)| call).collect();
        if counts.bodies.len() != calls.len() {
            violations.push(format!(
                "NR-3: {} calls were entered under {} admitted identities",
                calls.len(),
                counts.bodies.len()
            ));
        }
        // NR-4.
        if counts.outcome_lookups.values().sum::<usize>() != 0 {
            violations.push("NR-4: an outcome was looked up for re-running code".to_owned());
        }
        if counts.committed_ordinals.values().sum::<usize>() != 0 {
            violations.push("NR-4: a committed ordinal was emitted again".to_owned());
        }
        violations
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

const FAULTS: [Fault; 5] = [
    Fault::FailBefore,
    Fault::AckHidden,
    Fault::Zombie,
    Fault::Abort,
    Fault::CommitThenAbort,
];

/// Every process label the run commits, cut under every fault: S1, S2 and
/// NR-1 to NR-4 hold in every cell.
#[tokio::test]
async fn a_process_cut_at_every_label_runs_no_step_before_its_state_commits() {
    let report = Matrix::new()
        .faults(&FAULTS)
        .horizon(Duration::from_secs(600))
        .run(|| Proof::new(Shape::Run))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "process run: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in [
        CommitLabel::PROCESS_ADVANCE,
        CommitLabel::STEP_OUTCOME,
        CommitLabel::WAIT_TIMEOUT,
        CommitLabel::PROCESS_TERMINAL,
        CommitLabel::CASCADE_BATCH,
    ] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// A tree three levels deep, every scope wider than one cascade batch,
/// ends fully across a crash before and after each `cascade.batch` commit.
#[tokio::test]
async fn a_cascade_three_levels_deep_ends_across_a_crash_at_each_batch() {
    let report = Matrix::new()
        .faults(&[Fault::Abort, Fault::CommitThenAbort])
        .labels(&[CommitLabel::CASCADE_BATCH])
        .across_nodes()
        .horizon(Duration::from_secs(600))
        .run(|| Proof::new(Shape::Cascade))
        .await;
    eprintln!("cascade: {} cells", report.cells.len());
    report.assert_held();
    assert!(
        !report.cells.is_empty(),
        "the matrix cut no cascade.batch commit"
    );
}
