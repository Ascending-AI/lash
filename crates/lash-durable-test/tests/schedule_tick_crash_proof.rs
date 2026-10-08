//! FIG-5348: a scheduled trigger source's tick fires exactly once across a
//! crash at the tick.
//!
//! One session owns one enabled subscription to a scheduled source that
//! ticks twice. The production session activation, with the deployment's
//! scheduled sources, keeps the next tick as the session's due time and
//! emits each tick through the production trigger router when the claim
//! that due time causes runs, on simulated nodes A and B over the production
//! store. Each delivery starts a process that ends at once.
//!
//! The matrix cuts every `trigger.start` write (the tick's occurrence and
//! its delivery, committed together) and every `session.release` under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks:
//!
//! - T1, once per tick: the store holds exactly one occurrence per tick,
//!   naming its tick, each with one delivery bound to one process, and the
//!   registry holds exactly those processes;
//! - T2, every tick: both ticks fired, the second after the first, whatever
//!   the cut lost.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::runtime::durable::head::SessionHead;
use lash_core::runtime::durable::schedules::ScheduledTriggers;
use lash_core::runtime::durable::session::{
    AdmittedInputs, OpenTurn, SessionActivation, TurnDrive, TurnError, TurnRestore, TurnRow,
    TurnServices,
};
use lash_core_execution::facade_support::TriggerRouter;
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    ActorContext, Backend, BackendParts, DurableSettings, EngineAction, EngineEvent, EngineState,
    EngineStateFormat, ExecutionBudgets, NoProjectionProviders, ProcessEngine, ProcessInfraError,
    ProcessRecord, StepRequest, StoreSet, TriggerSchedule, TriggerScheduleError, TriggerSchedules,
};
use lash_durable::{
    ActorDispatch, ActorKey, ActorState, CommitLabel, DurableStore, FormatSet, MailTx,
};
use lash_durable_test::{Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

const KIND: &str = "schedule-tick-proof";
const SOURCE_TYPE: &str = "proof.schedule";
const SESSION: &str = "schedule-tick-proof-session";
/// How far after the store set's first instant the first tick falls, and
/// how far apart the two ticks are.
const TICK_SPACING_MS: u64 = 30_000;

// --- the started process's engine -------------------------------------------

/// The engine every delivery starts: it ends as soon as it starts.
struct EndsAtOnce;

#[async_trait::async_trait]
impl ProcessEngine for EndsAtOnce {
    fn kind(&self) -> &'static str {
        KIND
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
        _artifact_ref: &str,
    ) -> Result<(), lash_core_execution::PluginError> {
        Ok(())
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::ZERO
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
        state: EngineState,
        _event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        Ok((
            state,
            EngineAction::Terminal(lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::Value::Null),
            )),
        ))
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

/// The started processes take no steps.
struct NoSteps;

#[async_trait::async_trait]
impl ProcessSteps for NoSteps {
    async fn admit(
        &self,
        _process: &ProcessRecord,
        step: &StepRequest,
        _now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Err(StepRefusal::UnknownTool {
            step: step.step().0.clone(),
            tool: step.admitted_tool(KIND).as_str().to_owned(),
        })
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_execution::runtime::actor::round::Material<
            lash_core_store::tool_run::CompletionSource,
        >,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_execution::runtime::actor::round::SettledOutput {
        lash_core_execution::runtime::actor::round::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        unreachable!("no step of `{}` is ever admitted", step.step().0)
    }
}

// --- the session's turns and schedule ----------------------------------------

/// The session is sent nothing, so it never runs a turn.
struct NoTurns;

#[async_trait::async_trait]
impl TurnServices for NoTurns {
    fn execution_budgets(&self, _session: &SessionId) -> ExecutionBudgets {
        ExecutionBudgets::default()
    }

    async fn start(
        &self,
        _cx: &ActorContext,
        row: &TurnRow,
        _head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        Err(TurnError::Exec(format!(
            "the proof runs no turn, yet {} opened",
            row.run
        )))
    }

    async fn resume(
        &self,
        _cx: &ActorContext,
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        Err(TurnError::Exec(format!(
            "the proof runs no turn, yet {} opened",
            restore.row().run
        )))
    }

    async fn apply_commands(
        &self,
        _cx: &ActorContext,
        admitted: &AdmittedInputs,
    ) -> Result<(), TurnError> {
        Err(TurnError::Exec(format!(
            "the proof sends no command, yet run {} is one",
            admitted.run
        )))
    }
}

/// A schedule that ticks at two fixed instants.
struct TwoTicks {
    ticks: [u64; 2],
}

impl TriggerSchedule for TwoTicks {
    fn occurrence_source(
        &self,
        source: &serde_json::Value,
    ) -> Result<serde_json::Value, TriggerScheduleError> {
        Ok(source.clone())
    }

    fn next_tick(
        &self,
        _source: &serde_json::Value,
        after_ms: u64,
    ) -> Result<Option<u64>, TriggerScheduleError> {
        Ok(self.ticks.into_iter().find(|tick| *tick > after_ms))
    }

