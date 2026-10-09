//! A host engine's catalog tool step on the production process steps,
//! killed at every commit label (FIG-5216).
//!
//! A host starts one process of a host engine through the core's process
//! API. Its engine asks for one tool step that runs `ext_write`, a host
//! catalog tool declared `Once` whose body writes to an [`ExternalWorld`]
//! that survives every node, then ends with what the step answered. The
//! tool is pinned from the process's catalog and runs through the round a
//! turn's tools run on.
//!
//! The process actor runs on the production process activation with the
//! core's own durable process worker as its steps. The nodes are simulated
//! (A and B) over the production SQLite store.
//!
//! The matrix cuts the uncut run at every labelled write, under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks:
//!
//! - the step's body ran at most once and wrote at most once, and a
//!   terminal that says it wrote is backed by exactly that one write;
//! - the process committed exactly one terminal: the work resumed on
//!   another owner and ended;
//! - a zombie's writes after its reap are refused.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core_execution::{
    Backend, EngineAction, EngineEvent, EngineState, EngineStateFormat, ProcessEngine, ProcessId,
    ProcessInfraError, StoreSet,
};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sync::MutexExt as _;

const ENGINE: &str = "process-step-proof";
const TOOL: &str = "ext_write";
const WROTE: &str = "wrote";

fn process_actor(process: &ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).expect("a process actor key")
}

/// The outside world: every write `ext_write` made, by call. It survives
/// every node, as the world outside a deployment does.
#[derive(Debug, Default)]
struct ExternalWorld {
    writes: Mutex<Vec<(lash_core::ToolCallId, serde_json::Value)>>,
}

struct ExtWrite {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl StaticToolExecute for ExtWrite {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world
            .writes
            .lock_recover()
            .push((call.context.call_id().clone(), call.args.clone()));
        lash_core::ToolOutcome::ok(serde_json::json!({ WROTE: call.args })).into()
    }
}

fn ext_write(world: &Arc<ExternalWorld>) -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL,
        "Writes x to the outside world, once.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } },
            "required": ["x"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("ext_write's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(lash_core::ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL));
    Arc::new(StaticToolProvider::new(
        vec![definition],
        ExtWrite {
            world: Arc::clone(world),
        },
    ))
}

/// The engine: one step on its start payload, then a terminal carrying the
/// step's payload.
#[derive(Default)]
struct WriteEngine {
    unavailable: Option<Arc<std::sync::atomic::AtomicBool>>,
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
}

#[async_trait::async_trait]
impl ProcessEngine for WriteEngine {
    async fn check_args(
        &self,
        _signature: &lash_core_execution::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_core_execution::ArgsMode,
    ) -> std::result::Result<(), lash_core_execution::ArgsMismatch> {
        Err(lash_core_execution::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        ENGINE
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: ENGINE.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core_execution::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core_execution::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, lash_core_execution::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        _state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        if self
            .unavailable
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
        {
            return Err(infra("the process engine is temporarily unavailable"));
        }
        let action = match event {
            EngineEvent::Started { payload } => {
                let step = lash_core_execution::StepName("write".to_owned());
                EngineAction::Steps {
                    steps: vec![lash_core_execution::StepRequest::Tool {
                        step,
                        tool: lash_sansio::ToolId::new(TOOL),
                        input: payload,
                        site: None,
                    }],
                    wake: None,
                }
            }
            EngineEvent::StepSettled { outcome, .. } => {
                EngineAction::Terminal(lash_core_execution::ProcessOutcome::from_tool_output(
                    lash_core_execution::ToolCallOutput::success(serde_json::json!({
                        "step": outcome.payload().unwrap_or("interrupted"),
                    })),
                ))
            }
            EngineEvent::Cancelled { origin, .. } => {
                EngineAction::Terminal(lash_core_execution::ProcessOutcome::from_tool_output(
                    lash_core_execution::ToolCallOutput::cancelled(
                        lash_core_execution::ToolCancellation::runtime("cancelled")
                            .with_origin(origin),
                    ),
                ))
            }
            _ => EngineAction::Idle,
        };
        let state = EngineState {
            format: self.state_format(),
            bytes: serde_json::to_vec(&serde_json::Value::Null).map_err(infra)?,
        };
        Ok((state, action))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
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
        ))
    }
}

