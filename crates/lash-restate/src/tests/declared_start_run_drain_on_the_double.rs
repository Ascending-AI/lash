//! A final's declared process start drains inside its declarations (K5,
//! FIG-4884): the singleton Run route through a real handler on the
//! in-process Restate server double, launching into a real SQLite process
//! registry in SQLite memory.
//!
//! The start's obligation is recorded with its attempt: the body's stable
//! start key, bound by the Run to the Run's environment (the start is an
//! engine process lash executes) and to a consumer hold that carries the
//! call's recorded cancel policy. The declaration
//! record admits it; one eagerly started `start:prepare` run launches and
//! discharges it. D folds that acknowledged outcome before V settles the
//! declarations. A crash drops its attempt and the double replays the handler.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::plugin::{BehaviorRevision, PluginRevision};
use lash_core::runtime::AttemptStream;
use lash_core::store::plugin_writers::PluginCallbackIdentity;
use lash_core::tool_dispatch::{
    BeforeCheckReply, DeclaredStartObligation, DeclaredStartObligationRefusal,
    IsolatedProcessDescriptor, IsolatedStartRefusal, IsolatedToolStart, PhysicalProcessWorker,
    ProcessExecutionBoundary, SingletonAttempt, SingletonBodyOutcome, SingletonCapture,
    SingletonPreparedRequest, SingletonRunError, SingletonTerminal, SingletonToolCall,
    SingletonToolHandlers, WorkerTerminationReceipt,
};
use lash_core::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, CallDecision, DeclarationRefusal,
    ExternalCancelPolicy, PresentationBinding, RunEvent, RunRecord, SegmentOrdinal,
    ToolDeclaration,
};
use lash_core::{
    AdmittedScope, CancelOrigin, EffectOpener, Lifetime, ProcessExecutionEnvRef, ProcessId,
    ProcessInput, ProcessLifecycle as _, ProcessProvenance, ProcessRegistrar as _,
    ProcessRetention as _, ProcessStartRegistration, StartKey, ToolCallId,
};
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use lash_sansio::ToolIntentKind;
use lash_sqlite_store::{SqliteDatabase, SqliteProcessRegistry, SqliteStoreSet};

use super::{SingletonRunOutcome, decide_round, run_singleton};

const PLUGIN: &str = "fig4884-tools";
const OUTPUT: &str = "fig4884 started";
const PRESENTATION: &str = "fig4884 presented";
const ENVIRONMENT: &str = "process-env:fig4884";

fn binding() -> AdmittedBinding {
    let callback = |key: &str| PluginCallbackIdentity {
        owner: PluginRevision::new(PLUGIN, BehaviorRevision::new(1).unwrap()),
        key: key.to_owned(),
    };
    AdmittedBinding {
        executable: callback("tool:start"),
        preparation: callback("tool:start"),
        presentation: PresentationBinding {
            presenter: Some(callback("present:start")),
            steps: Vec::new(),
        },
    }
}

fn call(label: &str, cancel: ExternalCancelPolicy) -> SingletonToolCall {
    SingletonToolCall {
        owner: EffectOpener::turn("session", "turn"),
        segment: SegmentOrdinal(0),
        call_id: ToolCallId::fixture(label),
        tool_name: "start".to_owned(),
        arguments: serde_json::json!({ "label": label }),
        declaration: ToolDeclaration::default().with_intents([ToolIntentKind::StartProcess]),
        binding: binding(),
        available: vec![PluginRevision::new(
            PLUGIN,
            BehaviorRevision::new(1).unwrap(),
        )],
        cancel,
        environment: Some(ProcessExecutionEnvRef::new(ENVIRONMENT)),
    }
}

fn start_key(label: &str) -> StartKey {
    StartKey::for_host(format!("fig4884-{label}"))
}

fn declaring(key: Option<StartKey>) -> SingletonBodyOutcome {
    SingletonBodyOutcome::Done {
        commands: Default::default(),
        output: OUTPUT.to_owned(),
        intents: Vec::new(),
        start: Some(Box::new(
            ProcessStartRegistration::of_target(
                ProcessInput::Engine {
                    kind: "fig4884-index".to_owned(),
                    payload: serde_json::json!({ "job": "fig4884" }),
                },
                ProcessProvenance::host(),
                Lifetime::Detached,
            )
            .with_start_key(key),
        )),
    }
}

/// When the Run's cancellation is requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelAt {
    Never,
    Preparation,
    /// Inside the body: before the decision, so before the start's admission.
    Body,
    /// Inside the launch: after the start's admission.
    Launch,
    /// Inside the presentation: after the start's discharge.
    Presentation,
}

/// The tool, whose start launches into a real process registry.
struct Starter {
    registry: Arc<SqliteProcessRegistry>,
    body: SingletonBodyOutcome,
    cancel_at: CancelAt,
    cancel: AtomicBool,
    executions: AtomicUsize,
    launches: Mutex<Vec<ProcessId>>,
    discharges: Mutex<Vec<(ProcessId, bool)>>,
    engines: Option<lash_core::ProcessEngineRegistry>,
    isolation: Option<ProcessExecutionBoundary>,
    worker: Option<Arc<IsolatedEngine>>,
    slow: bool,
    materials: std::sync::OnceLock<Arc<dyn lash_core::store::ToolMaterialStore>>,
    ingress: std::sync::OnceLock<crate::RestateIngressClient>,
    launch_workflow: bool,
}

