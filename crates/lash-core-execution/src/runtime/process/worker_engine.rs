//! A process engine whose processes each run in their own OS worker (D04).
//!
//! An isolated call that promises
//! [`ProcessExecutionBoundary::WorkerProcess`](super::ProcessExecutionBoundary)
//! needs an engine that runs the work outside the host process and can
//! terminate and reap it before a cancellation is acknowledged. This engine
//! runs one host-configured program per lash process: the start payload is
//! written to the worker's stdin as JSON, and the worker's stdout (as JSON,
//! else as text) is the process's result when it exits successfully.
//!
//! The host supplies a durable ownership directory shared by its replacement
//! deployments on the same Linux kernel. K5 recovers the same ProcessId from
//! the StartKey; that process's ledger retains its worker identity and terminal.
//! Cold redelivery adopts a verified live PID or returns its retained terminal,
//! never launching another worker. A payload is released only after the launch
//! is durable. A host lost before that write closes the gate with EOF, so
//! no configured work has run and a retry is safe (ADR 0110).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::worker_ownership::{Adopted, Ledger, Ownership, WorkerIdentity};
use tokio::sync::{Notify, watch};

use super::{
    PhysicalProcessWorker, ProcessAwaitOutput, ProcessEngine, ProcessEngineRunContext, ProcessId,
    ProcessInfraError, ProcessRunOutcome, WorkerTerminationReceipt,
};

/// The most stdout a worker's result may carry.
const STDOUT_LIMIT: u64 = 1024 * 1024;
/// How long a termination waits for the run that spawns the worker.
const DEFAULT_SPAWN_WAIT: Duration = Duration::from_secs(30);

/// The host-configured program a [`WorkerProcessEngine`] runs, one OS
/// process per lash process. It runs with an empty environment.
#[derive(Clone, Debug)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum WorkerState {
    Running {
        worker: WorkerIdentity,
    },
    /// The worker ended by itself and was reaped.
    Exited {
        pid: NonZeroU32,
        output: Box<ProcessAwaitOutput>,
    },
    /// The worker was killed on request and reaped.
    Reaped(WorkerTerminationReceipt),
}

impl WorkerState {
    pub(super) fn is_terminal(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Reaped(_))
    }
}

struct Slot {
    state: watch::Receiver<WorkerState>,
    kill: Arc<Notify>,
}

/// A [`ProcessEngine`] that runs each process of its kind in its own OS
/// worker process and terminates and reaps it on cancellation: the
/// `WorkerProcess` boundary an isolated tool may promise.
///
/// Construct it once and contribute the same `Arc` from every
/// [`process_engine_contributions`](crate::PluginFactory::process_engine_contributions)
/// call. Supply the same durable directory to a replacement deployment on
/// the same Linux kernel. Retain it for as long as its StartKeys are retained:
/// deleting it permits a new launch and breaks start idempotency. A record from
/// another kernel or PID namespace refuses ownership observation.
pub struct WorkerProcessEngine {
    kind: &'static str,
    command: WorkerCommand,
    ownership_dir: PathBuf,
    spawn_wait: Duration,
    workers: Mutex<BTreeMap<ProcessId, Slot>>,
    spawned: watch::Sender<u64>,
}

impl WorkerProcessEngine {
    /// Bind this worker kind to a host program and a retained ownership
    /// directory. A replacement host supplies the same directory. Physical
    /// adoption requires Linux pidfds and the original PID namespace.
    #[must_use]
    pub fn new(kind: &'static str, command: WorkerCommand, ownership_dir: PathBuf) -> Self {
        Self {
            kind,
            command,
            ownership_dir,
            spawn_wait: DEFAULT_SPAWN_WAIT,
            workers: Mutex::default(),
            spawned: watch::Sender::new(0),
        }
    }