/// The environment the host's start captures: the core's plugins at their
/// defaults, and the standard protocol's builtin renderer, which the tool's
/// output is rendered by.
fn environment() -> lash_core_execution::ProcessExecutionEnvSpec {
    let mut environment = lash_core_execution::ProcessExecutionEnvSpec::new(
        lash_core_execution::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
            lash_core::NoProgressBudget::bounded(12),
        ),
        lash_core_execution::SessionToolAccess::ambient(),
    );
    environment.render = Some(lash_core::RecordedRender {
        renderer_id: lash::render::ToolOutputRendererSlot::default()
            .0
            .id()
            .to_owned(),
        params: serde_json::to_value(lash::render::ResolvedStandardRenderConfig {
            defaults: lash::render::ToolRenderParams::default(),
            per_tool: std::collections::BTreeMap::new(),
        })
        .expect("the render config encodes"),
    });
    environment
}

struct StepProof {
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<ProcessId>>,
}

impl StepProof {
    fn new() -> Self {
        Self {
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            process: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core over the scenario's backend: it serves no node
    /// of its own, the simulated nodes run its actors.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let builder = lash::LashCore::standard_builder(backend)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .tools(ext_write(&self.world));
                builder
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("process-step-deployment"),
                        lash::persistence::LeaseIncarnationId::new("process-step-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    fn process(&self) -> Option<ProcessId> {
        self.process.lock_recover().clone()
    }

    /// Start the process through the core's process API, as a host does,
    /// uncut.
    async fn start_process(&self) -> Result<ProcessId, String> {
        let core = self.core();
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&lash_core::HostArtifactPin::mint(), &environment())
            .await
            .map_err(|error| format!("publish the environment: {error}"))?;
        let request = lash_core::ProcessStartRequest::new(
            lash_core::ProcessInput::Engine {
                kind: ENGINE.to_owned(),
                payload: serde_json::json!({ "x": 7 }),
            },
            lash_core::ProcessOriginator::host_scoped("proof-host"),
            lash_core::LifetimeDecision::Detached,
        )
        .with_env_ref(env_ref);
        core.processes()
            .start(request, core.effect_host())
            .await
            .map(|receipt| receipt.process_id)
            .map_err(|error| format!("start the process: {error}"))
    }
}

#[async_trait::async_trait]
impl Scenario for StepProof {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = sim::memory(clock).await;
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let stores: Arc<dyn StoreSet> = Arc::new(stores);
        let backend = lash::durable::DurableBackendBuilder::new(stores)
            .process_engine(Arc::new(WriteEngine::default()))
            .build()
            .expect("the backend assembles");
        *self.backend.lock_recover() = Some(backend);
        database
    }

