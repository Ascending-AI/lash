//! S19/S20 (L08): an isolated call bound at admission to a registered engine
//! whose physical worker is a real OS process the serving node spawns.
//!
//! The worker writes its own PID marker. Retained files key each process by
//! the digest of its id, so a later incarnation of the node adopts a launched
//! worker and recovers a termination receipt instead of acting again.
use super::*;
use lash_core::tool_dispatch::{
    IsolatedToolStart, PhysicalProcessWorker, ProcessExecutionBoundary, WorkerTerminationReceipt,
};
use lash_core::{ProcessId, ProcessInput, ProcessProvenance, ProcessStartRegistration, StartKey};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(in crate::node) const ISOLATED: &str = "e2e.h3.isolated";
/// The engine kind the isolated call binds.
pub const WORKER_KIND: &str = "e2e-h3-worker";
const MARKER_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(25);

/// Whether the bound engine owns a physical worker.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(rename_all = "snake_case")]
pub enum IsolatedClaim {
    Physical,
    /// The start still claims a worker process; the engine has none.
    UnsupportedBoundary,
}

/// One isolated call. Each hold names a gate in the serving node's gate dir.
#[derive(Clone, Debug, Serialize, Deserialize, lash_core::facade_support::JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct IsolatedArgs {
    pub key: String,
    pub worker_dir: PathBuf,
    pub claim: IsolatedClaim,
    #[serde(default)]
    pub hold_prepare: Option<String>,
    /// At entry to the launch, after admission, before registration.
    #[serde(default)]
    pub hold_launch: Option<String>,
    /// After registration and worker spawn, before the launch returns.
    #[serde(default)]
    pub hold_registered: Option<String>,
    /// After termination, before the hold is released and discharge is ACKed.
    #[serde(default)]
    pub hold_discharge: Option<String>,
}

/// What the worker writes about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerMarker {
    pub process_id: ProcessId,
    pub pid: u32,
}

/// One actual worker spawn, appended by the node that spawned it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSpawn {
    pub process_id: ProcessId,
    pub pid: u32,
    pub node_pid: u32,
}

/// The `isolated-worker` command: the physical boundary of one process.
#[derive(Debug, Args)]
pub struct IsolatedWorkerArgs {
    #[arg(long)]
    pub process: String,
    #[arg(long)]
    pub marker: PathBuf,
}

pub fn start_key(key: &str) -> StartKey {
    StartKey::for_host(format!("e2e-h3-{key}"))
}

fn digest(process: &ProcessId) -> String {
    lash_core::stable_hash::sha256_hex(process.as_str().as_bytes())
}

pub fn worker_file(dir: &Path, process: &ProcessId) -> PathBuf {
    dir.join(format!("worker-{}.json", digest(process)))
}

/// The serving incarnation that entered the registered worker engine.
pub fn observer_file(dir: &Path, process: &ProcessId) -> PathBuf {
    dir.join(format!("observer-{}.marker", digest(process)))
}

pub fn receipt_file(dir: &Path, process: &ProcessId) -> PathBuf {
    dir.join(format!("receipt-{}.json", digest(process)))
}

pub fn spawns_file(dir: &Path) -> PathBuf {
    dir.join("spawns.jsonl")
}

pub fn body_marker(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("body-{key}.marker"))
}