impl Starter {
    fn new(
        registry: Arc<SqliteProcessRegistry>,
        body: SingletonBodyOutcome,
        cancel_at: CancelAt,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            body,
            cancel_at,
            cancel: AtomicBool::new(false),
            executions: AtomicUsize::new(0),
            launches: Mutex::new(Vec::new()),
            discharges: Mutex::new(Vec::new()),
            engines: None,
            isolation: None,
            worker: None,
            slow: false,
            materials: Default::default(),
            ingress: Default::default(),
            launch_workflow: false,
        })
    }

    fn launches(&self) -> Vec<ProcessId> {
        self.launches.lock().unwrap().clone()
    }

    fn discharges(&self) -> Vec<(ProcessId, bool)> {
        self.discharges.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl SingletonToolHandlers for Starter {
    fn tool_material_store(&self) -> Option<&dyn lash_core::store::ToolMaterialStore> {
        self.materials.get().map(AsRef::as_ref)
    }

    async fn attach_start_terminal(
        &self,
        descriptor: &lash_core::tool_run::SourceDescriptor,
        process_id: &ProcessId,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        let ingress = self
            .ingress
            .get()
            .ok_or(lash_core::tool_run::SourceRefusal::NotArmed)?;
        let subscription = crate::ProcessTerminalSubscription::for_source(descriptor.clone())?;
        let address =
            crate::durable_wait::RestateDurableWaitAddress::for_key(&subscription.receiver);
        ingress
            .call_lash_object::<_, bool>(
                "LashDurableWaitIndex",
                &address.index_key(),
                "attach_process_terminal",
                &subscription,
            )
            .await
            .map_err(|error| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::EngineProcessAwait,
                    error.to_string(),
                )
            })?;
        let source = crate::RestateDurableWaitAddress::for_key(&subscription.terminal);
        let output: Option<lash_core::ProcessAwaitOutput> = ingress
            .call_lash_object(
                "LashDurableWaitIndex",
                &source.index_key(),
                "subscribe_process_terminal",
                &subscription,
            )
            .await
            .map_err(|error| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::EngineProcessAwait,
                    error.to_string(),
                )
            })?;
        if let Some(output) = output {
            ingress
                .call_lash_object::<_, ()>(
                    "LashDurableWaitIndex",
                    &address.index_key(),
                    "deliver_process_terminal",
                    &crate::ProcessTerminalDelivery {
                        subscription,
                        output,
                    },
                )
                .await
                .map_err(|error| {
                    lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::EngineProcessAwait,
                        error.to_string(),
                    )
                })?;
        }
        if self.launch_workflow {
            return Ok(());
        }
        let output = lash_core::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::success(serde_json::json!(OUTPUT)),
        );
        // Process terminals can arrive before the Run subscribes. First write wins.
        for value in [
            output,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("late result must not replace the terminal"),
            )),
        ] {
            ingress
                .call_lash_workflow::<_, ()>(
                    "LashProcessWorkflow",
                    process_id.as_str(),
                    "complete_terminal",
                    &crate::RestateProcessCompleteRequest {
                        process_id: process_id.clone(),
                        output: value,
                    },
                )
                .await
                .map_err(|error| {
                    lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::EngineProcessAwait,
                        error.to_string(),
                    )
                })?;
        }
        Ok(())
    }

    fn process_engines(&self) -> Option<&lash_core::ProcessEngineRegistry> {
        self.engines.as_ref()
    }

    fn isolated_start(&self, call: &SingletonToolCall) -> Option<IsolatedToolStart> {
        let boundary = self.isolation?;
        let label = call.arguments["label"].as_str().unwrap();
        let registration = ProcessStartRegistration::of_target(
            ProcessInput::Engine {
                kind: self
                    .worker
                    .as_ref()
                    .map_or("fig4884-index", |engine| engine.kind)
                    .to_owned(),
                payload: serde_json::json!({"label": label}),
            },
            ProcessProvenance::host(),
            Lifetime::Detached,
        )
        .with_start_key(Some(start_key(label)));
        Some(IsolatedToolStart {
            boundary,
            registration,
        })
    }

    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        if self.cancel_at == CancelAt::Preparation {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(call.arguments.clone())
    }

    async fn before_checks(
        &self,
        _call: &SingletonToolCall,
        _request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(Vec::new())
    }

    async fn execute(
        &self,
        _attempt: SingletonAttempt<'_>,
    ) -> Result<SingletonBodyOutcome, String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        if self.slow {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if self.cancel_at == CancelAt::Body {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(self.body.clone())
    }

    async fn after_checks(
        &self,
        _call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }

    async fn run_cancel_requested(&self) -> Result<bool, String> {
        Ok(self.cancel.load(Ordering::SeqCst))
    }

    async fn cancel_call(
        &self,
        _call_id: &ToolCallId,
        _source: Option<&lash_core::AwaitEventKey>,
    ) -> Result<(), String> {
        // This fixture's external work is its declared start. Its recorded
        // obligation cancels the process and releases the hold in discharge_start.
        Ok(())
    }

    async fn present(
        &self,
        call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Result<String, lash_core::tool_dispatch::SingletonPresentationError> {
        assert_eq!(
            self.launches.lock().unwrap().is_empty(),
            self.discharges.lock().unwrap().is_empty(),
            "{call_id}'s launched start is discharged before its presentation"
        );
        if self.cancel_at == CancelAt::Presentation {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(PRESENTATION.to_owned())
    }

    fn emit_stream(&self, _call_id: &ToolCallId, _stream: &AttemptStream) {}

    async fn launch_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<lash_core::ProcessHandleView, String> {
        if self.cancel_at == CancelAt::Launch {
            self.cancel.store(true, Ordering::SeqCst);
        }
        let registration = obligation
            .registration
            .clone()
            .stating_input()
            .map_err(|_| "the start names a definition".to_owned())?;
        let record = self
            .registry
            .register_process(registration.clone())
            .await
            .map_err(|error| error.to_string())?;
        if let Some(engine) = &self.worker {
            engine.launch(&record.id);
        }
        if self.launch_workflow {
            let _: crate::RestateProcessWorkflowOutput = self
                .ingress
                .get()
                .ok_or("the child start has no ingress")?
                .call_lash_workflow(
                    "LashProcessWorkflow",
                    record.id.as_str(),
                    "run",
                    &crate::RestateProcessWorkflowPayload::from(
                        crate::RestateProcessWorkflowInput {
                            process_id: record.id.clone(),
                            registration,
                            execution_context: Default::default(),
                            segment_ordinal: 0,
                            sender_generation: super::test_build_generation(),
                        },
                    ),
                )
                .await
                .map_err(|error| error.to_string())?;
        }
        self.launches.lock().unwrap().push(record.id.clone());
        Ok(lash_core::ProcessHandleView::from_record(record))
    }

    async fn discharge_start(
        &self,
        obligation: &DeclaredStartObligation,
        process_id: &ProcessId,
        cancel: bool,
    ) -> Result<(), String> {
        if cancel {
            self.registry
                .request_process_cancel(
                    process_id,
                    CancelOrigin::TurnStopped,
                    format!("fig4884:{}", obligation.call_id),
                    None,
                )
                .await
                .map_err(|error| error.to_string())?;
        }
        if cancel && self.isolation == Some(ProcessExecutionBoundary::WorkerProcess) {
            assert!(
                self.worker.as_ref().unwrap().receipt(process_id).is_some(),
                "termination precedes hold release"
            );
        }
        let hold = obligation
            .registration
            .consumer_hold
            .as_ref()
            .ok_or("a declared start carries its hold")?;
        self.registry
            .release_consumer_hold(process_id, &hold.key)
            .await
            .map_err(|error| error.to_string())?;
        self.discharges
            .lock()
            .unwrap()
            .push((process_id.clone(), cancel));
        Ok(())
    }
}

/// One registry row as the law reads it back.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    process_id: String,
    start_key: Option<String>,
    environment: Option<String>,
    hold: Option<String>,
    cancel_requested: bool,
}

impl Row {
    /// The row a drained start leaves: one process under its key, run under
    /// the Run's environment, its hold released.
    fn drained(process: &ProcessId, key: &StartKey, cancel_requested: bool) -> Self {
        Self {
            process_id: process.to_string(),
            start_key: Some(key.to_string()),
            environment: Some(ENVIRONMENT.to_owned()),
            hold: None,
            cancel_requested,
        }
    }
}

struct Stores {
    set: SqliteStoreSet,
}

impl Stores {
    async fn open() -> Self {
        Self {
            set: SqliteStoreSet::memory().await.unwrap(),
        }
    }

    /// Every process row in the law's SQLite registry.
    async fn rows(self) -> Vec<Row> {
        let set = self.set;
        let connection =
            rusqlite::Connection::open(set.database_uri(SqliteDatabase::ProcessRegistry)).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT process_id, start_key, record_json, consumer_hold_key,
                        cancel_requested_at_ms
                 FROM processes ORDER BY process_id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                let record: serde_json::Value =
                    serde_json::from_str(&row.get::<_, String>(2)?).unwrap();
                Ok(Row {
                    process_id: row.get(0)?,
                    start_key: row.get(1)?,
                    environment: record["env_ref"].as_str().map(str::to_owned),
                    hold: row.get(3)?,
                    cancel_requested: row.get::<_, Option<i64>>(4)?.is_some(),
                })
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

type Returned = Arc<Mutex<Vec<Result<SingletonRunOutcome, SingletonRunError>>>>;

struct Driven {
    backend: RestateTestBackend,
    returned: Returned,
}

impl Driven {
    fn finished(&self) -> (SingletonTerminal, Vec<RunRecord>) {
        let outcome = self
            .returned
            .lock()
            .unwrap()
            .pop()
            .expect("the handler finished")
            .expect("the singleton finished");
        (outcome.terminal, outcome.records)
    }
}

async fn drive(crash: Option<&str>, call: SingletonToolCall, starter: Arc<Starter>) -> Driven {
    drive_with_replay(crash, call, starter, None).await
}

async fn drive_with_replay(
    crash: Option<&str>,
    call: SingletonToolCall,
    starter: Arc<Starter>,
    replay: Option<Arc<Starter>>,
) -> Driven {
    drive_with_config(crash, call, starter, replay, ServerConfig::default()).await
}

async fn drive_with_config(
    crash: Option<&str>,
    call: SingletonToolCall,
    starter: Arc<Starter>,
    replay: Option<Arc<Starter>>,
    config: ServerConfig,
) -> Driven {
    let backend = lash_restate_test::backend(0x4884, config).await.unwrap();
    starter
        .materials
        .set(backend.engine_stores().tool_material_store())
        .ok();
    let ingress = crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
        backend.server().ingress_url(),
        backend.server().transport(),
    ));
    starter.ingress.set(ingress.clone()).ok();
    if let Some(replay) = &replay {
        replay
            .materials
            .set(backend.engine_stores().tool_material_store())
            .ok();
        replay.ingress.set(ingress).ok();
    }
    if let Some(step) = crash {
        let name = step_name(&call.call_id, step);
        let point = if replay.is_some() {
            // Binding drift is judged before a fresh attempt. Its command
            // must not already occupy the positional journal's next slot.
            CrashPoint::BeforeRun { name }
        } else {
            CrashPoint::BeforeRunResult { name: Some(name) }
        };
        backend.server().crash_on(CrashRule::new(point));
    }
    let returned: Returned = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let returned = Arc::clone(&returned);
        let attempts = AtomicUsize::new(0);
        Arc::new(move |scoped| {
            let call = call.clone();
            let starter = if attempts.fetch_add(1, Ordering::SeqCst) > 0 {
                Arc::clone(replay.as_ref().unwrap_or(&starter))
            } else {
                Arc::clone(&starter)
            };
            let returned = Arc::clone(&returned);
            Box::pin(async move {
                let outcome = if matches!(starter.body, SingletonBodyOutcome::DeferredStart { .. })
                {
                    async {
                        let mut run = lash_core::tool_dispatch::RunCoordinator::open(
                            &scoped,
                            call.owner.clone(),
                            call.segment,
                            call.available.clone(),
                        );
                        decide_round(
                            &mut run,
                            std::slice::from_ref(&call),
                            Arc::clone(&starter) as Arc<dyn SingletonToolHandlers>,
                            Default::default(),
                        )
                        .await?;
                        run.await_deferred().await?;
                        let terminal = run
                            .drain()
                            .await?
                            .pop()
                            .ok_or_else(|| {
                                lash_core::RuntimeEffectControllerError::new(
                                    lash_core::RuntimeErrorCode::EngineProcessAwait,
                                    "the Deferred call has no final",
                                )
                            })?
                            .1;
                        Ok::<_, SingletonRunError>(SingletonRunOutcome {
                            terminal,
                            records: run.into_records(),
                        })
                    }
                    .await
                } else {
                    run_singleton(
                        &scoped,
                        &call,
                        Arc::clone(&starter) as Arc<dyn SingletonToolHandlers>,
                    )
                    .await
                };
                returned.lock().unwrap().push(outcome);
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    backend.server().settle().await;
    Driven { backend, returned }
}

/// A step's journal name: `schedule:N` is the Run's schedule record (a
/// one-member round records its decision in the one at ordinal 1, and
/// every V is one too); any other step is the call's own record.
fn step_name(call_id: &ToolCallId, step: &str) -> String {
    match step.strip_prefix("schedule:") {
        Some(ordinal) => {
            crate::controller::record_journal_name(format!("lash:run:schedule:{ordinal}"))
        }
        None => match step {
            "decide" => crate::controller::record_journal_name("lash:run:schedule:1".to_owned()),
            _ => crate::controller::call_step_journal_name(call_id, step),
        },
    }
}

/// The start events of the records, in order.
fn start_events(records: &[RunRecord]) -> Vec<RunEvent> {
    records
        .iter()
        .flat_map(|record| &record.events)
        .filter(|event| {
            matches!(
                event,
                RunEvent::StartAdmitted { .. }
                    | RunEvent::StartLaunched { .. }
                    | RunEvent::StartDischarged { .. }
            )
        })
        .cloned()
        .collect()
}

fn drained(
    call_id: &ToolCallId,
    key: &StartKey,
    process: &ProcessId,
    cancelled: bool,
) -> Vec<RunEvent> {
    vec![
        RunEvent::StartAdmitted {
            call_id: call_id.clone(),
            start_key: key.clone(),
        },
        RunEvent::StartLaunched {
            call_id: call_id.clone(),
            start_key: key.clone(),
            process_id: process.clone(),
            receipt: None,
        },
        RunEvent::StartDischarged {
            call_id: call_id.clone(),
            start_key: key.clone(),
            cancelled,
        },
    ]
}

struct IsolatedEngine {
    kind: &'static str,
    physical: bool,
    workers: Mutex<std::collections::BTreeMap<ProcessId, Worker>>,
    spawned: AtomicUsize,
}

enum Worker {
    Running(std::process::Child),
    Reaped(WorkerTerminationReceipt),
}

impl IsolatedEngine {
    fn new(kind: &'static str, physical: bool) -> Arc<Self> {
        Arc::new(Self {
            kind,
            physical,
            workers: Mutex::new(Default::default()),
            spawned: AtomicUsize::new(0),
        })
    }

    fn launch(&self, process: &ProcessId) {
        if !self.physical {
            return;
        }
        let mut workers = self.workers.lock().unwrap();
        if workers.contains_key(process) {
            return;
        }
        let child = lash_conformance::spawn_isolation_law_worker().unwrap();
        assert_ne!(
            child.id(),
            std::process::id(),
            "the body has a physical boundary"
        );
        workers.insert(process.clone(), Worker::Running(child));
        self.spawned.fetch_add(1, Ordering::SeqCst);
    }

    fn receipt(&self, process: &ProcessId) -> Option<WorkerTerminationReceipt> {
        match self.workers.lock().unwrap().get(process) {
            Some(Worker::Reaped(receipt)) => Some(receipt.clone()),
            _ => None,
        }
    }
}

impl Drop for IsolatedEngine {
    fn drop(&mut self) {
        for worker in self.workers.get_mut().unwrap().values_mut() {
            if let Worker::Running(child) = worker {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[async_trait::async_trait]
impl PhysicalProcessWorker for IsolatedEngine {
    async fn terminate_worker(
        &self,
        process: &ProcessId,
    ) -> Result<WorkerTerminationReceipt, lash_core::PluginError> {
        let mut workers = self.workers.lock().unwrap();
        let worker = workers.get_mut(process).unwrap();
        match worker {
            Worker::Reaped(receipt) => Ok(receipt.clone()),
            Worker::Running(child) => {
                let pid = std::num::NonZeroU32::new(child.id()).unwrap();
                child.kill().unwrap();
                let status = child.wait().unwrap();
                assert!(!status.success(), "cancellation terminated the worker");
                assert_eq!(
                    child.try_wait().unwrap(),
                    Some(status),
                    "the worker was reaped"
                );
                let receipt = WorkerTerminationReceipt {
                    process_id: process.clone(),
                    worker_pid: pid,
                };
                *worker = Worker::Reaped(receipt.clone());
                Ok(receipt)
            }
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for IsolatedEngine {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn physical_worker(&self) -> Option<&dyn PhysicalProcessWorker> {
        self.physical.then_some(self)
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        unreachable!("the law's launch callback owns process delivery")
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

fn isolated_call(label: &str, policy: ExternalCancelPolicy) -> SingletonToolCall {
    let mut call = call(label, policy);
    call.declaration = ToolDeclaration {
        isolated: true,
        ..ToolDeclaration::default()
    };
    call
}

fn isolated_starter(
    stores: &Stores,
    physical: bool,
    cancel: CancelAt,
    kind: &'static str,
) -> Arc<Starter> {
    let mut starter = Starter::new(stores.set.process_registry(), declaring(None), cancel);
    let engine = IsolatedEngine::new(kind, physical);
    let owned = Arc::get_mut(&mut starter).unwrap();
    owned.engines = Some(lash_core::ProcessEngineRegistry::new().with_registration(
        lash_core::ProcessEngineRegistration::accepting(engine.clone()),
    ));
    owned.worker = Some(engine);
    owned.isolation = Some(if physical {
        ProcessExecutionBoundary::WorkerProcess
    } else {
        ProcessExecutionBoundary::Invocation
    });
    starter
}

/// L08 and L12: unsupported isolation and unavailable recorded implementations
/// refuse before a body or new route. A cooperative engine cannot claim a
/// physical worker. A replacement live binding cannot replace a recorded one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_or_changed_isolation_refuses_before_a_body_or_new_identity() {
    let stores = Stores::open().await;
    for kind in ["missing", "physical-claim", "unavailable", "revision"] {
        let mut call = isolated_call(kind, ExternalCancelPolicy::Ignore);
        if kind == "revision" {
            call.available.clear();
        }
        let mut starter = isolated_starter(&stores, false, CancelAt::Never, "fig4884-index");
        let owned = Arc::get_mut(&mut starter).unwrap();
        match kind {
            "missing" => owned.isolation = None,
            "physical-claim" => owned.isolation = Some(ProcessExecutionBoundary::WorkerProcess),
            "unavailable" => owned.engines = None,
            _ => {}
        }
        let driven = drive(None, call, Arc::clone(&starter)).await;
        let refused = driven.returned.lock().unwrap().pop().unwrap().unwrap_err();
        if kind == "revision" {
            assert!(matches!(
                refused,
                SingletonRunError::Admission(
                    lash_core::tool_run::AdmissionRefusal::BindingUnavailable { .. }
                )
            ));
            assert_eq!(starter.executions.load(Ordering::SeqCst), 0);
            assert!(starter.launches().is_empty());
            continue;
        }
        match (kind, refused) {
            (
                "missing",
                SingletonRunError::Admission(
                    lash_core::tool_run::AdmissionRefusal::UnsupportedIsolation { member: 0 },
                ),
            )
            | (
                "physical-claim",
                SingletonRunError::Isolation(IsolatedStartRefusal::Boundary { .. }),
            )
            | (
                "unavailable",
                SingletonRunError::Isolation(IsolatedStartRefusal::Unavailable { .. }),
            ) => {}
            (_, error) => panic!("{kind}: {error:?}"),
        }
        assert_eq!(starter.executions.load(Ordering::SeqCst), 0);
        assert!(starter.launches().is_empty());
    }
    let original = isolated_starter(&stores, false, CancelAt::Never, "fig4884-index");
    let replacement = isolated_starter(&stores, false, CancelAt::Never, "replacement-engine");
    let driven = drive_with_replay(
        Some("attempt:1"),
        isolated_call("drift", ExternalCancelPolicy::Ignore),
        Arc::clone(&original),
        Some(Arc::clone(&replacement)),
    )
    .await;
    let refused = driven.returned.lock().unwrap().pop().unwrap().unwrap_err();
    assert!(
        matches!(refused, SingletonRunError::Isolation(IsolatedStartRefusal::Unavailable { kind }) if kind == "fig4884-index")
    );
    for starter in [&original, &replacement] {
        assert_eq!(starter.executions.load(Ordering::SeqCst), 0);
        assert!(starter.launches().is_empty());
    }
    assert!(stores.rows().await.is_empty());
}

/// Q3 and D04: ordinary work remains cooperative. A slow success or a
/// body-owned transport timeout stays that one ordinary attempt even when
/// the provider also has a registered process implementation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_or_timed_out_ordinary_work_is_never_rerun_as_a_process() {
    for body in [
        SingletonBodyOutcome::Failed {
            output: "transport timed out".to_owned(),
        },
        SingletonBodyOutcome::Done {
            commands: Default::default(),
            output: OUTPUT.to_owned(),
            intents: Vec::new(),
            start: None,
        },
    ] {
        let stores = Stores::open().await;
        let mut starter = isolated_starter(&stores, false, CancelAt::Never, "fig4884-index");
        let owned = Arc::get_mut(&mut starter).unwrap();
        owned.body = body;
        owned.slow = true;
        let mut call = call("ordinary", ExternalCancelPolicy::Ignore);
        call.declaration = ToolDeclaration::default();
        let driven = drive(None, call, Arc::clone(&starter)).await;
        let (terminal, _) = driven.finished();
        assert!(matches!(
            terminal,
            SingletonTerminal::Final { launched: None, .. }
        ));
        assert_eq!(starter.executions.load(Ordering::SeqCst), 1);
        assert!(starter.launches().is_empty());
        assert!(stores.rows().await.is_empty());
    }
}

/// L07/L08: a lost launch result recovers one start and consumes its immutable K4 terminal.
#[tokio::test]
async fn a_deferred_start_replays_one_identity_and_consumes_its_terminal() {
    for (cancel_at, policy, cut) in [
        (
            CancelAt::Never,
            ExternalCancelPolicy::Ignore,
            Some("start:launch"),
        ),
        (
            CancelAt::Body,
            ExternalCancelPolicy::CancelExternalWork,
            None,
        ),
        (CancelAt::Launch, ExternalCancelPolicy::Ignore, None),
        (
            CancelAt::Launch,
            ExternalCancelPolicy::CancelExternalWork,
            None,
        ),
    ] {
        let stores = Stores::open().await;
        let mut call = call("deferred", policy);
        call.declaration =
            ToolDeclaration::deferring().with_intents([ToolIntentKind::StartProcess]);
        let SingletonBodyOutcome::Done {
            start: Some(start), ..
        } = declaring(Some(start_key("deferred")))
        else {
            panic!("the fixture declares one start");
        };
        let starter = Starter::new(
            stores.set.process_registry(),
            SingletonBodyOutcome::DeferredStart { start },
            cancel_at,
        );
        let driven = drive(cut, call.clone(), Arc::clone(&starter)).await;
        let (terminal, records) = driven.finished();
        if cancel_at == CancelAt::Body {
            assert_eq!(
                terminal,
                SingletonTerminal::Withheld {
                    decision: CallDecision::Cancelled
                }
            );
            assert!(
                starter.launches().is_empty(),
                "pre-admission cancel forbids launch"
            );
            assert!(starter.discharges().is_empty());
            assert!(stores.rows().await.is_empty());
            assert!(start_events(&records).is_empty());
            continue;
        }
        let SingletonTerminal::Final {
            source: lash_core::tool_run::ResultSource::DeferredCompletion { .. },
            capture,
            ..
        } = terminal
        else {
            panic!("the process source must supply the final result: {terminal:?}");
        };
        let output: lash_core::ProcessAwaitOutput =
            serde_json::from_str(capture.output().unwrap()).unwrap();
        assert_eq!(
            output,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!(OUTPUT)
            ))
        );
        assert_eq!(starter.executions.load(Ordering::SeqCst), 1);
        let launches = starter.launches();
        assert_eq!(
            launches.len(),
            1 + usize::from(cut.is_some()),
            "only an unacknowledged launch is retried"
        );
        assert_eq!(
            launches[0],
            *launches.last().unwrap(),
            "StartKey recovers the same minted ProcessId"
        );
        let cancelled =
            cancel_at == CancelAt::Launch && policy == ExternalCancelPolicy::CancelExternalWork;
        assert_eq!(starter.discharges(), vec![(launches[0].clone(), cancelled)]);
        assert_eq!(
            stores.rows().await,
            vec![Row::drained(
                &launches[0],
                &start_key("deferred"),
                cancelled
            )]
        );
        let events: Vec<_> = records.iter().flat_map(|record| &record.events).collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::StartAdmitted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::StartLaunched { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::StartDischarged { .. }))
                .count(),
            1
        );
        assert!(
            driven
                .backend
                .server()
                .invocations()
                .iter()
                .all(|view| !view.target.ends_with("/await_terminal")),
            "no process waiter invocation survives"
        );
    }
}
mod process_transfer;
mod terminal_failure;

/// R2(b)/K5: a presentation fault after prepare's ACK cannot re-read the
/// live cancellation gate or re-run the acknowledged start on cold replay.
#[tokio::test]
async fn l08_acknowledged_prepare_keeps_its_discharge_when_presentation_redelivers() {
    let stores = Stores::open().await;
    let call = call(
        "prepared-before-v",
        ExternalCancelPolicy::CancelExternalWork,
    );
    let key = StartKey::for_host("prepared-before-v");
    let starter = Starter::new(
        stores.set.process_registry(),
        declaring(Some(key.clone())),
        CancelAt::Presentation,
    );
    let driven = drive_with_config(
        Some("present"),
        call.clone(),
        Arc::clone(&starter),
        None,
        ServerConfig::default().always_replay(true),
    )
    .await;
    let (terminal, records) = driven.finished();
    let SingletonTerminal::Final {
        launched: Some(process),
        ..
    } = terminal
    else {
        panic!("the protected final must drain");
    };
    assert!(
        starter.discharges().iter().all(|(_, cancelled)| !cancelled),
        "an acknowledged preparation never re-decides discharge from the later cancel"
    );
    assert_eq!(
        starter.launches(),
        vec![process.clone()],
        "an acknowledged start is served instead of re-launched"
    );
    assert_eq!(
        start_events(&records),
        drained(&call.call_id, &key, &process, false)
    );
}

/// R2(b), item 9: a durable deferred launch is VM-owned rather than an
/// owner-channel carrier; always-replay never asks its registrar again.
#[tokio::test]
async fn l08_deferred_launch_ack_is_served_without_an_owner_channel() {
    let stores = Stores::open().await;
    let mut call = call("deferred-owned-launch", ExternalCancelPolicy::Ignore);
    call.declaration = ToolDeclaration::deferring().with_intents([ToolIntentKind::StartProcess]);
    let SingletonBodyOutcome::Done {
        start: Some(start), ..
    } = declaring(Some(start_key("deferred-owned-launch")))
    else {
        panic!("one start");
    };
    let starter = Starter::new(
        stores.set.process_registry(),
        SingletonBodyOutcome::DeferredStart { start },
        CancelAt::Never,
    );
    let driven = drive_with_config(
        None,
        call,
        Arc::clone(&starter),
        None,
        ServerConfig::default().always_replay(true),
    )
    .await;
    let (terminal, _) = driven.finished();
    assert!(matches!(terminal, SingletonTerminal::Final { .. }));
    assert_eq!(
        starter.launches().len(),
        1,
        "the VM serves an acknowledged launch without polling an owner channel"
    );
}
/// L08: the preparation gate follows the recorded cancel policy and a
/// pre-admission cancellation cannot create an obligation.
#[tokio::test]
async fn l08_prepare_applies_cancel_policy_only_to_an_admitted_start() {
    for (cancel_at, policy, cancelled) in [
        (
            CancelAt::Body,
            ExternalCancelPolicy::CancelExternalWork,
            false,
        ),
        (
            CancelAt::Launch,
            ExternalCancelPolicy::CancelExternalWork,
            true,
        ),
        (CancelAt::Launch, ExternalCancelPolicy::Ignore, false),
    ] {
        let stores = Stores::open().await;
        let call = call("prepare-policy", policy);
        let key = start_key("prepare-policy");
        let starter = Starter::new(
            stores.set.process_registry(),
            declaring(Some(key.clone())),
            cancel_at,
        );
        let driven = drive_with_config(
            None,
            call.clone(),
            Arc::clone(&starter),
            None,
            ServerConfig::default().always_replay(true),
        )
        .await;
        let (terminal, records) = driven.finished();
        if cancel_at == CancelAt::Body {
            assert_eq!(
                terminal,
                SingletonTerminal::Withheld {
                    decision: CallDecision::Cancelled
                }
            );
            assert!(starter.launches().is_empty());
            assert!(start_events(&records).is_empty());
            assert!(stores.rows().await.is_empty());
        } else {
            let SingletonTerminal::Final {
                launched: Some(process),
                ..
            } = terminal
            else {
                panic!("admitted start drains");
            };
            assert_eq!(starter.launches(), vec![process.clone()]);
            assert_eq!(starter.discharges(), vec![(process.clone(), cancelled)]);
            assert_eq!(
                start_events(&records),
                drained(&call.call_id, &key, &process, cancelled)
            );
            assert_eq!(
                stores.rows().await,
                vec![Row::drained(&process, &key, cancelled)]
            );
        }
    }
}

/// L08: an isolated prepare owns the physical receipt and never enters the
/// ordinary body, including an acknowledged result replay.
#[tokio::test]
async fn l08_isolated_prepare_retains_the_terminated_worker_receipt() {
    let stores = Stores::open().await;
    let call = isolated_call("physical-prepare", ExternalCancelPolicy::CancelExternalWork);
    let starter = isolated_starter(&stores, true, CancelAt::Launch, "fig4884-index");
    let driven = drive_with_config(
        Some("present"),
        call.clone(),
        Arc::clone(&starter),
        None,
        ServerConfig::default().always_replay(true),
    )
    .await;
    let (terminal, records) = driven.finished();
    let SingletonTerminal::Final {
        launched: Some(process),
        presentation,
        ..
    } = terminal
    else {
        panic!("physical start drains");
    };
    let descriptor: IsolatedProcessDescriptor = serde_json::from_str(&presentation).unwrap();
    let engine = starter.worker.as_ref().unwrap();
    assert_eq!(starter.executions.load(Ordering::SeqCst), 0);
    assert_eq!(engine.spawned.load(Ordering::SeqCst), 1);
    assert_eq!(descriptor.process_id, process);
    assert_eq!(descriptor.termination, engine.receipt(&process));
    assert!(descriptor.termination.is_some());
    assert_eq!(
        start_events(&records),
        drained(
            &call.call_id,
            &start_key("physical-prepare"),
            &process,
            true
        )
    );
}

/// R3/V1: a recorded body watches the deployment gate through ingress. It
/// resolves while the owner's step token is held and adds no handler command.
#[tokio::test]
async fn l02_gate_watch_is_non_journaling_during_an_owner_step() {
    let backend = lash_restate_test::backend(500903, ServerConfig::default().always_replay(true))
        .await
        .unwrap();
    let host = backend.lash_backend().effect_host();
    let control = Arc::new(
        lash_core::runtime::turn_control::ActiveTurnControl::new(
            host.as_ref(),
            lash_core::runtime::TurnAddress::new("watch-s", "watch-t"),
        )
        .await
        .unwrap(),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let attempt: lash_restate_test::HandlerAttempt = {
        let host = Arc::clone(&host);
        let control = Arc::clone(&control);
        let entered = Arc::clone(&entered);
        Arc::new(move |scoped| {
            let host = Arc::clone(&host);
            let control = Arc::clone(&control);
            let entered = Arc::clone(&entered);
            Box::pin(async move {
                scoped.admit_journal_write().unwrap();
                let step = scoped.controller().record_run_record(
                    "guard-watch".to_owned(),
                    Box::pin(async move {
                        control
                            .run_recorded_step_body(&host, false, |stop| async move {
                                entered.notify_one();
                                stop.cancelled().await;
                            })
                            .await;
                        Ok(lash_core::tool_run::RunJournalEntry {
                            state: Vec::new(),
                            materials: Vec::new(),
                            record: RunRecord {
                                segment: SegmentOrdinal(0),
                                first: lash_core::tool_run::RunEventOrdinal(0),
                                events: Vec::new(),
                                trace: None,
                            },
                        })
                    }),
                );
                scoped
                    .await_owner_step("guard-watch".to_owned(), step)
                    .await
                    .unwrap();
            })
        })
    };
    let request = async {
        entered.notified().await;
        control
            .request_local_stop(host.as_ref(), lash_sansio::TurnCancelMode::Immediate, None)
            .await
            .unwrap();
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            backend.run_in_handler(AdmittedScope::turn("watch-s", "watch-t"), attempt),
            request
        )
        .0
        .unwrap();
    })
    .await
    .expect("ingress watch resolves an owner-awaited step");
    let journal = backend
        .server()
        .invocations()
        .iter()
        .filter(|view| view.target.starts_with("LashTestHandlerHost/"))
        .find_map(|view| {
            let journal = backend.server().journal(&view.id).unwrap();
            journal
                .iter()
                .any(|entry| entry.run_completion().is_some())
                .then_some(journal)
        })
        .expect("handler journal");
    assert_eq!(
        journal
            .iter()
            .filter(|entry| entry.ty.is_command()
                && !matches!(
                    entry.ty,
                    lash_restate_test::protocol::MessageType::InputCommand
                        | lash_restate_test::protocol::MessageType::OutputCommand
                ))
            .count(),
        1,
        "watching the gate adds no command to the owner's journal"
    );
}

/// L08/L12: invalid obligations cannot reach the prepare bridge.
#[tokio::test]
async fn l08_invalid_start_obligations_never_issue_preparation() {
    let mut no_env = call("no-env", ExternalCancelPolicy::Ignore);
    no_env.environment = None;
    let mut undeclared = call("undeclared", ExternalCancelPolicy::Ignore);
    undeclared.declaration = ToolDeclaration::default();
    for (call, key, expected) in [
        (
            call("keyless", ExternalCancelPolicy::Ignore),
            None,
            SingletonCapture::StartRefused {
                refusal: DeclaredStartObligationRefusal::Keyless,
            },
        ),
        (
            no_env,
            Some(start_key("no-env")),
            SingletonCapture::StartRefused {
                refusal: DeclaredStartObligationRefusal::NoEnvironment,
            },
        ),
        (
            undeclared,
            Some(start_key("undeclared")),
            SingletonCapture::Refused {
                refusal: DeclarationRefusal::UndeclaredIntent {
                    kind: ToolIntentKind::StartProcess,
                },
            },
        ),
    ] {
        let stores = Stores::open().await;
        let starter = Starter::new(
            stores.set.process_registry(),
            declaring(key),
            CancelAt::Never,
        );
        let driven = drive_with_config(
            None,
            call,
            Arc::clone(&starter),
            None,
            ServerConfig::default().always_replay(true),
        )
        .await;
        let (terminal, records) = driven.finished();
        let SingletonTerminal::Final {
            capture,
            launched: None,
            ..
        } = terminal
        else {
            panic!("typed refusal final");
        };
        assert_eq!(capture, expected);
        assert!(starter.launches().is_empty());
        assert!(start_events(&records).is_empty());
        assert!(stores.rows().await.is_empty());
    }
}
