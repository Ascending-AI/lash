//! A final's declared process start drains inside its declarations (K5,
//! FIG-4884): the singleton Run route through a real handler on the
//! in-process Restate server double, launching into a real SQLite process
//! registry, in memory and in a file the law closes and reopens.
//!
//! The start's obligation is recorded with its attempt: the body's stable
//! start key, bound by the Run to the Run's environment (the start is an
//! engine process lash executes) and to a consumer hold that carries the
//! call's recorded cancel policy. The declaration
//! record admits it, `start:launch` registers it under its key and
//! `start:discharge` follows the policy and releases the hold, before the
//! presentation settles the declarations. A crash drops the attempt that hit
//! it, and the double replays the invocation into the same handler.

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
    SingletonPreparedRequest, SingletonRunError, SingletonRunOutcome, SingletonTerminal,
    SingletonToolCall, SingletonToolHandlers, WorkerTerminationReceipt, run_singleton_tool,
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

const PLUGIN: &str = "fig4884-tools";
const OUTPUT: &str = "fig4884 started";
const PRESENTATION: &str = "fig4884 presented";
const ENVIRONMENT: &str = "process-env:fig4884";
const STEPS: [&str; 7] = [
    "admit",
    "attempt:1",
    "decide",
    "declare",
    "start:launch",
    "start:discharge",
    "present",
];

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
    ) -> Vec<AttributedVerdict<BeforeCheckReply>> {
        Vec::new()
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

    fn run_cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
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

    async fn realize_declarations(
        &self,
        _call_id: &ToolCallId,
        _intents: &[ToolIntentKind],
    ) -> Result<(), String> {
        Err("a declared start is not realized as an intent".to_owned())
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
    ) -> Result<ProcessId, String> {
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
            .register_process(registration)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(engine) = &self.worker {
            engine.launch(&record.id);
        }
        self.launches.lock().unwrap().push(record.id.clone());
        Ok(record.id)
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

/// Where the law's registry lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Memory,
    /// A file the law closes and reopens before it reads the rows.
    FileReopen,
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
    tier: Tier,
    set: SqliteStoreSet,
    dir: Option<tempfile::TempDir>,
}

impl Stores {
    async fn open(tier: Tier) -> Self {
        match tier {
            Tier::Memory => Self {
                tier,
                set: SqliteStoreSet::memory().await.unwrap(),
                dir: None,
            },
            Tier::FileReopen => {
                let dir = tempfile::tempdir().unwrap();
                Self {
                    tier,
                    set: SqliteStoreSet::open(dir.path()).await.unwrap(),
                    dir: Some(dir),
                }
            }
        }
    }

    /// Every process row, after closing and reopening a file tier.
    async fn rows(self) -> Vec<Row> {
        let set = match (self.tier, &self.dir) {
            (Tier::FileReopen, Some(dir)) => {
                drop(self.set);
                SqliteStoreSet::open(dir.path()).await.unwrap()
            }
            _ => self.set,
        };
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

    /// The names of the `ctx.run` records the handler's journal holds.
    fn journal(&self) -> Vec<String> {
        let mut names = Vec::new();
        for view in self.backend.server().invocations() {
            for entry in self.backend.server().journal(&view.id).unwrap() {
                if entry.ty == lash_restate_test::protocol::MessageType::RunCommand {
                    names.push(entry.name.unwrap_or_default());
                }
            }
        }
        names
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
    let backend = lash_restate_test::backend(0x4884, ServerConfig::default())
        .await
        .unwrap();
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
        let name = format!("lash:run:{}:{step}", call.call_id);
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
                        run.decide(&call, starter.as_ref()).await?;
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
                    run_singleton_tool(&scoped, &call, starter.as_ref()).await
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

fn names(call_id: &ToolCallId, steps: &[&str]) -> Vec<String> {
    steps
        .iter()
        .map(|step| format!("lash:run:{call_id}:{step}"))
        .collect()
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
        },
        RunEvent::StartDischarged {
            call_id: call_id.clone(),
            start_key: key.clone(),
            cancelled,
        },
    ]
}

/// L08 and L04: a final's start is admitted with its declarations,
/// registered, then discharged, before the presentation settles them. A lost
/// record at any boundary reruns only that record's step: a lost launch
/// registers again under the same key and gets the same process back, so
/// the registry holds one process under the key, and the hold is released
/// whichever cut was taken. SQLite memory and a reopened SQLite file agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declared_start_drains_inside_its_declarations_at_every_cut() {
    for tier in [Tier::Memory, Tier::FileReopen] {
        for cut in std::iter::once(None).chain(STEPS.iter().copied().map(Some)) {
            let label = "drain";
            let call = call(label, ExternalCancelPolicy::Ignore);
            let key = start_key(label);
            let stores = Stores::open(tier).await;
            let starter = Starter::new(
                stores.set.process_registry(),
                declaring(Some(key.clone())),
                CancelAt::Never,
            );
            let driven = drive(cut, call.clone(), Arc::clone(&starter)).await;
            let (terminal, records) = driven.finished();
            let SingletonTerminal::Final {
                launched: Some(process),
                capture,
                ..
            } = terminal
            else {
                panic!("{tier:?} {cut:?}: {terminal:?}");
            };
            assert_eq!(
                capture.start().map(|start| &start.start_key),
                Some(&key),
                "the attempt records the start under its key"
            );
            let rerun = |step: &str| 1 + usize::from(cut == Some(step));
            assert_eq!(
                starter.executions.load(Ordering::SeqCst),
                rerun("attempt:1"),
                "{tier:?} {cut:?}"
            );
            assert_eq!(
                starter.launches(),
                vec![process.clone(); rerun("start:launch")],
                "{tier:?} {cut:?}: every launch recovers the same process"
            );
            assert_eq!(
                starter.discharges(),
                vec![(process.clone(), false); rerun("start:discharge")],
                "{tier:?} {cut:?}"
            );
            assert_eq!(
                start_events(&records),
                drained(&call.call_id, &key, &process, false)
            );
            assert_eq!(driven.journal(), names(&call.call_id, &STEPS));
            assert_eq!(
                stores.rows().await,
                vec![Row::drained(&process, &key, false)],
                "{tier:?} {cut:?}: one process under the key, its hold released"
            );
        }
    }
}

/// L08: a cancellation before the decision is durable withholds the final,
/// so its start is never admitted and nothing is registered. One after the
/// start's admission cannot forbid it: a launch whose record was lost
/// registers again under the same key and recovers the same process, and the
/// discharge then follows the recorded policy — a cancelling policy cancels
/// that process once however often the discharge runs, an ignoring one
/// leaves it running — and releases the hold. A cancellation after the
/// discharge changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_before_admission_forbids_the_start_and_one_after_recovers_it() {
    use ExternalCancelPolicy::{CancelExternalWork, Ignore};
    for tier in [Tier::Memory, Tier::FileReopen] {
        let stores = Stores::open(tier).await;
        let forbidden = call("forbidden", CancelExternalWork);
        let starter = Starter::new(
            stores.set.process_registry(),
            declaring(Some(start_key("forbidden"))),
            CancelAt::Body,
        );
        let driven = drive(None, forbidden.clone(), Arc::clone(&starter)).await;
        let (terminal, records) = driven.finished();
        assert_eq!(
            terminal,
            SingletonTerminal::Withheld {
                decision: CallDecision::Cancelled
            },
            "{tier:?}"
        );
        assert!(starter.launches().is_empty(), "{tier:?}: no start launched");
        assert!(start_events(&records).is_empty(), "{tier:?}");
        assert_eq!(
            driven.journal(),
            names(
                &forbidden.call_id,
                &["admit", "attempt:1", "decide", "present"]
            )
        );
        assert_eq!(
            stores.rows().await,
            Vec::new(),
            "{tier:?}: nothing registered"
        );

        for (label, policy, cancel_at, crash, cancelled) in [
            (
                "cancels",
                CancelExternalWork,
                CancelAt::Launch,
                Some("start:launch"),
                true,
            ),
            (
                "cancels-twice",
                CancelExternalWork,
                CancelAt::Launch,
                Some("start:discharge"),
                true,
            ),
            (
                "ignores",
                Ignore,
                CancelAt::Launch,
                Some("start:launch"),
                false,
            ),
            (
                "after-discharge",
                CancelExternalWork,
                CancelAt::Presentation,
                None,
                false,
            ),
        ] {
            let stores = Stores::open(tier).await;
            let call = call(label, policy);
            let key = start_key(label);
            let starter = Starter::new(
                stores.set.process_registry(),
                declaring(Some(key.clone())),
                cancel_at,
            );
            let driven = drive(crash, call.clone(), Arc::clone(&starter)).await;
            let (terminal, records) = driven.finished();
            let SingletonTerminal::Final {
                launched: Some(process),
                ..
            } = terminal
            else {
                panic!(
                    "{tier:?} {label}: post-admission cancellation keeps the final: {terminal:?}"
                );
            };
            let launches = starter.launches();
            assert!(
                !launches.is_empty() && launches.iter().all(|launched| *launched == process),
                "{tier:?} {label}: {launches:?} recover {process}"
            );
            assert!(
                starter
                    .discharges()
                    .iter()
                    .all(|discharge| discharge == &(process.clone(), cancelled)),
                "{tier:?} {label}"
            );
            assert_eq!(
                start_events(&records),
                drained(&call.call_id, &key, &process, cancelled),
                "{tier:?} {label}"
            );
            assert_eq!(
                stores.rows().await,
                vec![Row::drained(&process, &key, cancelled)],
                "{tier:?} {label}: the recorded policy decides the cancel; the hold is released"
            );
        }
    }
}

