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
    BeforeCheckReply, DeclaredStartObligation, DeclaredStartObligationRefusal, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonPreparedRequest, SingletonRunError,
    SingletonRunOutcome, SingletonTerminal, SingletonToolCall, SingletonToolHandlers,
    run_singleton_tool,
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
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
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
        if self.cancel_at == CancelAt::Body {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(self.body.clone())
    }

    async fn after_checks(
        &self,
        _call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>> {
        Vec::new()
    }

    fn run_cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
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
    ) -> Result<String, String> {
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
    let backend = lash_restate_test::backend(0x4884, ServerConfig::default())
        .await
        .unwrap();
    if let Some(step) = crash {
        backend
            .server()
            .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
                name: Some(format!("lash:run:{}:{step}", call.call_id)),
            }));
    }
    let returned: Returned = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let returned = Arc::clone(&returned);
        Arc::new(move |scoped| {
            let call = call.clone();
            let starter = Arc::clone(&starter);
            let returned = Arc::clone(&returned);
            Box::pin(async move {
                let outcome = run_singleton_tool(&scoped, &call, starter.as_ref()).await;
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
