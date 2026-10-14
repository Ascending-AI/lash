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

/// The component a process names to call `ext_write`.
const EXT_WRITE: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("ext-write", lash_core::FormatVersion::ONE);

fn ext_write_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
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
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL))
}

/// The process component that contributes `ext_write` over `world`.
fn ext_write(world: &Arc<ExternalWorld>) -> Arc<dyn lash::process::ProcessComponentFactory> {
    let provider = Arc::new(StaticToolProvider::new(
        vec![ext_write_definition()],
        ExtWrite {
            world: Arc::clone(world),
        },
    ));
    Arc::new(
        lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial(EXT_WRITE.component()),
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider(lash::tool_provider_key!("tools"), provider),
        )
        .as_component(EXT_WRITE.format())
        .expect("ext_write is a process component"),
    )
}

/// The engine: one step on its start payload, then a terminal carrying the
/// step's payload.
struct WriteEngine;

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
}

#[async_trait::async_trait]
impl ProcessEngine for WriteEngine {
    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash_core::PerformedEffects, lash_core::ProcessEnvRefusal> {
        Ok(lash_core_execution::testing::performs(&[
            ext_write_definition(),
        ]))
    }

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

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_core_execution::EngineSettings>,
    ) -> std::result::Result<(), lash_core_execution::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(lash_core_execution::PluginError::Registration(format!(
                "process engine `{}` takes no settings",
                self.kind()
            ))),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_core_execution::EngineSettings>,
        lash_core_execution::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        _state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let action = match event {
            EngineEvent::Started { payload } => {
                let step = lash_core_execution::StepName("write".to_owned());
                EngineAction::Steps {
                    steps: vec![lash_core_execution::StepRequest::Tool {
                        step,
                        effect: lash_core_execution::testing::effect_of(&ext_write_definition()),
                        input: payload,
                        site: None,
                        matched: None,
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
            pages: None,
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
        lash_core_execution::ProcessDefinitionError,
    > {
        Ok(lash_core_execution::ProcessDefinitionResolution::new(
            lash_core_execution::ProcessSignature::Unknown,
        ))
    }
}

/// The environment a host start states in these laws: `ext_write` from its
/// component.
fn environment() -> lash_core::ProcessEnv {
    environment_admitting(vec![ext_write_definition()])
}

/// An environment that names the `ext_write` component and admits `tools`.
fn environment_admitting(tools: Vec<lash_core::ToolDefinition>) -> lash_core::ProcessEnv {
    tools
        .iter()
        .fold(
            lash_core::ProcessEnv::builder(lash::MaxToolCalls::new(16)).component(EXT_WRITE, &()),
            |env, tool| {
                env.link_tool(
                    EXT_WRITE.tool_provider(lash::tool_provider_key!("tools")),
                    tool,
                )
            },
        )
        .build()
        .expect("the environment builds")
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
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .process_component(ext_write(&self.world), EXT_WRITE);
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
        let request = lash_core::ProcessStartRequest::engine(
            ENGINE.to_owned(),
            serde_json::json!({ "x": 7 }),
            lash_core::EngineLaunch::new(env_ref, None),
            lash_core::ProcessOriginator::host_scoped("proof-host"),
            lash_core::LifetimeDecision::Detached,
        );
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
            .process_engine(Arc::new(WriteEngine))
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
            serves: lash::testing::node_serves(&self.core()),
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
    report.assert_baseline_labels(&uncut_labels());
    for label in uncut_labels() {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}