pub fn terminal_file(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("terminal-{key}.json"))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("decode {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// Every spawn the case's nodes made, in order.
pub fn spawns(dir: &Path) -> Result<Vec<WorkerSpawn>> {
    let text = match std::fs::read_to_string(spawns_file(dir)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

/// Write the marker, then stay alive until terminated.
pub async fn isolated_worker(args: IsolatedWorkerArgs) -> Result<()> {
    let marker = WorkerMarker {
        process_id: ProcessId::parse(&args.process)?,
        pid: std::process::id(),
    };
    crate::node::write_atomically(&args.marker, &serde_json::to_vec(&marker)?)?;
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// What the serving node gives every isolated call.
#[derive(Clone)]
pub struct IsolatedHost {
    registry: Arc<dyn lash_core::ProcessRegistry>,
    effects: Arc<dyn lash_core::EffectHost>,
    gate_dir: Option<PathBuf>,
    environments: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    environment_pin: lash_core::HostArtifactPin,
    children: Arc<Mutex<BTreeMap<ProcessId, std::process::Child>>>,
}

impl IsolatedHost {
    pub fn new(
        registry: Arc<dyn lash_core::ProcessRegistry>,
        effects: Arc<dyn lash_core::EffectHost>,
        gate_dir: Option<PathBuf>,
        environments: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    ) -> Self {
        Self {
            registry,
            effects,
            gate_dir,
            environments,
            environment_pin: lash_core::HostArtifactPin::mint(),
            children: Arc::default(),
        }
    }

    fn engine(&self, physical: bool) -> Arc<WorkerEngine> {
        Arc::new(WorkerEngine {
            physical,
            registry: self.registry.clone(),
            children: self.children.clone(),
        })
    }

    pub(in crate::node) fn factory(
        &self,
        inner: StaticPluginFactory,
    ) -> Arc<dyn lash_core::facade_support::PluginFactory> {
        Arc::new(WorkerFactory {
            inner,
            engine: self.engine(true),
        })
    }

    async fn gate(&self, gate: Option<&String>) -> Result<(), String> {
        let Some(gate) = gate else {
            return Ok(());
        };
        let dir = self
            .gate_dir
            .as_ref()
            .ok_or_else(|| format!("gate {gate} needs a serving gate dir"))?;
        std::fs::write(
            crate::node::provider::reached_file(dir, gate),
            std::process::id().to_string(),
        )
        .map_err(|error| format!("mark gate {gate} reached: {error}"))?;
        while !crate::node::provider::release_file(dir, gate).exists() {
            tokio::time::sleep(POLL).await;
        }
        Ok(())
    }
}

struct WorkerFactory {
    inner: StaticPluginFactory,
    engine: Arc<WorkerEngine>,
}

impl lash_core::plugin::PluginMetadata for WorkerFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginMetadata::plugin_declaration(&self.inner)
    }
}

impl lash_core::facade_support::PluginFactory for WorkerFactory {
    fn id(&self) -> &'static str {
        lash_core::facade_support::PluginFactory::id(&self.inner)
    }

    fn build(
        &self,
        context: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
        lash_core::facade_support::PluginFactory::build(&self.inner, context)
    }

    fn process_engine_contributions(
        &self,
        _context: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            self.engine.clone(),
        )])
    }
}

struct WorkerEngine {
    registry: Arc<dyn lash_core::ProcessRegistry>,
    physical: bool,
    children: Arc<Mutex<BTreeMap<ProcessId, std::process::Child>>>,
}

