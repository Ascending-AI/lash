//! L11 (FIG-5187): a build drains by release (ADR 0106 §1).
//!
//! One process runs on the production process activation over the
//! production store, SQLite in memory or PostgreSQL. Its engine runs a `Once` step, then a second
//! `Once` step, then ends. Node A, of the old build, claims it; while the
//! first step's body runs, A starts draining and node B, of a newer build
//! that also decodes a successor format set, starts. A finishes the body,
//! commits its outcome, starts nothing more and releases the process
//! `ready` under `drain.release`; its runner then stops `Drained`. B claims
//! the released process and finishes it.
//!
//! The matrix cuts `drain.release` under fail-before, ack-hidden, zombie,
//! abort and commit-then-abort, and checks D1:
//!
//! - the process ended with its result, and its terminal committed on B;
//! - A committed no transition after its drain began;
//! - each `Once` body ran exactly once, and nothing was looked up for
//!   re-running code;
//! - where A survived the cut, its runner stopped `Drained`.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

// Shared with the matrices that also run a SQLite file leg.
#[allow(dead_code)]
#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

#[path = "support/scripted_steps.rs"]
mod scripted_steps;

use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use lash_core_execution::DurableSettings;
use lash_core_execution::ToolCallOutput;
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_core_store::tool_run::{MaterialOwner, MaterialRole};
use lash_durable::runner::{Activation, Stopped};
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore, FormatSet};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::EngineStateFormat;
use lash_execution_contracts::process::LifetimeDecision;
use lash_execution_contracts::process::ProcessEngine;
use lash_execution_contracts::process::ProcessInfraError;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::ProcessOutcome;
use lash_execution_contracts::process::ProcessProvenance;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::ProcessRegistration;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_execution_contracts::projection::NoProjectionProviders;
use lash_protocol_types::ExecutionLimit;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::tool_contract::ExecutionPolicy;
use serde_json::{Value, json};

use dialect::Dialect;

const KIND: &str = "drain-proof";
const FIRST: &str = "drain_first";
const SECOND: &str = "drain_second";

/// The proof engine: `Started` runs the first step, its settlement the
/// second, and the second's settlement ends the process.
struct DrainEngine;

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

fn step(name: &str, tool: &str) -> StepRequest {
    StepRequest::Engine {
        step: StepName(name.to_owned()),
        kind: lash_execution_contracts::process::EngineStepKind::new(tool),
        input: json!({ "step": name }),
    }
}

#[async_trait::async_trait]
impl ProcessEngine for DrainEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn inspect_link(
        &self,
        link: lash_vocabulary::LinkId,
        selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link,
            document: lash_vocabulary::DocumentId::from_bytes([0; 32]),
            selector: selector.clone(),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
    }
    async fn inspect_start(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "generic fixture has no linked code".into(),
            },
        )
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        Default::default()
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        KIND
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: KIND.to_owned(),
            version: 1,
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

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let mut settled = match &event {
            EngineEvent::Started { .. } => 0,
            _ => serde_json::from_slice::<u64>(&state.bytes).map_err(infra)?,
        };
        let action = match event {
            EngineEvent::Started { .. } => EngineAction::Steps {
                steps: vec![step("first", FIRST)],
                wake: None,
            },
            EngineEvent::StepSettled { .. } => {
                settled += 1;
                if settled == 1 {
                    EngineAction::Steps {
                        steps: vec![step("second", SECOND)],
                        wake: None,
                    }
                } else {
                    EngineAction::Terminal(ProcessOutcome::from_tool_output(
                        ToolCallOutput::success(json!({ "drained": true })),
                    ))
                }
            }
            _ => EngineAction::Idle,
        };
        Ok((
            EngineState {
                staged: Vec::new(),
                format: self.state_format(),
                bytes: serde_json::to_vec(&settled).map_err(infra)?,
                pages: None,
                requirements: Default::default(),
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<
        Vec<lash_core_execution::ArtifactName>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the drain engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
    }
}

/// Every step body's entry, by tool; and the deployment, so the first body
/// can start the drain while it runs.
#[derive(Default)]
struct World {
    entries: Mutex<Vec<String>>,
    nodes: OnceLock<Weak<SimNodes>>,
}

struct DrainSteps {
    world: Arc<World>,
}

#[async_trait::async_trait]
impl ProcessSteps for DrainSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _target: Option<&lash_core_execution::runtime::process::steps::InvocationTarget>,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Ok(StepAdmission::new(
            scripted_steps::tool(_step),
            ExecutionPolicy::Once,
            ExecutionLimit::starting_at(now_ms, Duration::from_secs(60), Duration::from_secs(60)),
            None,
        ))
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_store::tool_run::Material<lash_core_store::tool_run::CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_store::tool_run::SettledOutput {
        lash_core_store::tool_run::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        lash_core_execution::runtime::actor::round::member_body({
            let world = Arc::clone(&self.world);
            let process = process.id.clone();
            let tool = scripted_steps::tool(step).as_str().to_owned();
            Box::new(move |_token| {
                world.entries.lock_recover().push(tool.clone());
                Box::pin(async move {
                    if tool == FIRST
                        && let Some(nodes) = world.nodes.get().and_then(Weak::upgrade)
                    {
                        // The operator starts the roll while the body runs: A
                        // drains, and the newer build's node starts.
                        nodes.drain("a");
                        nodes.start("b");
                    }
                    let output = json!({ "ok": tool }).to_string();
                    SettledOutput::Completed(Material::journal_local(
                        MaterialOwner::Process {
                            process_id: process,
                        },
                        MaterialRole::AttemptOutput,
                        output,
                    ))
                })
            })
        })
    }
}

struct Drain {
    dialect: Dialect,
    postgres_url: Option<String>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    world: Arc<World>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    process: Mutex<Option<ProcessId>>,
}