/// L08 and L12: a start the Run cannot hold to one identity is refused in
/// its attempt record, before admission: a keyless start, a start in a Run
/// that owns no environment, and a start the admitted declaration does not
/// name. None launches, and the final declares nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_without_key_environment_or_declaration_is_refused_before_admission() {
    let mut no_environment = call("no-environment", ExternalCancelPolicy::Ignore);
    no_environment.environment = None;
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
            no_environment,
            Some(start_key("no-environment")),
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
        let stores = Stores::open(Tier::Memory).await;
        let starter = Starter::new(
            stores.set.process_registry(),
            declaring(key),
            CancelAt::Never,
        );
        let driven = drive(None, call.clone(), Arc::clone(&starter)).await;
        let (terminal, records) = driven.finished();
        let SingletonTerminal::Final {
            capture, launched, ..
        } = terminal
        else {
            panic!("{}: {terminal:?}", call.call_id);
        };
        assert_eq!(capture, expected, "{}", call.call_id);
        assert_eq!(launched, None);
        assert!(starter.launches().is_empty());
        assert!(start_events(&records).is_empty());
        assert_eq!(
            driven.journal(),
            names(&call.call_id, &["admit", "attempt:1", "decide", "present"]),
            "a refused start declares nothing"
        );
        assert_eq!(stores.rows().await, Vec::new());
    }
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

