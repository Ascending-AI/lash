//! A host engine's catalog tool step and host step on the production
//! process steps, killed at every commit label (FIG-5216, FIG-5313).
//!
//! A host starts one process of a host engine through the core's process
//! API. Its engine asks for one step, then ends with what the step
//! answered:
//!
//! - a tool step that runs `ext_write`, a host catalog tool declared `Once`
//!   whose body writes to an [`ExternalWorld`] that survives every node; the
//!   tool is pinned from the process's catalog and runs through the round a
//!   turn's tools run on;
//! - or a host step its engine's registration declares, whose body
//!   registers a trigger subscription in the deployment's trigger store as
//!   the process's originator: its store-local effect.
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
/// The host operation the engine's host step performs.
const HOST_WRITE: &str = "proof.register";
/// The host binding the process's originator acts for, which owns the
/// subscription its host step registers.
const BINDING: &str = "process-step-proof-binding";
/// The subscription the host step registers.
const SUBSCRIPTION: &str = "process-step-proof-subscription";

/// Which step the engine asks for.
#[derive(Clone, Copy, Debug)]
enum StepKind {
    /// `ext_write`, a catalog tool.
    Tool,
    /// [`HOST_WRITE`], a host step of the engine's registration.
    Host,
}

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
struct WriteEngine {
    step: StepKind,
}

/// The engine's host step: it registers [`SUBSCRIPTION`] in the trigger
/// store as the process's originator, counting its runs in the world.
struct HostWrite {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl lash_core::EngineHostSteps for HostWrite {
    fn serves(&self, operation: &str) -> bool {
        operation == HOST_WRITE
    }

    fn execution(&self, _operation: &str) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }

    async fn run(
        &self,
        context: lash_core::RuntimeExecutionContext<'static>,
        run: lash_core::HostStepRun,
    ) -> lash_core::ToolCallOutput {
        self.world
            .writes
            .lock_recover()
            .push((run.call.clone(), run.input.clone()));
        match register(&context, run).await {
            Ok(revision) => lash_core::ToolCallOutput::success(
                serde_json::json!({ WROTE: { "revision": revision } }),
            ),
            Err(error) => lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
                lash_core::ToolFailureClass::Execution,
                "proof_register_failed",
                error,
            )),
        }
    }
}

/// Register [`SUBSCRIPTION`] over `context`, keyed by the step's call.
async fn register(
    context: &lash_core::RuntimeExecutionContext<'static>,
    run: lash_core::HostStepRun,
) -> Result<u64, String> {
    let claim = context.execution_claim().map_err(|e| e.to_string())?;
    let env_ref = context
        .captured_process_execution_env_ref(&claim)
        .await
        .map_err(|e| e.to_string())?;
    let draft = lash_core::TriggerSubscriptionDraft::for_process(
        SUBSCRIPTION,
        env_ref,
        "timer.tick",
        "proof-source",
        lash_core::ProcessInput::Engine {
            kind: ENGINE.to_owned(),
            payload: run.input,
        },
        lash_core::ProcessIdentity::new(ENGINE),
    );
    let command = lash_core::TriggerCommand::Register {
        owner_scope: context.trigger_owner_scope().map_err(|e| e.to_string())?,
        actor: context.trigger_actor().map_err(|e| e.to_string())?,
        draft,
    };
    match context
        .execute_trigger_effect(run.call.to_string(), command)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?
    {
        lash_core::TriggerCommandOutcome::Mutation { receipt } => Ok(receipt.record.revision),
        other => Err(format!("a registration answered {other:?}")),
    }
}

/// A plugin that contributes the engine with its host step: a host step is
/// declared by the engine's registration.
struct HostStepPlugin {
    world: Arc<ExternalWorld>,
}

struct NoSessionPlugin;