    fn config(&self) -> SimNodesConfig {
        // The node's backend decodes every engine the core registers.
        let (node, _) =
            lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
                .expect("the core's node activation");
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: node.formats().decodes(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        // What the core's own node runs: its sessions, and its processes
        // on its durable process worker.
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let process = self.start_process().await?;
        *self.process.lock_recover() = Some(process);
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        self.process()
            .as_ref()
            .map(process_actor)
            .into_iter()
            .collect()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let Some(process) = self.process() else {
            return false;
        };
        matches!(
            nodes.database().actor(&process_actor(&process)).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let Some(process) = self.process() else {
            return vec!["nothing was started".to_owned()];
        };
        let trace = nodes.script().trace();

        // `Once`: the step's body ran at most once.
        let writes = self.world.writes.lock_recover().clone();
        if writes.len() > 1 {
            violations.push(format!(
                "the Once step ran {} times: {writes:?}",
                writes.len()
            ));
        }
        let wrote = writes.len();
        if cut.is_none() && wrote != 1 {
            violations.push(format!("the uncut step wrote {wrote} times ({writes:?})"));
        }

        // The work resumed and ended once.
        let terminals = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::PROCESS_TERMINAL && write.committed())
            .count();
        if terminals != 1 {
            violations.push(format!("the process committed {terminals} terminals"));
        }

        // A terminal that says the tool wrote is backed by its one write.
        let answer = self
            .backend()
            .process_registry()
            .get_process(&process)
            .await
            .ok()
            .flatten()
            .and_then(|record| record.terminal().cloned())
            .map(|terminal| format!("{:?}", terminal.into_await_output()));
        match answer {
            Some(answer) if answer.contains(WROTE) && wrote != 1 => violations.push(format!(
                "the terminal says the step wrote, but it wrote {wrote} times \
                 ({writes:?}, {writes:?}): {answer}"
            )),
            Some(_) => {}
            None => violations.push("the process holds no terminal".to_owned()),
        }

        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
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

/// The owner commits of the uncut run: the start's transition admits the
/// step, its outcome commits, the transition that reads it ends the
/// process, and its terminal's cascade drains.
fn uncut_labels() -> Vec<CommitLabel> {
    vec![
        CommitLabel::PROCESS_ADVANCE,
        CommitLabel::STEP_OUTCOME,
        CommitLabel::PROCESS_TERMINAL,
        CommitLabel::CASCADE_BATCH,
    ]
}

/// The uncut run: the tool step runs once and the process ends with its
/// answer.
#[tokio::test]
async fn a_host_engines_tool_step_runs_on_the_production_steps() {
    let report = Matrix::new().faults(&[]).run_test(StepProof::new).await;
    report.assert_held();
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(labels, uncut_labels());
}

/// A host engine's `Once` tool step killed at every process commit label
/// resumes on another owner, never writes twice, and ends its process once.
#[tokio::test]
async fn a_host_engines_once_tool_step_killed_at_every_label_runs_at_most_once() {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(StepProof::new)
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "process step: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in uncut_labels() {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// FIG-5582: a process's restricted authority survives a file-store reopen
/// and the engine's redrive of its parked activation.
#[tokio::test]
async fn restricted_process_tool_access_survives_reopen_and_redrive() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let directory = tempfile::tempdir().expect("SQLite directory");
    let path = directory.path().join("authority.db");
    let clock = SimClock::new();
    let world = Arc::new(ExternalWorld::default());
    let unavailable = Arc::new(AtomicBool::new(true));
    let authority =
        lash_core::SessionToolAccess::restricted(Vec::new()).expect("no resident tools");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut process = None;

    for boot in 0..2 {
        let stores = sim::file(&path, Arc::clone(&clock)).await;
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
            .process_engine(Arc::new(WriteEngine {
                unavailable: Some(Arc::clone(&unavailable)),
            }))
            .build()
            .expect("backend");
        let captures = Arc::clone(&observed);
        let probe = Arc::new(lash_core::plugin::PluginSpecFactory::new(
            lash_core::plugin::PluginDeclaration::initial("process-authority-probe"),
            Arc::new(move |context| {
                if matches!(context.owner, lash_core::RuntimeOwner::Process(_)) {
                    captures.lock_recover().push(context.tool_access.clone());
                }
                Ok(lash_core::plugin::PluginSpec::new())
            }),
        ));
        let core = lash::LashCore::standard_builder(backend.clone())
            .serve_sessions(false)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .tools(ext_write(&world))
            .plugin(probe)
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("authority-law"),
                lash::persistence::LeaseIncarnationId::new(format!("boot-{boot}")),
            ))
            .expect("core");
        if boot == 0 {
            let mut env = environment();
            env.tool_access = authority.clone();
            let reference = core
                .host_artifacts()
                .publish_process_env(&lash_core::HostArtifactPin::mint(), &env)
                .await
                .expect("publish environment");
            process = Some(
                core.processes()
                    .start(
                        lash_core::ProcessStartRequest::new(
                            lash_core::ProcessInput::Engine {
                                kind: ENGINE.to_owned(),
                                payload: serde_json::json!({ "x": 7 }),
                            },
                            lash_core::ProcessOriginator::host(),
                            lash_core::LifetimeDecision::Detached,
                        )
                        .with_env_ref(reference),
                        core.effect_host(),
                    )
                    .await
                    .expect("start process")
                    .process_id,
            );
        } else {
            unavailable.store(false, Ordering::SeqCst);
            assert!(
                backend
                    .redrive_process(process.as_ref().expect("process"), "authority-law")
                    .await
                    .expect("redrive")
            );
        }
        let id = process.as_ref().expect("process");
        let (node, activation) =
            lash::testing::node_activation(&core, Arc::new(Tripwire::default()))
                .expect("node activation");
        let nodes = SimNodes::new(
            database,
            Arc::clone(&clock),
            lash_durable_test::Script::new(),
            SimNodesConfig {
                lease: Matrix::test_lease(),
                decodes: node.formats().decodes(),
                max_active: 4,
            },
            activation,
        );
        nodes.start("authority-node");
        let expected = if boot == 0 {
            ActorState::Parked
        } else {
            ActorState::Terminal
        };
        let mut reached = false;
        for _ in 0..400 {
            nodes.quiesce().await;
            if nodes
                .database()
                .actor(&process_actor(id))
                .await
                .expect("actor")
                .is_some_and(|actor| actor.state == expected)
            {
                reached = true;
                break;
            }
            if nodes.step().await.is_none() {
                clock.advance_by(1000).await;
            }
        }
        assert!(reached, "the process reaches {expected:?}");
        nodes.stop("authority-node");
        nodes.quiesce().await;
        core.shutdown().await.expect("shutdown");
    }
    let observed = observed.lock_recover();
    assert!(
        !observed.is_empty(),
        "the redriven process built its tool runtime"
    );
    assert!(
        observed.iter().all(|access| access == &authority),
        "the process lost its restriction: {observed:?}"
    );
    assert!(
        world.writes.lock_recover().is_empty(),
        "a restricted process cannot call the deployment's resident tool"
    );
}