/// L08: a supported isolated call fixes a registered implementation at A,
/// recovers one process through every cut and never invokes an ordinary body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_isolated_call_starts_its_registered_process_without_an_ordinary_body() {
    for tier in [Tier::Memory, Tier::FileReopen] {
        for cut in std::iter::once(None).chain(STEPS.iter().copied().map(Some)) {
            let stores = Stores::open(tier).await;
            let call = isolated_call("isolated", ExternalCancelPolicy::Ignore);
            let starter = isolated_starter(&stores, false, CancelAt::Never, "fig4884-index");
            let driven = drive(cut, call.clone(), Arc::clone(&starter)).await;
            let (terminal, records) = driven.finished();
            let SingletonTerminal::Final {
                launched: Some(process),
                presentation,
                ..
            } = terminal
            else {
                panic!("{tier:?} {cut:?}: {terminal:?}")
            };
            let descriptor: IsolatedProcessDescriptor =
                serde_json::from_str(&presentation).unwrap();
            assert_eq!(descriptor.process_id, process);
            assert_eq!(descriptor.start_key, start_key("isolated"));
            assert_eq!(descriptor.boundary, ProcessExecutionBoundary::Invocation);
            assert_eq!(descriptor.termination, None);
            assert_eq!(
                starter.executions.load(Ordering::SeqCst),
                0,
                "no ordinary body"
            );
            assert_eq!(
                starter.launches(),
                vec![process.clone(); 1 + usize::from(cut == Some("start:launch"))]
            );
            assert_eq!(
                start_events(&records),
                drained(&call.call_id, &start_key("isolated"), &process, false)
            );
            assert_eq!(
                stores.rows().await,
                vec![Row::drained(&process, &start_key("isolated"), false)]
            );
        }
    }
}