    fn last_tick(
        &self,
        _source: &serde_json::Value,
        at_ms: u64,
    ) -> Result<Option<u64>, TriggerScheduleError> {
        Ok(self.ticks.into_iter().rev().find(|tick| *tick <= at_ms))
    }

    fn payload(&self, tick_ms: u64) -> serde_json::Value {
        serde_json::json!({ "tick": tick_ms })
    }
}

fn session() -> SessionId {
    SessionId::from(SESSION)
}

fn session_actor() -> ActorKey {
    ActorKey::session(SESSION).expect("a valid actor id")
}

fn source() -> serde_json::Value {
    serde_json::json!({ "every": "two ticks" })
}

async fn processes(backend: &Backend) -> Vec<ProcessRecord> {
    backend
        .process_registry()
        .list_processes(&lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            ..lash_core_execution::ProcessListFilter::default()
        })
        .await
        .unwrap_or_default()
}

// --- the scenario ----------------------------------------------------------------

/// Where a scenario's database lives.
#[derive(Clone, Copy, Debug)]
enum Dialect {
    SqliteMemory,
    Postgres,
}

struct Proof {
    dialect: Dialect,
    postgres_url: Option<String>,
    backend: Mutex<Option<Backend>>,
    ticks: Mutex<[u64; 2]>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Proof {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            backend: Mutex::default(),
            ticks: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn ticks(&self) -> [u64; 2] {
        *self.ticks.lock_recover()
    }

    fn engines() -> lash_core_execution::ProcessEngineRegistry {
        lash_core_execution::ProcessEngineRegistry::new().with_registration(
            lash_core_execution::ProcessEngineRegistration::accepting(Arc::new(EndsAtOnce)),
        )
    }
}

/// A fresh isolated PostgreSQL database, provisioned from the schema, made
/// on a thread and runtime of its own (see `vertical_crash_proof`).
fn isolated_database(url: String) -> lash_postgres_store::testing::IsolatedDatabase {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a setup runtime")
            .block_on(lash_postgres_store::testing::IsolatedDatabase::create(&url))
    })
    .join()
    .expect("the isolated database is created")
}