impl WorkerEngine {
    /// Spawn the worker for `process`, or adopt the one a previous
    /// incarnation launched under the same process.
    async fn launch(&self, dir: &Path, process: &ProcessId) -> Result<u32, String> {
        let marker = worker_file(dir, process);
        if let Some(existing) = read_json::<WorkerMarker>(&marker)? {
            return Ok(existing.pid);
        }
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let log = std::fs::File::create(dir.join(format!("worker-{}.log", digest(process))))
            .map_err(|error| error.to_string())?;
        let child = std::process::Command::new(exe)
            .arg("isolated-worker")
            .arg("--process")
            .arg(process.as_str())
            .arg("--marker")
            .arg(&marker)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(|error| error.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|error| format!("spawn the isolated worker: {error}"))?;
        let pid = child.id();
        let spawn = WorkerSpawn {
            process_id: process.clone(),
            pid,
            node_pid: std::process::id(),
        };
        {
            use std::io::Write as _;
            let mut ledger = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(spawns_file(dir))
                .map_err(|error| error.to_string())?;
            writeln!(
                ledger,
                "{}",
                serde_json::to_string(&spawn).map_err(|error| error.to_string())?
            )
            .map_err(|error| error.to_string())?;
        }
        self.children
            .lock()
            .map_err(|_| "worker table poisoned".to_owned())?
            .insert(process.clone(), child);
        let deadline = Instant::now() + MARKER_WAIT;
        loop {
            if let Some(written) = read_json::<WorkerMarker>(&marker)? {
                if written.pid != pid || written.process_id != *process {
                    return Err(format!("worker marker {written:?} names another worker"));
                }
                return Ok(pid);
            }
            if Instant::now() >= deadline {
                return Err(format!("worker {pid} never wrote its marker"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn receipt(
        &self,
        dir: &Path,
        process: &ProcessId,
    ) -> Result<Option<WorkerTerminationReceipt>, String> {
        read_json(&receipt_file(dir, process))
    }
}

#[lash_core::async_trait]
impl PhysicalProcessWorker for WorkerEngine {
    async fn terminate_worker(
        &self,
        process: &ProcessId,
    ) -> Result<WorkerTerminationReceipt, lash_core::PluginError> {
        let record = self.registry.get_process(process).await?.ok_or_else(|| {
            lash_core::PluginError::Invoke(format!("no worker process {process}"))
        })?;
        let ProcessInput::Engine { payload, .. } = record.input.as_ref() else {
            return Err(lash_core::PluginError::Invoke(
                "the worker has no engine input".into(),
            ));
        };
        let dir = worker_directory(payload)?;
        if let Some(receipt) = self
            .receipt(&dir, process)
            .map_err(lash_core::PluginError::Invoke)?
        {
            return Ok(receipt);
        }
        let child = self
            .children
            .lock()
            .map_err(|_| lash_core::PluginError::Invoke("worker table poisoned".into()))?
            .remove(process);
        let Some(mut child) = child else {
            return Err(lash_core::PluginError::Invoke(format!(
                "worker of {process} is not a child of this incarnation and has no retained receipt"
            )));
        };
        let invoke = |error: std::io::Error| lash_core::PluginError::Invoke(error.to_string());
        let worker_pid = std::num::NonZeroU32::new(child.id())
            .ok_or_else(|| lash_core::PluginError::Invoke("worker has no pid".into()))?;
        child.kill().map_err(invoke)?;
        let status = child.wait().map_err(invoke)?;
        if status.success() || child.try_wait().map_err(invoke)? != Some(status) {
            return Err(lash_core::PluginError::Invoke(format!(
                "worker {worker_pid} was not terminated and reaped: {status}"
            )));
        }
        let receipt = WorkerTerminationReceipt {
            process_id: process.clone(),
            worker_pid,
        };
        let bytes = serde_json::to_vec(&receipt)
            .map_err(|error| lash_core::PluginError::Invoke(error.to_string()))?;
        crate::node::write_atomically(&receipt_file(&dir, process), &bytes)
            .map_err(|error| lash_core::PluginError::Invoke(format!("{error:#}")))?;
        Ok(receipt)
    }
}

#[lash_core::async_trait]
impl lash_core::ProcessEngine for WorkerEngine {
    fn kind(&self) -> &'static str {
        WORKER_KIND
    }

    fn physical_worker(&self) -> Option<&dyn PhysicalProcessWorker> {
        self.physical.then_some(self)
    }

    async fn run(
        &self,
        context: lash_core::ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        // Launch belongs to the isolated start. A workflow only observes its
        // retained worker, including after a host cut; it never spawns again.
        let dir = worker_directory(&payload)?;
        crate::node::write_atomically(
            &observer_file(&dir, context.process_id()),
            std::process::id().to_string().as_bytes(),
        )
        .map_err(|error| lash_core::PluginError::Invoke(format!("observe worker: {error:#}")))?;
        let cancellation = context.cancellation_token();
        loop {
            if self
                .receipt(&dir, context.process_id())
                .map_err(lash_core::PluginError::Invoke)?
                .is_some()
            {
                return Ok(lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
                        "the isolated worker was terminated",
                    )),
                )
                .into());
            }
            tokio::select! {
                () = cancellation.cancelled() => {
                    self.terminate_worker(context.process_id()).await?;
                }
                () = tokio::time::sleep(POLL) => {}
            }
        }
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

fn worker_directory(payload: &serde_json::Value) -> Result<PathBuf, lash_core::PluginError> {
    serde_json::from_value(payload["worker_dir"].clone())
        .map_err(|error| lash_core::PluginError::Invoke(format!("worker directory: {error}")))
}

pub(in crate::node) fn register(spec: PluginSpec, host: IsolatedHost) -> PluginSpec {
    spec.with_plugin_task_typed::<Isolated, _, _>(move |ctx, args| {
        let host = host.clone();
        async move { run(ctx, host, args).await }
    })
}

async fn run(
    ctx: PluginTaskContext,
    host: IsolatedHost,
    args: IsolatedArgs,
) -> Result<PluginOperationOutcome<String>, String> {
    let mut call = super::call(
        &ctx,
        "isolated",
        ToolDeclaration {
            isolated: true,
            ..ToolDeclaration::default()
        },
    )?;
    call.arguments = serde_json::json!({ "key": args.key });
    call.cancel = ExternalCancelPolicy::CancelExternalWork;
    let session_id = ctx
        .session_id
        .as_ref()
        .ok_or("the isolated fixture has no session")?;
    let snapshot = ctx
        .sessions
        .snapshot_session(session_id)
        .await
        .map_err(|error| error.to_string())?;
    // These fixture sessions have only their initial configuration revision.
    let environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::new(snapshot.plugin_config, 0),
        snapshot.policy,
    );
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        host.environment_pin.clone(),
    ))
    .map_err(|error| error.to_string())?;
    call.environment = Some(
        lash_core::publish_process_execution_env(host.environments.as_ref(), &claim, &environment)
            .await
            .map_err(|error| error.to_string())?,
    );
    let engine = host.engine(args.claim == IsolatedClaim::Physical);
    let engines = lash_core::ProcessEngineRegistry::new().with_registration(
        lash_core::ProcessEngineRegistration::accepting(engine.clone()),
    );
    // The operation's durable cancel signal: the task's token is fired by a
    // watch that a cold replay may not have run yet when a recorded decision
    // reads it, so the decision also peeks the signal itself.
    let lash_core::ExecutionScope::SessionOperation {
        session_id,
        operation_id,
    } = ctx.scoped_effect_controller.execution_scope()
    else {
        return Err("the isolated fixture was not admitted as an operation Run".into());
    };
    let cancel_signal = host
        .effects
        .await_event_resolver()
        .await_event_key(
            &lash_core::ExecutionScope::session_operation(session_id.clone(), operation_id.clone()),
            lash_core::AwaitEventWaitIdentity::SessionCommandCancelSignal,
        )
        .await
        .map_err(|error| error.to_string())?;
    let token = ctx.cancellation_token.clone();
    let handlers = Arc::new(IsolatedHandlers {
        args: args.clone(),
        host,
        engine,
        engines,
        cancelled: Arc::new(move || token.is_cancelled()),
        cancel_signal,
    });
    let mut run = RunCoordinator::open(
        &ctx.scoped_effect_controller,
        call.owner.clone(),
        call.segment,
        call.available.clone(),
    );
    let driven = Box::pin(async {
        let decided = run
            .start_round(
                std::slice::from_ref(&call),
                lash_core::tool_run::CapacityScope::Held,
                handlers,
                Default::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
        if decided.is_empty() {
            while run
                .progress()
                .await
                .map_err(|error| error.to_string())?
                .is_none()
            {}
        }
        let terminals = run.drain().await.map_err(|error| error.to_string())?;
        run.close().await.map_err(|error| error.to_string())?;
        Ok::<_, String>(terminals)
    })
    .await;
    let (record, output) = match &driven {
        Ok(terminals) => {
            let output = terminals.iter().find_map(|(_, terminal)| match terminal {
                lash_core::tool_dispatch::SingletonTerminal::Final {
                    launched: Some(_),
                    presentation,
                    ..
                } => Some(presentation.clone()),
                _ => None,
            });
            let withheld = terminals.iter().find_map(|(_, terminal)| match terminal {
                lash_core::tool_dispatch::SingletonTerminal::Withheld { decision } => {
                    Some(serde_json::json!({ "withheld": decision }).to_string())
                }
                _ => None,
            });
            (
                serde_json::json!({
                    "terminals": format!("{terminals:?}"),
                    "presentation": output,
                    "withheld": withheld,
                }),
                output.or(withheld),
            )
        }
        Err(error) => (serde_json::json!({ "error": error }), None),
    };
    crate::node::write_atomically(
        &terminal_file(&args.worker_dir, &args.key),
        &serde_json::to_vec(&record).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("{error:#}"))?;
    driven?;
    output
        .map(PluginOperationOutcome::new)
        .ok_or_else(|| "isolated call ended without a launched final or a withheld decision".into())
}

struct Isolated;
task!(Isolated, ISOLATED, IsolatedArgs);

struct IsolatedHandlers {
    args: IsolatedArgs,
    host: IsolatedHost,
    engine: Arc<WorkerEngine>,
    engines: lash_core::ProcessEngineRegistry,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    cancel_signal: lash_core::AwaitEventKey,
}

#[lash_core::async_trait]
impl SingletonToolHandlers for IsolatedHandlers {
    fn process_engines(&self) -> Option<&lash_core::ProcessEngineRegistry> {
        Some(&self.engines)
    }

