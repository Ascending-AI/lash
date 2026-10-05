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
//! The engine owns its live workers. Each worker's child handle belongs to
//! one supervisor task, not to the engine's `run` future, so a dropped or
//! redelivered run never orphans its worker or starts a second one for the
//! same process. Workers live on this host: after a host crash the engine
//! that owns process recovery redelivers `run` (ADR 0110), and a fresh worker
//! starts.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
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

#[derive(Clone, Debug)]
enum WorkerState {
    Running,
    /// The worker ended by itself and was reaped.
    Exited {
        pid: NonZeroU32,
        output: Box<ProcessAwaitOutput>,
    },
    /// The worker was killed on request and reaped.
    Reaped(WorkerTerminationReceipt),
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
/// call: the engine that ran a worker is the one that can terminate it.
pub struct WorkerProcessEngine {
    kind: &'static str,
    command: WorkerCommand,
    spawn_wait: Duration,
    workers: Mutex<BTreeMap<ProcessId, Slot>>,
    spawned: watch::Sender<u64>,
}

impl WorkerProcessEngine {
    #[must_use]
    pub fn new(kind: &'static str, command: WorkerCommand) -> Self {
        Self {
            kind,
            command,
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
        tokio::process::Command::new(&self.command.program)
            .args(&self.command.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }

    /// The worker of `process_id`: the live one this engine already runs for
    /// it, else a newly spawned one fed `payload`.
    fn start_worker(
        &self,
        process_id: &ProcessId,
        payload: &serde_json::Value,
    ) -> Result<(watch::Receiver<WorkerState>, Arc<Notify>), crate::PluginError> {
        let mut workers = self.workers.lock_recover();
        if let Some(slot) = workers.get(process_id) {
            return Ok((slot.state.clone(), Arc::clone(&slot.kill)));
        }
        let input = serde_json::to_vec(payload)
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        let mut child = self.spawn_child().map_err(|error| {
            crate::PluginError::Session(format!(
                "worker program `{}` failed to start: {error}",
                self.command.program.display()
            ))
        })?;
        let pid = child.id().and_then(NonZeroU32::new).ok_or_else(|| {
            crate::PluginError::Session("the spawned worker has no process id".to_owned())
        })?;
        let (state, receiver) = watch::channel(WorkerState::Running);
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
) {
    let exited = tokio::select! {
        biased;
        () = kill.notified() => None,
        status = child.wait() => Some(status),
    };
    let next = match exited {
        None => {
            // `kill` sends SIGKILL and waits: the worker is reaped.
            let _ = child.kill().await;
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
    let _ = state.send(next);
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
            if matches!(*slot.state.borrow(), WorkerState::Running) {
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
                WorkerState::Running => kill.notify_one(),
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
        Some(self)
    }

    async fn run(
        &self,
        context: ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<ProcessRunOutcome, ProcessInfraError> {
        let process_id = context.process_id().clone();
        let (mut state, kill) = self.start_worker(&process_id, &payload)?;
        let cancellation = context.cancellation_token();
        let mut kill_requested = false;
        loop {
            let current = state.borrow_and_update().clone();
            match current {
                WorkerState::Exited { output, .. } => return Ok((*output).into()),
                WorkerState::Reaped(_) => return Ok(terminated_output().into()),
                WorkerState::Running => {}
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
        let engine = WorkerProcessEngine::new(
            "worker-engine-law",
            WorkerCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exec sleep 600".into()],
            },
        );
        let process = crate::ProcessId::fixture("worker-engine-law");
        let (state, _) = engine
            .start_worker(&process, &serde_json::json!({"job": "law"}))
            .unwrap();
        assert!(matches!(*state.borrow(), WorkerState::Running));
        let (again, _) = engine
            .start_worker(&process, &serde_json::json!({"job": "law"}))
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