    /// How long a termination waits for a process's worker to be spawned
    /// before it fails (and its caller retries).
    #[must_use]
    pub fn with_spawn_wait(mut self, spawn_wait: Duration) -> Self {
        self.spawn_wait = spawn_wait;
        self
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the host configured this worker program (FIG-2971)"
    )]
    fn spawn_child(&self) -> std::io::Result<tokio::process::Child> {
        // The wrapper execs in place, preserving the recorded PID. It cannot
        // run the configured program before the parent durably records that
        // PID and sends the authorization line. EOF on host loss forbids work.
        tokio::process::Command::new("/bin/sh")
            .args([
                "-c",
                "IFS= read -r permit; [ \"$permit\" = lash-worker-start ] || exit 125; exec /usr/bin/env -i -- \"$@\"",
                "lash-worker",
            ])
            .arg(&self.command.program)
            .args(&self.command.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }

    fn ledger(&self, process_id: &ProcessId) -> Ledger {
        Ledger::new(&self.ownership_dir, self.kind, process_id)
    }

    /// Restore a worker before a cold cancellation, even if its workflow
    /// has already finished and therefore will never call `run` again.
    fn restore_worker(&self, process_id: &ProcessId) -> Result<(), crate::PluginError> {
        let mut workers = self.workers.lock_recover();
        if workers.contains_key(process_id) {
            return Ok(());
        }
        let ledger = self.ledger(process_id);
        let record = match ledger.lock().and_then(|locked| locked.read()) {
            Ok(record) => record,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(ownership_error(error)),
        };
        if let Some(record) = record {
            if record.process_id != *process_id {
                return Err(crate::durable_identity_conflict(format!(
                    "worker ownership changed for process `{process_id}`"
                )));
            }
            self.install_worker(&mut workers, process_id, ledger, record.state)?;
        }
        Ok(())
    }

    fn install_worker(
        &self,
        workers: &mut BTreeMap<ProcessId, Slot>,
        process_id: &ProcessId,
        ledger: Ledger,
        initial: WorkerState,
    ) -> Result<(watch::Receiver<WorkerState>, Arc<Notify>), crate::PluginError> {
        let adopted = match &initial {
            WorkerState::Running { worker } => Adopted::open(worker).map_err(ownership_error)?,
            _ => None,
        };
        let (state, receiver) = watch::channel(initial.clone());
        let kill = Arc::new(Notify::new());
        workers.insert(
            process_id.clone(),
            Slot {
                state: receiver.clone(),
                kill: kill.clone(),
            },
        );
        if let WorkerState::Running { worker } = initial {
            lash_core_ids::task::spawn(supervise_adopted(
                process_id.clone(),
                worker,
                adopted,
                ledger,
                kill.clone(),
                state,
            ));
        }
        self.spawned.send_modify(|generation| *generation += 1);
        Ok((receiver, kill))
    }

    /// One durable launch for the process the StartKey recovered. A second
    /// deployment consults the same ledger while holding its OS file lock.
    #[expect(
        clippy::disallowed_methods,
        reason = "the host supplies this durable worker ownership directory"
    )]
    fn start_worker(
        &self,
        process_id: &ProcessId,
        start_key: Option<&super::StartKey>,
        payload: &serde_json::Value,
    ) -> Result<(watch::Receiver<WorkerState>, Arc<Notify>), crate::PluginError> {
        let mut workers = self.workers.lock_recover();
        if let Some(slot) = workers.get(process_id) {
            return Ok((slot.state.clone(), Arc::clone(&slot.kill)));
        }
        std::fs::create_dir_all(&self.ownership_dir).map_err(ownership_error)?;
        let ledger = self.ledger(process_id);
        let locked = ledger.lock().map_err(ownership_error)?;
        if let Some(record) = locked.read().map_err(ownership_error)? {
            if record.process_id != *process_id
                || record.start_key.as_ref() != start_key
                || record.payload != *payload
            {
                return Err(crate::durable_identity_conflict(format!(
                    "worker ownership changed for process `{process_id}`"
                )));
            }
            drop(locked);
            return self.install_worker(&mut workers, process_id, ledger, record.state);
        }
        let mut input = b"lash-worker-start\n".to_vec();
        input.extend(
            serde_json::to_vec(payload)
                .map_err(|error| crate::PluginError::Session(error.to_string()))?,
        );
        let mut child = self.spawn_child().map_err(ownership_error)?;
        let pid = child.id().and_then(NonZeroU32::new).ok_or_else(|| {
            crate::PluginError::Session("the spawned worker has no process id".to_owned())
        })?;
        let record = Ownership {
            process_id: process_id.clone(),
            start_key: start_key.cloned(),
            payload: payload.clone(),
            state: WorkerState::Running {
                worker: WorkerIdentity::read(pid).map_err(ownership_error)?,
            },
        };
        locked.write(&record).map_err(ownership_error)?;
        drop(locked);
        let (state, receiver) = watch::channel(record.state);
        let kill = Arc::new(Notify::new());
        workers.insert(
            process_id.clone(),
            Slot {
                state: receiver.clone(),
                kill: Arc::clone(&kill),
            },
        );
        drop(workers);
        self.spawned.send_modify(|generation| *generation += 1);
        if let Some(mut stdin) = child.stdin.take() {
            lash_core_ids::task::spawn(async move {
                let _ = stdin.write_all(&input).await;
            });
        }
        let stdout = child.stdout.take().map(|stdout| {
            lash_core_ids::task::spawn(async move {
                let mut bytes = Vec::new();
                let _ = stdout.take(STDOUT_LIMIT).read_to_end(&mut bytes).await;
                bytes
            })
        });
        lash_core_ids::task::spawn(supervise(
            process_id.clone(),
            child,
            pid,
            stdout,
            Arc::clone(&kill),
            state,
            ledger,
        ));
        Ok((receiver, kill))
    }
}

