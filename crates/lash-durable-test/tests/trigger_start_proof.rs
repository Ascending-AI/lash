//! FIG-5175: a trigger occurrence starts its processes in the transaction
//! that records it (ADR 0132 §12).
//!
//! One emitter actor emits one occurrence that two subscriptions match,
//! through the production trigger router, on simulated nodes A and B over
//! the production store. The router plans the occurrence, prepares each
//! delivery's process, and commits the occurrence, both processes and both
//! deliveries bound to them in one `trigger.start` mailbox transaction
//! through its node's store. The started processes run on the production
//! process activation and end at once.
//!
//! The matrix cuts every `trigger.start` write under fail-before,
//! ack-hidden, zombie, abort and commit-then-abort, recovers on the other
//! node, and checks:
//!
//! - S1, exactly once: the store holds one occurrence, one delivery per
//!   subscription and one process per delivery, under the delivery's start
//!   key, and every emission that answered names exactly those processes;
//! - S2, all or nothing: whenever the emitter looks, at each activation and
//!   after each emission that failed, the store holds the whole start or
//!   none of it; a cut that never entered the store leaves no process and no
//!   reservation behind the prepared start.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::facade_support::{
    TriggerDeliveryEmitOutcome, TriggerEmitReport, TriggerRouter,
};
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    ActorContext, AdmittedScope, Backend, BackendParts, DurableSettings, EngineAction, EngineEvent,
    EngineState, EngineStateFormat, NoProjectionProviders, ProcessEngine, ProcessInfraError,
    ProcessRecord, StepRequest, StoreSet, TriggerOccurrenceRequest,
};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorKey, ActorKind, ActorState, CommitLabel, DurableError, DurableStore, FormatSet,
    LeaseConfig, MailKind, MailTx, Release,
};
use lash_durable_test::{Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig};
use lash_sansio::sync::MutexExt as _;

const KIND: &str = "trigger-start-proof";
const EMITTER_FORMATS: &str = "trigger-start-proof-v1";
const SOURCE_TYPE: &str = "proof.event";
const SOURCE_KEY: &str = "proof-source";
const SUBSCRIPTIONS: [&str; 2] = ["first", "second"];
const EMITTER_DONE: CommitLabel = CommitLabel::new("trigger-proof.done");
const EMIT_ATTEMPTS: usize = 8;

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

// --- what the store holds ----------------------------------------------------

/// What the store holds of the occurrence's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    occurrences: usize,
    deliveries: usize,
    processes: usize,
}

impl Held {
    const NOTHING: Self = Self {
        occurrences: 0,
        deliveries: 0,
        processes: 0,
    };
    const STARTED: Self = Self {
        occurrences: 1,
        deliveries: SUBSCRIPTIONS.len(),
        processes: SUBSCRIPTIONS.len(),
    };