/// L08: pre-admission cancellation forbids launch. A protected start recovers
/// the same worker on cancellation, records a physical reap receipt, and
/// releases its hold after termination even when launch or discharge is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_isolated_cancel_forbids_launch_or_recovers_and_reaps_the_same_worker() {
    for tier in [Tier::Memory, Tier::FileReopen] {
        for (cancel_at, cut) in [
            (CancelAt::Preparation, None),
            (CancelAt::Launch, None),
            (CancelAt::Launch, Some("start:launch")),
            (CancelAt::Launch, Some("start:discharge")),
            (CancelAt::Launch, Some("present")),
        ] {
            let stores = Stores::open(tier).await;
            let call = isolated_call("physical", ExternalCancelPolicy::CancelExternalWork);
            let starter = isolated_starter(&stores, true, cancel_at, "fig4884-index");
            let driven = drive(cut, call.clone(), Arc::clone(&starter)).await;
            let (terminal, records) = driven.finished();
            let engine = starter.worker.as_ref().unwrap();
            assert_eq!(starter.executions.load(Ordering::SeqCst), 0);
            if cancel_at == CancelAt::Preparation {
                assert_eq!(
                    terminal,
                    SingletonTerminal::Withheld {
                        decision: CallDecision::Cancelled
                    }
                );
                assert_eq!(engine.spawned.load(Ordering::SeqCst), 0);
                assert!(start_events(&records).is_empty());
                assert!(stores.rows().await.is_empty());
            } else {
                let SingletonTerminal::Final {
                    launched: Some(process),
                    presentation,
                    ..
                } = terminal
                else {
                    panic!("{tier:?} {cut:?}: {terminal:?}")
                };
                let descriptor: IsolatedProcessDescriptor =
                    serde_json::from_str(&presentation).unwrap();
                assert_eq!(descriptor.process_id, process);
                assert_eq!(descriptor.boundary, ProcessExecutionBoundary::WorkerProcess);
                assert_eq!(descriptor.termination, engine.receipt(&process));
                assert!(
                    descriptor.termination.is_some(),
                    "a physical termination receipt is required"
                );
                assert_eq!(
                    engine.spawned.load(Ordering::SeqCst),
                    1,
                    "a replay never spawns a replacement worker"
                );
                assert!(starter.launches().iter().all(|id| *id == process));
                assert_eq!(
                    start_events(&records),
                    drained(&call.call_id, &start_key("physical"), &process, true)
                );
                assert_eq!(
                    stores.rows().await,
                    vec![Row::drained(&process, &start_key("physical"), true)]
                );
            }
        }
    }
}

/// L08 and L12: unsupported isolation and unavailable recorded implementations
/// refuse before a body or new route. A cooperative engine cannot claim a
/// physical worker. A replacement live binding cannot replace a recorded one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_or_changed_isolation_refuses_before_a_body_or_new_identity() {
    let stores = Stores::open(Tier::Memory).await;
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
        let stores = Stores::open(Tier::Memory).await;
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
        let stores = Stores::open(Tier::Memory).await;
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
                .all(|view| !view.target.starts_with("LashProcessAttach/")
                    && !view.target.ends_with("/await_terminal")),
            "no process waiter invocation survives"
        );
    }
}
mod process_transfer;
