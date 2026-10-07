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

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::actor::round::{BodyOutput, ToolBody};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    Backend, BackendParts, DurableSettings, EngineAction, EngineEvent, EngineState,
    EngineStateFormat, LifetimeDecision, NoProjectionProviders, ProcessEngine, ProcessId,
    ProcessInfraError, ProcessInput, ProcessOutcome, ProcessProvenance, ProcessRecord,
    ProcessRegistration, StepName, StepRequest, ToolCallId, ToolCallOutput,
};
use lash_core_store::tool_run::{
    AttemptOutcome, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole,
};
use lash_durable::runner::{Activation, Stopped};
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore, FormatSet, LeaseConfig};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy, ToolId};
use serde_json::{Value, json};

use dialect::Dialect;

const KIND: &str = "drain-proof";
const FIRST: &str = "drain_first";
const SECOND: &str = "drain_second";

/// The proof engine: `Started` runs the first step, its settlement the
/// second, and the second's settlement ends the process.
struct DrainEngine;

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
impl ProcessEngine for DrainEngine {
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
        let mut settled = match &event {
            EngineEvent::Started { .. } => 0,
            _ => serde_json::from_slice::<u64>(&state.bytes).map_err(infra)?,
        };
        let action = match event {
            EngineEvent::Started { .. } => EngineAction::Steps(vec![step("first", FIRST)]),
            EngineEvent::StepSettled { .. } => {
                settled += 1;
                if settled == 1 {
                    EngineAction::Steps(vec![step("second", SECOND)])
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
                format: self.state_format(),
                bytes: serde_json::to_vec(&settled).map_err(infra)?,
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
            "the drain engine stores no artifact `{artifact_ref}`"
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

impl ProcessSteps for DrainSteps {
    fn admit(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Ok(StepAdmission {
            policy: ExecutionPolicy::Once,
            limit: ExecutionLimit::starting_at(
                now_ms,
                Duration::from_secs(60),
                Duration::from_secs(60),
            ),
        })
    }

    fn body(&self, process: &ProcessRecord, step: &StepRequest, _call: &ToolCallId) -> ToolBody {
        let world = Arc::clone(&self.world);
        let process = process.id.clone();
        let tool = step.admitted_tool(KIND).as_str().to_owned();
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
            lease: LeaseConfig::default(),
            decodes: self.backend().formats().decodes(),
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
            },
            ProcessProvenance::host(),
            LifetimeDecision::Detached,
        )
        .with_execution_env_ref(Some(
            lash_core_execution::testing::process_execution_env_fixture_ref(),
        ));
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
            .and_then(|terminal| serde_json::to_value(terminal.clone().into_await_output()).ok())
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
        .run(|| Drain::new(dialect, postgres_url.clone()))
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