impl lash_core::plugin::SessionPlugin for NoSessionPlugin {
    fn id(&self) -> &'static str {
        "process-step-proof-plugin"
    }

    fn register(
        &self,
        _registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::plugin::PluginFactory for HostStepPlugin {
    fn id(&self) -> &'static str {
        "process-step-proof-plugin"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![
            lash_core::ProcessEngineRegistration::accepting(Arc::new(WriteEngine {
                step: StepKind::Host,
            }))
            .with_host_steps(Arc::new(HostWrite {
                world: Arc::clone(&self.world),
            })),
        ])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NoSessionPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for HostStepPlugin {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("process-step-proof-plugin")
    }
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
        let action = match event {
            EngineEvent::Started { payload } => {
                let step = lash_core_execution::StepName("write".to_owned());
                EngineAction::Steps(vec![match self.step {
                    StepKind::Tool => lash_core_execution::StepRequest::Tool {
                        language_execution: None,
                        step,
                        tool: lash_sansio::ToolId::new(TOOL),
                        input: payload,
                        site: None,
                    },
                    StepKind::Host => lash_core_execution::StepRequest::Host {
                        language_execution: None,
                        step,
                        operation: HOST_WRITE.to_owned(),
                        input: payload,
                        site: None,
                    },
                }])
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
            Vec::new(),
        ))
    }
}

/// The environment the host's start captures: the core's plugins at their
/// defaults, and the standard protocol's builtin renderer, which the tool's
/// output is rendered by.
fn environment() -> lash_core_execution::ProcessExecutionEnvSpec {
    let mut environment = lash_core_execution::ProcessExecutionEnvSpec::new(
        lash_core_execution::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(lash::TurnBudget::Unbounded, lash::MaxToolCalls::new(16)),
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
    step: StepKind,
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<ProcessId>>,
}

impl StepProof {
    fn new(step: StepKind) -> Self {
        Self {
            step,
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
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tools(ext_write(&self.world));
                let builder = match self.step {
                    StepKind::Tool => builder,
                    StepKind::Host => builder.plugin(Arc::new(HostStepPlugin {
                        world: Arc::clone(&self.world),
                    })),
                };
                builder
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "process-step-deployment",
                        "process-step-boot",
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
            lash_core::ProcessOriginator::host_scoped(BINDING),
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
        // A host step's engine is its plugin's contribution.
        let builder = lash::durable::DurableBackendBuilder::new(stores);
        let builder = match self.step {
            StepKind::Tool => builder.process_engine(Arc::new(WriteEngine {
                step: StepKind::Tool,
            })),
            StepKind::Host => builder,
        };
        let backend = builder.build().expect("the backend assembles");
        *self.backend.lock_recover() = Some(backend);
        database
    }

    fn config(&self) -> SimNodesConfig {
        // The node's backend decodes every engine the core registers, a
        // plugin's contribution among them.
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
        // A host step's write is its subscription: at most one, and only
        // from the body's one run.
        let subscriptions = match self
            .backend()
            .trigger_store()
            .list_subscriptions(lash_core::TriggerSubscriptionFilter::default())
            .await
        {
            Ok(subscriptions) => subscriptions,
            Err(error) => return vec![format!("the trigger store lists nothing: {error}")],
        };
        if subscriptions.len() > writes.len() {
            violations.push(format!(
                "{} subscriptions from {} runs: {subscriptions:?}",
                subscriptions.len(),
                writes.len()
            ));
        }
        let wrote = match self.step {
            StepKind::Tool => writes.len(),
            StepKind::Host => subscriptions.len(),
        };
        // Uncut, the step writes exactly once.
        if cut.is_none() && wrote != 1 {
            violations.push(format!(
                "the uncut step wrote {wrote} times ({writes:?}, {subscriptions:?})"
            ));
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
                 ({writes:?}, {subscriptions:?}): {answer}"
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
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| StepProof::new(StepKind::Tool))
        .await;
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
        .run_test(|| StepProof::new(StepKind::Tool))
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

/// The uncut run of a host step: it registers its subscription once, as the
/// process's originator, and the process ends with its answer.
#[tokio::test]
async fn a_host_engines_host_step_runs_on_the_production_steps() {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| StepProof::new(StepKind::Host))
        .await;
    report.assert_held();
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(labels, uncut_labels());
}

/// A host engine's host step killed at every process commit label resumes
/// on another owner, never registers its subscription twice, and ends its
/// process once: its trigger write is its store-local effect, admitted
/// `Once` (FIG-5313).
#[tokio::test]
async fn a_host_engines_host_step_killed_at_every_label_writes_at_most_once() {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| StepProof::new(StepKind::Host))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "host step: {} cells over {} labels ({})",
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