#[async_trait::async_trait]
impl Scenario for Proof {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let first = lash_core_ids::clock::ClockWallTime::timestamp_ms(&*clock);
        let tick = first - first % 1_000 + TICK_SPACING_MS;
        *self.ticks.lock_recover() = [tick, tick + TICK_SPACING_MS];
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = match self.dialect {
            Dialect::SqliteMemory => {
                let stores = sim::memory(clock).await;
                let database = Arc::new(stores.durable_store());
                (Arc::new(stores), database)
            }
            Dialect::Postgres => {
                let url = self.postgres_url.clone().expect("a PostgreSQL URL");
                let isolated = isolated_database(url);
                let storage = lash_postgres_store::testing::connect(isolated.url())
                    .await
                    .expect("the isolated database opens");
                let database: Arc<dyn DurableStore> =
                    Arc::new(storage.durable_store().with_clock_for_testing(clock));
                let stores = lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
                );
                self.keep.lock_recover().push(Box::new(isolated));
                (Arc::new(stores), database)
            }
        };
        let backend = Backend::assemble(BackendParts {
            stores,
            settings: DurableSettings::default(),
            engines: vec![Arc::new(EndsAtOnce)],
            providers: Arc::new(NoProjectionProviders),
            formats: Vec::new(),
        })
        .expect("the proof backend assembles");
        *self.backend.lock_recover() = Some(backend);
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self
                .backend()
                .formats()
                .decodes()
                .into_iter()
                .chain([FormatSet::new(lash_durable::domain::SESSION_ACTOR_FORMATS)])
                .collect(),
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn lash_durable::runner::Activation> {
        let backend = self.backend();
        let router = TriggerRouter::new(
            backend.trigger_store(),
            lash_core_execution::ProcessWorkWiring::without_process_work(
                backend.process_registry(),
            ),
        )
        .with_process_artifacts(backend.process_env_store(), Self::engines());
        let mut schedules = TriggerSchedules::new();
        schedules.register(
            SOURCE_TYPE,
            Arc::new(TwoTicks {
                ticks: self.ticks(),
            }),
        );
        let probe = Arc::new(lash_durable::NoProbe);
        Arc::new(ActorDispatch {
            session: Arc::new(
                SessionActivation::new(backend.clone(), Arc::new(NoTurns), probe.clone())
                    .with_schedules(ScheduledTriggers::new(
                        backend.trigger_store(),
                        router,
                        schedules,
                    )),
            ),
            process: Arc::new(ProcessActivation::new(backend, Arc::new(NoSteps), probe)),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // The session's actor and its subscription are seeded straight into
        // the database, uncut.
        let backend = self.backend();
        let mut seed = MailTx::new();
        seed.create_actor(
            session_actor(),
            FormatSet::new(lash_durable::domain::SESSION_ACTOR_FORMATS),
        );
        nodes
            .database()
            .commit_mail(seed, CommitLabel::MAIL_SESSION)
            .await
            .map_err(|error| error.to_string())?;
        let env_ref = lash_core_execution::testing::process_execution_env_fixture(
            &*backend.process_env_store(),
        )
        .await;
        let source_key =
            lash_core_execution::facade_support::default_trigger_source_key(SOURCE_TYPE, &source());
        backend
            .trigger_store()
            .execute_command(
                "schedule-tick-proof-register",
                lash_core_execution::TriggerCommand::Register {
                    owner_scope: lash_core_execution::TriggerOwnerScope::session(session()),
                    actor: lash_core_execution::ProcessOriginator::session(
                        lash_core_execution::SessionScope::new(session()),
                    ),
                    draft: lash_core_execution::TriggerSubscriptionDraft::for_process(
                        "two-ticks",
                        env_ref,
                        SOURCE_TYPE,
                        source_key,
                        lash_core_execution::ProcessInput::Engine {
                            kind: KIND.to_owned(),
                            payload: serde_json::json!({}),
                        },
                        lash_core_execution::ProcessIdentity::labelled(KIND, Some("tick")),
                    )
                    .with_source(source())
                    .with_payload_schema(lash_core_execution::JsonSchema::any()),
                },
            )
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        // A starts and claims first, B once A is settled.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![session_actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let backend = self.backend();
        let fired = backend
            .trigger_store()
            .list_occurrences(lash_core_execution::TriggerOccurrenceFilter::default())
            .await
            .map_or(0, |occurrences| occurrences.len());
        if fired < 2 {
            return false;
        }
        for process in processes(&backend).await {
            let Ok(actor) = ActorKey::process(process.id.as_str()) else {
                return false;
            };
            match nodes.database().actor(&actor).await {
                Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal => {}
                _ => return false,
            }
        }
        true
    }

    async fn check(&self, _nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let backend = self.backend();
        let triggers = backend.trigger_store();

        // T1 and T2: one occurrence per tick, each naming its tick, and both
        // ticks fired.
        let occurrences = triggers
            .list_occurrences(lash_core_execution::TriggerOccurrenceFilter::default())
            .await
            .unwrap_or_default();
        let mut fired: Vec<u64> = occurrences
            .iter()
            .filter_map(|occurrence| occurrence.payload["tick"].as_u64())
            .collect();
        fired.sort_unstable();
        if occurrences.len() != 2 || fired != self.ticks() {
            violations.push(format!(
                "T1/T2: the ticks {:?} fired as {fired:?} in {} occurrences",
                self.ticks(),
                occurrences.len()
            ));
        }
        // T1: each occurrence has one delivery, bound to one process, and the
        // registry holds exactly those processes.
        let mut bound = BTreeSet::new();
        for occurrence in &occurrences {
            let deliveries = triggers
                .list_deliveries_by_occurrence_id(&occurrence.occurrence_id)
                .await
                .unwrap_or_default();
            match deliveries.as_slice() {
                [delivery] => match delivery.process_id() {
                    Some(process) => {
                        bound.insert(process.clone());
                    }
                    None => violations.push(format!(
                        "T1: the tick {} delivery started nothing: {:?}",
                        occurrence.payload, delivery.outcome
                    )),
                },
                deliveries => violations.push(format!(
                    "T1: the tick {} holds {} deliveries",
                    occurrence.payload,
                    deliveries.len()
                )),
            }
        }
        let registered: BTreeSet<_> = processes(&backend)
            .await
            .into_iter()
            .map(|record| record.id)
            .collect();
        if registered != bound {
            violations.push(format!(
                "T1: the deliveries are bound to {bound:?}, the registry holds {registered:?}"
            ));
        }
        violations
    }
}

const FAULTS: [Fault; 5] = [
    Fault::FailBefore,
    Fault::AckHidden,
    Fault::Zombie,
    Fault::Abort,
    Fault::CommitThenAbort,
];

async fn prove(dialect: Dialect, postgres_url: Option<String>) {
    let report = Matrix::new()
        .faults(&FAULTS)
        .labels(&[CommitLabel::TRIGGER_START, CommitLabel::SESSION_RELEASE])
        .horizon(Duration::from_secs(600))
        .run_test(|| Proof::new(dialect, postgres_url.clone()))
        .await;
    eprintln!("schedule tick on {dialect:?}: {} cells", report.cells.len());
    report.assert_held();
    assert!(
        report.labels().contains(&CommitLabel::TRIGGER_START),
        "the matrix never cut a tick's trigger.start"
    );
}

/// T1 and T2 on SQLite in memory, at every cut of the tick.
#[tokio::test]
async fn a_scheduled_tick_fires_once_at_every_cut_on_sqlite_memory() {
    prove(Dialect::SqliteMemory, None).await;
}

/// T1 and T2 on PostgreSQL, at every cut of the tick.
#[tokio::test]
async fn a_scheduled_tick_fires_once_at_every_cut_on_postgres() {
    let Some(url) = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
    else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}