    async fn read(backend: &Backend) -> Self {
        let triggers = backend.trigger_store();
        Self {
            occurrences: triggers
                .list_occurrences(lash_core_execution::TriggerOccurrenceFilter::default())
                .await
                .map_or(usize::MAX, |rows| rows.len()),
            deliveries: triggers
                .list_deliveries()
                .await
                .map_or(usize::MAX, |rows| rows.len()),
            processes: processes(backend).await.len(),
        }
    }
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

/// What the emitter saw, across every node it ran on.
#[derive(Default)]
struct World {
    /// The store at each activation's start and after each failed emission.
    seen: Mutex<Vec<Held>>,
    /// Every report an emission answered with.
    reports: Mutex<Vec<TriggerEmitReport>>,
    /// Every emission's error.
    errors: Mutex<Vec<String>>,
}

// --- the emitter ---------------------------------------------------------------

fn emitter() -> ActorKey {
    ActorKey::session("trigger-start-proof-emitter").expect("a valid actor id")
}

fn request() -> TriggerOccurrenceRequest {
    TriggerOccurrenceRequest::new(
        SOURCE_TYPE,
        SOURCE_KEY,
        serde_json::json!({"proof": true}),
        "trigger-start-proof-occurrence",
    )
}

/// The emitter emits the occurrence until an emission answers, then
/// settles its mail; a started process runs on the process activation.
struct Activations {
    backend: Backend,
    router: Arc<TriggerRouter>,
    processes: ProcessActivation,
    world: Arc<World>,
}

#[async_trait::async_trait]
impl Activation for Activations {
    async fn activate(&self, owned: Owned) -> Exit {
        if owned.actor().kind() == ActorKind::Process {
            return self.processes.activate(owned).await;
        }
        let cx = ActorContext::claimed(
            self.backend.clone(),
            &owned,
            AdmittedScope::runtime_operation("trigger-start-proof-emitter"),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(lash_durable::NoProbe),
        );
        let held = Held::read(&self.backend).await;
        self.world.seen.lock_recover().push(held);
        for _ in 0..EMIT_ATTEMPTS {
            match self.router.emit(request(), &cx).await {
                Ok(report) => {
                    self.world.reports.lock_recover().push(report);
                    break;
                }
                Err(error) => {
                    self.world.errors.lock_recover().push(error.to_string());
                    let held = Held::read(&self.backend).await;
                    self.world.seen.lock_recover().push(held);
                    lash_core_ids::clock::Clock::sleep(&**owned.clock(), Duration::from_secs(1))
                        .await;
                }
            }
        }
        loop {
            let mut tx = match owned.begin().await {
                Ok(tx) => tx,
                Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(_) => {
                    owned.wait_for_mail().await;
                    continue;
                }
            };
            tx.ack_seen().give_up(Release::Idle);
            match owned.commit(tx, EMITTER_DONE).await {
                Ok(_) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(_) => continue,
            }
        }
    }
}

// --- the scenario ----------------------------------------------------------------

/// Where a scenario's database lives.
#[derive(Clone, Copy, Debug)]
enum Dialect {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

struct Proof {
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    backend: Mutex<Option<Backend>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Proof {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            world: Arc::default(),
            backend: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
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
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = match self.dialect {
            Dialect::SqliteMemory => {
                let stores = sim::memory(clock).await;
                let database = Arc::new(stores.durable_store());
                (Arc::new(stores), database)
            }
            Dialect::SqliteFile => {
                let dir = tempfile::tempdir().expect("a temporary directory");
                let stores = sim::file(dir.path().join("lash.db"), clock).await;
                self.keep.lock_recover().push(Box::new(dir));
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
            lease: LeaseConfig::default(),
            decodes: self
                .backend()
                .formats()
                .decodes()
                .into_iter()
                .chain([FormatSet::new(EMITTER_FORMATS)])
                .collect(),
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        let backend = self.backend();
        let router = TriggerRouter::new(
            backend.trigger_store(),
            lash_core_execution::ProcessWorkWiring::without_process_work(
                backend.process_registry(),
            ),
        )
        .with_process_artifacts(backend.process_env_store(), Self::engines());
        Arc::new(Activations {
            processes: ProcessActivation::new(
                backend.clone(),
                Arc::new(NoSteps),
                Arc::new(lash_durable::NoProbe),
            ),
            backend,
            router: Arc::new(router),
            world: Arc::clone(&self.world),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // The subscriptions, their environment and the emitter's mail are
        // seeded straight into the database, uncut.
        let backend = self.backend();
        let env_ref = lash_core_execution::testing::process_execution_env_fixture(
            &*backend.process_env_store(),
        )
        .await;
        let triggers = backend.trigger_store();
        for key in SUBSCRIPTIONS {
            triggers
                .execute_command(
                    &format!("trigger-start-proof-register-{key}"),
                    lash_core_execution::TriggerCommand::Register {
                        owner_scope: lash_core_execution::TriggerOwnerScope::host("proof")
                            .map_err(|error| error.to_string())?,
                        actor: lash_core_execution::ProcessOriginator::host_scoped("proof"),
                        draft: lash_core_execution::TriggerSubscriptionDraft::for_process(
                            format!("proof/{key}"),
                            env_ref.clone(),
                            SOURCE_TYPE,
                            SOURCE_KEY,
                            lash_core_execution::ProcessInput::Engine {
                                kind: KIND.to_owned(),
                                payload: serde_json::json!({ "subscription": key }),
                            },
                            lash_core_execution::ProcessIdentity::labelled(KIND, Some(key)),
                        )
                        .with_payload_schema(lash_core_execution::JsonSchema::any()),
                    },
                )
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
        }
        let mut seed = MailTx::new();
        seed.create_actor(emitter(), FormatSet::new(EMITTER_FORMATS))
            .append(emitter(), MailKind::new("emit"), String::new());
        nodes
            .database()
            .commit_mail(seed, CommitLabel::MAIL_SESSION)
            .await
            .map_err(|error| error.to_string())?;
        // A starts and claims first, B once A is settled.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![emitter()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let emitted = matches!(
            nodes.database().actor(&emitter()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle && snapshot.pending_mail == 0
        );
        if !emitted {
            return false;
        }
        let started = processes(&self.backend()).await;
        if started.len() != SUBSCRIPTIONS.len() {
            return false;
        }
        for process in started {
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

    async fn check(&self, _nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let backend = self.backend();
        let triggers = backend.trigger_store();

        // S1: one occurrence, one delivery per subscription, one process per
        // delivery under its start key, and no other process.
        let occurrences = triggers
            .list_occurrences(lash_core_execution::TriggerOccurrenceFilter::default())
            .await
            .unwrap_or_default();
        let [occurrence] = occurrences.as_slice() else {
            return vec![format!(
                "S1: the store holds {} occurrences",
                occurrences.len()
            )];
        };
        let deliveries = triggers
            .list_deliveries_by_occurrence_id(&occurrence.occurrence_id)
            .await
            .unwrap_or_default();
        let subscriptions: BTreeSet<&str> = deliveries
            .iter()
            .map(|delivery| delivery.subscription.subscription_key.as_str())
            .collect();
        if deliveries.len() != SUBSCRIPTIONS.len() || subscriptions.len() != SUBSCRIPTIONS.len() {
            violations.push(format!(
                "S1: the occurrence holds {} deliveries to {subscriptions:?}",
                deliveries.len()
            ));
        }
        let bound: BTreeSet<_> = deliveries
            .iter()
            .filter_map(|delivery| delivery.process_id().cloned())
            .collect();
        let registered: BTreeSet<_> = processes(&backend)
            .await
            .into_iter()
            .map(|record| record.id)
            .collect();
        if bound.len() != deliveries.len() || registered != bound {
            violations.push(format!(
                "S1: the deliveries are bound to {bound:?}, the registry holds {registered:?}"
            ));
        }
        for delivery in &deliveries {
            let key = lash_core_execution::facade_support::trigger_delivery_start_key(delivery);
            match backend
                .process_registry()
                .get_process_by_start_key(&key)
                .await
            {
                Ok(Some(record)) if Some(&record.id) == delivery.process_id() => {}
                other => violations.push(format!(
                    "S1: the start key of the delivery to `{}` holds {other:?}",
                    delivery.subscription.subscription_key
                )),
            }
        }
        let reports = self.world.reports.lock_recover().clone();
        if reports.is_empty() {
            violations.push(format!(
                "no emission answered; errors: {:?}",
                self.world.errors.lock_recover()
            ));
        }
        for report in &reports {
            let started: BTreeSet<_> = report
                .deliveries
                .iter()
                .filter_map(|delivery| match &delivery.outcome {
                    TriggerDeliveryEmitOutcome::Started { process_id } => Some(process_id.clone()),
                    TriggerDeliveryEmitOutcome::Failed { .. } => None,
                })
                .collect();
            if report.deliveries.len() != SUBSCRIPTIONS.len() || started != bound {
                violations.push(format!("S1: an emission answered {report:?}"));
            }
        }

        // S2: every look found the whole start or none of it, and a cut the
        // store never saw left nothing behind its prepared start.
        let seen = self.world.seen.lock_recover().clone();
        for held in &seen {
            if *held != Held::NOTHING && *held != Held::STARTED {
                violations.push(format!("S2: the emitter saw a partial start {held:?}"));
            }
        }
        let unentered = cut.is_some_and(|cut| {
            cut.point.label == CommitLabel::TRIGGER_START
                && matches!(cut.fault, Fault::FailBefore | Fault::Abort)
        });
        if unentered && seen.get(1) != Some(&Held::NOTHING) {
            violations.push(format!(
                "S2: after a start cut before it entered the store, the emitter saw {:?}",
                seen.get(1)
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
        .labels(&[CommitLabel::TRIGGER_START])
        .horizon(Duration::from_secs(600))
        .run(|| Proof::new(dialect, postgres_url.clone()))
        .await;
    eprintln!("trigger start on {dialect:?}: {} cells", report.cells.len());
    report.assert_held();
    assert!(
        report.labels().contains(&CommitLabel::TRIGGER_START),
        "the matrix never cut trigger.start"
    );
}

/// S1 and S2 on SQLite in memory, at every `trigger.start` cut.
#[tokio::test]
async fn an_occurrence_starts_each_delivery_once_at_every_cut_on_sqlite_memory() {
    prove(Dialect::SqliteMemory, None).await;
}

/// S1 and S2 on a SQLite file, at every `trigger.start` cut.
#[tokio::test]
async fn an_occurrence_starts_each_delivery_once_at_every_cut_on_sqlite_file() {
    prove(Dialect::SqliteFile, None).await;
}

/// S1 and S2 on PostgreSQL, at every `trigger.start` cut.
#[tokio::test]
async fn an_occurrence_starts_each_delivery_once_at_every_cut_on_postgres() {
    let Some(url) = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
    else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}

/// FIG-5236: a held occurrence preserves every delivery's first disposition,
/// including payload refusals alongside a start and when all deliveries refuse.
async fn refusal_receipts_are_stable(dialect: Dialect, postgres_url: Option<String>) {
    let proof = Proof::new(dialect, postgres_url);
    proof.database(SimClock::new()).await;
    let backend = proof.backend();
    let env_ref =
        lash_core_execution::testing::process_execution_env_fixture(&*backend.process_env_store())
            .await;
    let triggers = backend.trigger_store();
    let router = TriggerRouter::new(
        Arc::clone(&triggers),
        lash_core_execution::ProcessWorkWiring::without_process_work(backend.process_registry()),
    )
    .with_process_artifacts(backend.process_env_store(), Proof::engines());
    let cx = ActorContext::detached(backend.clone());
    for (case, starts) in [("all-refused", false), ("mixed", true)] {
        for (key, accepts) in [("first", starts), ("second", false)] {
            let schema = if accepts {
                lash_core_execution::JsonSchema::any()
            } else {
                lash_core_execution::JsonSchema::admit(serde_json::json!({"type": "string"}))
                    .expect("a string payload schema")
            };
            triggers
                .execute_command(
                    &format!("refusal-register-{case}-{key}"),
                    lash_core_execution::TriggerCommand::Register {
                        owner_scope: lash_core_execution::TriggerOwnerScope::host("proof").unwrap(),
                        actor: lash_core_execution::ProcessOriginator::host_scoped("proof"),
                        draft: lash_core_execution::TriggerSubscriptionDraft::for_process(
                            format!("refusal/{case}/{key}"),
                            env_ref.clone(),
                            SOURCE_TYPE,
                            case,
                            lash_core_execution::ProcessInput::Engine {
                                kind: KIND.to_owned(),
                                payload: serde_json::json!({"key": key}),
                            },
                            lash_core_execution::ProcessIdentity::labelled(KIND, Some(key)),
                        )
                        .with_payload_schema(schema),
                    },
                )
                .await
                .unwrap()
                .unwrap();
        }
        let request = TriggerOccurrenceRequest::new(
            SOURCE_TYPE,
            case,
            serde_json::json!({"proof": true}),
            format!("refusal-{case}"),
        );
        let first = router.emit(request.clone(), &cx).await.unwrap();
        assert_eq!(first.deliveries.len(), 2);
        assert_eq!(first.started_process_ids().len(), usize::from(starts));
        for delivery in &first.deliveries {
            if let TriggerDeliveryEmitOutcome::Failed { value_mismatch, .. } = &delivery.outcome {
                assert!(
                    value_mismatch.is_some(),
                    "the typed mismatch survives the receipt"
                );
            }
        }
        let again = router.emit(request.clone(), &cx).await.unwrap();
        assert_eq!(
            again, first,
            "{case}: a held emission preserves every disposition"
        );
        if !starts {
            let recorded_request = TriggerOccurrenceRequest::new(
                SOURCE_TYPE,
                case,
                serde_json::json!({"proof": true}),
                "recorded-all-refused",
            );
            let error = router
                .emit_recorded(recorded_request.clone(), &cx)
                .await
                .unwrap_err();
            let again_error = router
                .emit_recorded(recorded_request, &cx)
                .await
                .unwrap_err();
            assert_eq!(
                serde_json::to_value(again_error).unwrap(),
                serde_json::to_value(error).unwrap(),
                "an all-refused occurrence stays refused"
            );
        }
        let held = triggers
            .list_deliveries_by_occurrence_id(&first.occurrence_id)
            .await
            .unwrap();
        assert_eq!(held.len(), 2, "both dispositions are durable");
    }
    assert_eq!(
        processes(&backend).await.len(),
        1,
        "only the accepted delivery starts"
    );
}

#[tokio::test]
async fn held_emissions_preserve_refusals_on_sqlite_memory() {
    refusal_receipts_are_stable(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn held_emissions_preserve_refusals_on_sqlite_file() {
    refusal_receipts_are_stable(Dialect::SqliteFile, None).await;
}

#[tokio::test]
async fn held_emissions_preserve_refusals_on_postgres() {
    let Some(url) = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
    else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    refusal_receipts_are_stable(Dialect::Postgres, Some(url)).await;
}