/// Own one worker until it ends: by itself, or killed and reaped on request.
async fn supervise(
    process_id: ProcessId,
    mut child: tokio::process::Child,
    pid: NonZeroU32,
    stdout: Option<tokio::task::JoinHandle<Vec<u8>>>,
    kill: Arc<Notify>,
    state: watch::Sender<WorkerState>,
    ledger: Ledger,
) {
    let exited = tokio::select! {
        biased;
        () = kill.notified() => None,
        status = child.wait() => Some(status),
    };
    let next = match exited {
        None => {
            // `kill` sends SIGKILL and waits: the worker is reaped.
            let _ = child.start_kill();
            loop {
                match child.wait().await {
                    Ok(_) => break,
                    Err(error) => {
                        tracing::warn!(%error, "worker reap failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            WorkerState::Reaped(WorkerTerminationReceipt {
                process_id,
                worker_pid: pid,
            })
        }
        Some(status) => {
            let bytes = match stdout {
                Some(read) => read.await.unwrap_or_default(),
                None => Vec::new(),
            };
            WorkerState::Exited {
                pid,
                output: Box::new(exit_output(status, &bytes)),
            }
        }
    };
    publish_terminal(ledger, state, next).await;
}

fn ownership_error(error: std::io::Error) -> crate::PluginError {
    crate::PluginError::Session(format!("worker ownership: {error}"))
}

/// Keep the observed terminal locally until its durable write succeeds.
/// No cancellation receipt is acknowledged before that write.
async fn publish_terminal(ledger: Ledger, state: watch::Sender<WorkerState>, next: WorkerState) {
    loop {
        match ledger.finish(next.clone()) {
            Ok(retained) => {
                let _ = state.send(retained);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "worker terminal ownership write failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn supervise_adopted(
    process_id: ProcessId,
    worker: WorkerIdentity,
    adopted: Option<Adopted>,
    ledger: Ledger,
    kill: Arc<Notify>,
    state: watch::Sender<WorkerState>,
) {
    let mut requested = false;
    loop {
        // A terminal committed by the original supervisor remains authoritative.
        match ledger.lock().and_then(|locked| locked.read()) {
            Ok(Some(record)) if record.state.is_terminal() => {
                let _ = state.send(record.state);
                return;
            }
            Err(error) => tracing::warn!(%error, "worker ownership read failed"),
            _ => {}
        }
        match adopted.as_ref().map(Adopted::ended).transpose() {
            Ok(None | Some(true)) => {
                // pidfd readability observes death, including zombies. A
                // receipt also waits for the original parent or init to reap.
                match worker.matches() {
                    Ok(true) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "worker reap observation failed");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    Ok(false) => {}
                }
                let next = if requested {
                    WorkerState::Reaped(WorkerTerminationReceipt {
                        process_id,
                        worker_pid: worker.pid,
                    })
                } else {
                    WorkerState::Exited {
                        pid: worker.pid,
                        output: Box::new(lost_output()),
                    }
                };
                publish_terminal(ledger, state, next).await;
                return;
            }
            Err(error) => tracing::warn!(%error, "worker exit observation failed"),
            _ => {}
        }
        tokio::select! {
            () = kill.notified() => {
                if let Some(adopted) = &adopted {
                    match adopted.kill() {
                        Ok(()) => requested = true,
                        Err(error) => tracing::warn!(%error, "adopted worker termination failed"),
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
}

fn lost_output() -> ProcessAwaitOutput {
    ProcessAwaitOutput::Abandoned {
        evidence: Box::new(super::AbandonEvidence {
            writer: super::AbandonWriter::ResumeRefused {
                reason: super::ProcessResumeRefusal::SubstrateLost,
            },
            owner: None,
            epoch_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default(),
        }),
        control: None,
    }
}

fn exit_output(
    status: std::io::Result<std::process::ExitStatus>,
    stdout: &[u8],
) -> ProcessAwaitOutput {
    let output = match status {
        Ok(status) if status.success() => {
            crate::ToolCallOutput::success(serde_json::from_slice(stdout).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(stdout).into_owned())
            }))
        }
        Ok(status) => crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Execution,
            "worker_process_failed",
            format!("the worker process ended with {status}"),
        )),
        Err(error) => crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Execution,
            "worker_process_failed",
            format!("the worker process could not be awaited: {error}"),
        )),
    };
    ProcessAwaitOutput::from_tool_output(output)
}

fn terminated_output() -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
        crate::ToolCancellation::runtime("the worker process was terminated"),
    ))
}

fn supervisor_gone(process_id: &ProcessId) -> crate::PluginError {
    crate::PluginError::Session(format!("process `{process_id}`'s worker supervisor ended"))
}