    fn isolated_start(&self, _call: &SingletonToolCall) -> Option<IsolatedToolStart> {
        Some(IsolatedToolStart {
            boundary: ProcessExecutionBoundary::WorkerProcess,
            registration: ProcessStartRegistration::of_target(
                ProcessInput::Engine {
                    kind: WORKER_KIND.to_owned(),
                    payload: serde_json::json!({ "key": self.args.key, "worker_dir": self.args.worker_dir }),
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_start_key(Some(start_key(&self.args.key))),
        })
    }

    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        self.host.gate(self.args.hold_prepare.as_ref()).await?;
        Ok(call.arguments.clone())
    }

    async fn before_checks(
        &self,
        _: &SingletonToolCall,
        _: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(Vec::new())
    }

    async fn execute(&self, _: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        std::fs::write(body_marker(&self.args.worker_dir, &self.args.key), b"ran")
            .map_err(|error| error.to_string())?;
        Err("an isolated call ran an ordinary body".into())
    }

    async fn after_checks(
        &self,
        _: &lash_core::ToolCallId,
        _: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }

    async fn run_cancel_requested(&self) -> Result<bool, String> {
        if (self.cancelled)() {
            return Ok(true);
        }
        match self
            .host
            .effects
            .await_event_resolver()
            .peek_await_event(&self.cancel_signal)
            .await
            .map_err(|error| error.to_string())?
        {
            None => Ok(false),
            Some(lash_core::Resolution::Cancelled) => Ok(true),
            Some(other) => Err(format!("the operation's cancel signal holds {other:?}")),
        }
    }

    async fn cancel_call(
        &self,
        _call_id: &lash_core::ToolCallId,
        _source: Option<&lash_core::AwaitEventKey>,
    ) -> Result<(), String> {
        // The external work is the declared start; its recorded obligation
        // terminates the worker and releases the hold at discharge.
        Ok(())
    }

    async fn present(
        &self,
        _: &lash_core::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        Ok(capture.output().unwrap_or_default().to_owned())
    }

    fn emit_stream(&self, _: &lash_core::ToolCallId, _: &lash_core::runtime::AttemptStream) {}

    async fn launch_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<lash_core::tool_dispatch::StartLaunch, String> {
        self.host.gate(self.args.hold_launch.as_ref()).await?;
        let registration = obligation
            .registration
            .clone()
            .stating_input()
            .map_err(|_| "the isolated start names a definition".to_owned())?;
        let record = self
            .host
            .registry
            .register_process(registration)
            .await
            .map_err(|error| error.to_string())?;
        self.engine
            .launch(&self.args.worker_dir, &record.id)
            .await?;
        self.host.gate(self.args.hold_registered.as_ref()).await?;
        Ok(lash_core::tool_dispatch::StartLaunch::Launched(
            lash_core::ProcessHandleView::from_record(record),
        ))
    }

    async fn discharge_start(
        &self,
        obligation: &DeclaredStartObligation,
        process: &ProcessId,
        cancel: bool,
    ) -> Result<(), String> {
        self.host.gate(self.args.hold_discharge.as_ref()).await?;
        if cancel {
            self.host
                .registry
                .request_process_cancel(
                    process,
                    lash_core::CancelOrigin::TurnStopped,
                    format!("e2e-h3:{}", obligation.call_id),
                    None,
                )
                .await
                .map_err(|error| error.to_string())?;
            if self
                .engine
                .receipt(&self.args.worker_dir, process)?
                .is_none()
            {
                return Err("the consumer hold would be released before termination".into());
            }
        }
        let hold = obligation
            .registration
            .consumer_hold
            .as_ref()
            .ok_or("a declared start carries its hold")?;
        self.host
            .registry
            .release_consumer_hold(process, &hold.key)
            .await
            .map_err(|error| error.to_string())
    }
}