impl Drain {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            keep: Mutex::default(),
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            process: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn process(&self) -> Option<ProcessId> {
        self.process.lock_recover().clone()
    }
}

fn actor(process: &ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).expect("a process actor key")
}

/// The set only the newer build writes: B decodes it beside the old ones.
fn successor() -> FormatSet {
    FormatSet::new("process:drain-proof-next")
}

#[async_trait::async_trait]
impl Scenario for Drain {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        let backend = Backend::assemble(BackendParts {
            formats: Vec::new(),
            stores,
            settings: DurableSettings::default(),
            engines: vec![Arc::new(DrainEngine)],
            providers: Arc::new(NoProjectionProviders),
        })
        .expect("the drain backend assembles");
        *self.backend.lock_recover() = Some(backend);
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self.backend().formats().decodes(),
            serves: Vec::new(),
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(ProcessActivation::new(
            self.backend(),
            Arc::new(DrainSteps {
                world: Arc::clone(&self.world),
            }),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let _ = self.world.nodes.set(Arc::downgrade(nodes));
        let registration = ProcessRegistration::new(
            ProcessInput::Engine {
                kind: KIND.to_owned(),
                payload: json!({}),
                launch: lash_execution_contracts::process::EngineLaunch::new(
                    lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                    None,
                ),
            },
            ProcessProvenance::host(),
            LifetimeDecision::Detached,
        );
        let process = self
            .backend()
            .process_registry()
            .register_process(registration)
            .await
            .map(|record| record.id)
            .map_err(|error| error.to_string())?;
        *self.process.lock_recover() = Some(process);
        let mut newer = self.backend().formats().decodes();
        newer.push(successor());
        nodes.decode_on("b", newer);
        nodes.start("a");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        self.process().iter().map(actor).collect()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let Some(process) = self.process() else {
            return false;
        };
        matches!(
            nodes.database().actor(&actor(&process)).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let Some(process) = self.process() else {
            return vec!["nothing was seeded".to_owned()];
        };
        let record = self
            .backend()
            .process_registry()
            .get_process(&process)
            .await
            .ok()
            .flatten();
        let end = record
            .as_ref()
            .and_then(|record| record.terminal())
            .and_then(|terminal| {
                serde_json::to_value(lash_protocol_types::Durable(
                    &terminal.clone().into_await_output(),
                ))
                .ok()
            })
            .unwrap_or_default();
        if !end.to_string().contains("\"drained\":true") {
            violations.push(format!("the process did not end with its result: {end}"));
        }

        let trace = nodes.script().trace();
        let committed = |node: &str, label: CommitLabel| {
            trace.iter().any(|write| {
                &*write.node == node && write.point.label == label && write.committed()
            })
        };
        if !committed("b", CommitLabel::PROCESS_TERMINAL) {
            violations.push("the process's terminal did not commit on the newer build".to_owned());
        }
        // A starts nothing once its drain began: its last transition is
        // before its first drain release.
        if let Some(drained_at) = trace.iter().position(|write| {
            &*write.node == "a" && write.point.label == CommitLabel::DRAIN_RELEASE
        }) {
            for write in &trace[drained_at..] {
                if &*write.node == "a"
                    && write.committed()
                    && [CommitLabel::PROCESS_ADVANCE, CommitLabel::STEP_START]
                        .contains(&write.point.label)
                {
                    violations.push(format!("A committed {write} after its drain began"));
                }
            }
        } else {
            violations.push("A never released under drain.release".to_owned());
        }

        // Each Once body ran exactly once, and nothing re-ran.
        let entries = self.world.entries.lock_recover().clone();
        for tool in [FIRST, SECOND] {
            let runs = entries
                .iter()
                .filter(|entry| entry.as_str() == tool)
                .count();
            if runs != 1 {
                violations.push(format!("Once body {tool} ran {runs} times"));
            }
        }
        let counts = self.tripwire.counts();
        if counts.outcome_lookups.values().sum::<usize>() != 0 {
            violations.push("an outcome was looked up for re-running code".to_owned());
        }

        // A survivor of the cut stopped drained.
        let survived =
            cut.is_none_or(|cut| matches!(cut.fault, Fault::FailBefore | Fault::AckHidden));
        if survived {
            match nodes.stopped("a").await {
                Some(Ok(Stopped::Drained)) => {}
                other => violations.push(format!("A did not stop drained: {other:?}")),
            }
        }
        if let Some(cut) = cut
            && cut.fault == Fault::Zombie
        {
            for write in trace
                .iter()
                .filter(|write| &*write.node == "a" && write.cut == Some(Fault::Zombie))
            {
                if !matches!(write.stored, Stored::Refused(_)) {
                    violations.push(format!("the zombie's drain release was {:?}", write.stored));
                }
            }
        }
        violations
    }
}

/// D1 on `dialect`: every cut of `drain.release` leaves the process
/// finished on the newer build, each `Once` body run once.
async fn prove(dialect: Dialect, postgres_url: Option<String>) {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .labels(&[CommitLabel::DRAIN_RELEASE])
        .horizon(Duration::from_secs(600))
        .run_test(|| Drain::new(dialect, postgres_url.clone()))
        .await;
    report.assert_held();
    assert_eq!(
        report.cells.len(),
        5,
        "the matrix did not cut A's drain release under each fault"
    );
    assert!(
        report.labels().contains(&CommitLabel::DRAIN_RELEASE),
        "the matrix never cut drain.release"
    );
}

#[tokio::test]
async fn a_draining_node_releases_its_process_at_a_committed_phase_for_the_next_build() {
    prove(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn a_draining_node_releases_its_process_at_a_committed_phase_for_the_next_build_on_postgres()
{
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}