impl Drop for WorkerProcessEngine {
    fn drop(&mut self) {
        for slot in self.workers.lock_recover().values() {
            if matches!(*slot.state.borrow(), WorkerState::Running { .. }) {
                slot.kill.notify_one();
            }
        }
    }
}

#[async_trait::async_trait]
impl PhysicalProcessWorker for WorkerProcessEngine {
    async fn terminate_worker(
        &self,
        process_id: &ProcessId,
    ) -> Result<WorkerTerminationReceipt, crate::PluginError> {
        let mut spawned = self.spawned.subscribe();
        self.restore_worker(process_id)?;
        let deadline = tokio::time::Instant::now() + self.spawn_wait;
        let (mut state, kill) = loop {
            if let Some(slot) = self.workers.lock_recover().get(process_id) {
                break (slot.state.clone(), Arc::clone(&slot.kill));
            }
            if !matches!(
                tokio::time::timeout_at(deadline, spawned.changed()).await,
                Ok(Ok(()))
            ) {
                return Err(crate::PluginError::Session(format!(
                    "process `{process_id}` has no worker on this host to terminate"
                )));
            }
        };
        loop {
            let current = state.borrow_and_update().clone();
            match current {
                WorkerState::Reaped(receipt) => return Ok(receipt),
                WorkerState::Exited { pid, .. } => {
                    return Ok(WorkerTerminationReceipt {
                        process_id: process_id.clone(),
                        worker_pid: pid,
                    });
                }
                WorkerState::Running { .. } => kill.notify_one(),
            }
            state
                .changed()
                .await
                .map_err(|_| supervisor_gone(process_id))?;
        }
    }
}

#[async_trait::async_trait]
impl ProcessEngine for WorkerProcessEngine {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn physical_worker(&self) -> Option<&dyn PhysicalProcessWorker> {
        if cfg!(target_os = "linux") {
            Some(self)
        } else {
            None
        }
    }

    async fn run(
        &self,
        context: ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<ProcessRunOutcome, ProcessInfraError> {
        let process_id = context.process_id().clone();
        let (mut state, kill) = self.start_worker(
            &process_id,
            context.registration().start_key.as_ref(),
            &payload,
        )?;
        let cancellation = context.cancellation_token();
        let mut kill_requested = false;
        loop {
            let current = state.borrow_and_update().clone();
            match current {
                WorkerState::Exited { mut output, .. } => {
                    // The ledger owns the physical observation; the replayed
                    // process admission supplies its execution owner evidence.
                    if let ProcessAwaitOutput::Abandoned { evidence, .. } = output.as_mut() {
                        evidence.owner = context
                            .execution_context()
                            .execution_write_authority
                            .as_ref()
                            .map(|authority| authority.owner_identity());
                    }
                    return Ok((*output).into());
                }
                WorkerState::Reaped(_) => return Ok(terminated_output().into()),
                WorkerState::Running { .. } => {}
            }
            tokio::select! {
                changed = state.changed() => {
                    changed.map_err(|_| supervisor_gone(&process_id))?;
                }
                () = cancellation.cancelled(), if !kill_requested => {
                    kill.notify_one();
                    kill_requested = true;
                }
            }
        }
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the worker engine `{}` stores no artifact `{artifact_ref}`",
            self.kind
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L08: a physical worker's termination kills and reaps the one worker
    /// its process runs, and a repeated termination recovers the same
    /// receipt without touching another worker.
    #[tokio::test]
    async fn terminate_worker_kills_reaps_and_repeats_one_receipt() {
        let ownership = tempfile::tempdir().unwrap();
        let engine = WorkerProcessEngine::new(
            "worker-engine-law",
            WorkerCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exec sleep 600".into()],
            },
            ownership.path().to_path_buf(),
        );
        let process = crate::ProcessId::fixture("worker-engine-law");
        let (state, _) = engine
            .start_worker(&process, None, &serde_json::json!({"job": "law"}))
            .unwrap();
        assert!(matches!(*state.borrow(), WorkerState::Running { .. }));
        let (again, _) = engine
            .start_worker(&process, None, &serde_json::json!({"job": "law"}))
            .unwrap();
        assert_eq!(
            engine.workers.lock_recover().len(),
            1,
            "one worker per process"
        );
        let first = engine.terminate_worker(&process).await.unwrap();
        let second = engine.terminate_worker(&process).await.unwrap();
        assert_eq!(
            first, second,
            "a repeated termination names the same worker"
        );
        assert_eq!(first.process_id, process);
        assert!(
            !std::path::Path::new(&format!("/proc/{}", first.worker_pid)).exists(),
            "the worker was killed and reaped"
        );
        assert!(matches!(*again.borrow(), WorkerState::Reaped(ref receipt) if *receipt == first));
    }
}
