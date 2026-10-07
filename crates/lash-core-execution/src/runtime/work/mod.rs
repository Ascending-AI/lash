//! The deployment port durable processes are served through
//! ([`ProcessWorkSubstrate`], [`ProcessWorkWiring`]), with the pieces every
//! deployment shares: the registry awaiter, the wake-delivery driver and
//! their pacing. Session work has no port: a producer wakes the session
//! actor in its own transaction ([`crate::Backend::wake_session`] outside
//! one).

use std::sync::Arc;

mod awaiter;
mod cadence;
mod durable;

pub use awaiter::ProcessRegistryAwaiter;
pub use cadence::{WorkCadenceError, WorkCadencePolicy};
pub use durable::DurableProcessWork;

use super::process::{ProcessRegistry, WatchedRegistry};
use crate::{PluginError, ProcessAwaitOutput};

/// Deployment port for durable process work.
#[async_trait::async_trait]
pub trait ProcessWorkSubstrate: Send + Sync {
    /// Submit `record`'s registered process to the engine: the delivery of
    /// its `ProcessStart` obligation (ADR 0109). The relay supplies the armed
    /// row's record as of the claim, already filtered of terminal processes;
    /// a workflow engine coalesces a repeated
    /// send on the process's workflow key.
    async fn deliver_process_start(&self, record: &crate::ProcessRecord)
    -> Result<(), PluginError>;

    /// There is no polling fallback and no "attach if provided". [`ProcessTerminalWait::Reattach`]
    /// is recoverable: the port bounded one transport attachment while the
    /// durable wait stayed live, so the caller re-enters with the same explicit
    /// `process_id`. The caller owns the
    /// overall wait bound through its cancellation select.
    async fn await_process_terminal(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError>;

    /// Deliver `request` to `process`'s live execution under `key`, so the
    /// running segment observes the cancel. `key` is the caller's stable
    /// dedupe identity for this delivery; a retry under the same key must be
    /// a no-op. The engine posts the cancel into the execution and dedupes
    /// on `key`.
    async fn deliver_cancel(
        &self,
        process_id: &crate::ProcessId,
        request: &crate::CancelRequest,
        key: &str,
    ) -> Result<(), PluginError>;

    /// Publish `process`'s stored terminal `output` to the engine's waiters
    /// under `key`: the delivery of its `ProcessTerminal` obligation (ADR
    /// 0109 §3). `key` is the obligation's stable dedupe identity; a repeat
    /// must be a no-op, and a terminal already published stays as it was.
    ///
    /// The engine's waiters wait on the engine, so it resolves them here. A port
    /// that wraps another forwards it, or the relay settles publications no
    /// waiter ever saw.
    async fn publish_process_terminal(
        &self,
        process_id: &crate::ProcessId,
        output: &crate::ProcessAwaitOutput,
        key: &str,
    ) -> Result<(), PluginError>;
}

/// Outcome of one bounded terminal wait.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ProcessTerminalWait {
    /// The process reached a terminal state.
    Terminal(ProcessAwaitOutput),
    /// The bounded transport attachment aged out; retry with the same id.
    Reattach,
}

/// The unit of process-work composition: one watched registry and the port
/// bound to it.
#[derive(Clone)]
pub struct ProcessWorkWiring {
    watched: WatchedRegistry,
    port: Arc<dyn ProcessWorkSubstrate>,
    event_awaiter: ProcessRegistryAwaiter,
    runs_processes: bool,
}

impl ProcessWorkWiring {
    /// Pair a watched registry and its change hub with the process port bound
    /// to exactly that handle. This constructs core's one event awaiter; the
    /// caller that created the port owns the pairing contract.
    pub fn new(watched: WatchedRegistry, port: Arc<dyn ProcessWorkSubstrate>) -> Self {
        let event_awaiter =
            ProcessRegistryAwaiter::new(Arc::clone(watched.registry()), watched.hub().clone());
        Self {
            watched,
            port,
            event_awaiter,
            runs_processes: true,
        }
    }

    /// The wiring of an engine that runs no processes over `registry`: its
    /// port is [`NoProcessWork`].
    pub fn without_process_work(registry: Arc<dyn ProcessRegistry>) -> Self {
        let watched = super::process::watch_process_registry(registry);
        let port = Arc::new(NoProcessWork::new(&watched));
        Self {
            runs_processes: false,
            ..Self::new(watched, port)
        }
    }

    /// Whether an engine runs processes through this wiring's port; `false`
    /// for [`Self::without_process_work`].
    pub fn runs_processes(&self) -> bool {
        self.runs_processes
    }

    /// Pace the event awaiter on `work_cadence`.
    pub fn with_work_cadence(
        mut self,
        work_cadence: WorkCadencePolicy,
    ) -> Result<Self, WorkCadenceError> {
        work_cadence.validate()?;
        self.event_awaiter = self.event_awaiter.with_work_cadence(work_cadence);
        Ok(self)
    }

    pub fn registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.watched.registry()
    }

    pub fn watched(&self) -> &WatchedRegistry {
        &self.watched
    }

    pub fn port(&self) -> &Arc<dyn ProcessWorkSubstrate> {
        &self.port
    }
}

/// The process port of an engine that runs no processes: it admits nothing,
/// and a wait on a process reads the registry, so a process some other
/// deployment runs is still observed to its terminal. A cancel is delivered
/// by the registry write its caller makes, and a terminal is published by its
/// commit.
#[derive(Clone)]
pub struct NoProcessWork {
    terminal_awaiter: ProcessRegistryAwaiter,
}

impl NoProcessWork {
    /// No process work over `watched`.
    pub fn new(watched: &WatchedRegistry) -> Self {
        Self {
            terminal_awaiter: ProcessRegistryAwaiter::new(
                Arc::clone(watched.registry()),
                watched.hub().clone(),
            ),
        }
    }

    /// No process work over an unwatched `registry`: its waits poll.
    pub fn for_registry(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            terminal_awaiter: ProcessRegistryAwaiter::for_registry(registry),
        }
    }

    /// Wait for `process_id`'s terminal output in the registry.
    pub async fn await_terminal(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        self.terminal_awaiter.await_terminal(process_id).await
    }

    /// Wait for `process_id`'s first `event_type` event after
    /// `after_sequence` in the registry.
    pub async fn await_event(
        &self,
        process_id: &crate::ProcessId,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.terminal_awaiter
            .await_event(process_id, event_type, after_sequence)
            .await
    }
}

impl std::fmt::Debug for NoProcessWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NoProcessWork")
    }
}

#[async_trait::async_trait]
impl ProcessWorkSubstrate for NoProcessWork {
    async fn deliver_process_start(
        &self,
        record: &crate::ProcessRecord,
    ) -> Result<(), PluginError> {
        Err(PluginError::Invoke(format!(
            "this engine cannot start process `{}`",
            record.id
        )))
    }

    async fn await_process_terminal(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError> {
        self.terminal_awaiter
            .await_terminal(process_id)
            .await
            .map(ProcessTerminalWait::Terminal)
    }

    async fn deliver_cancel(
        &self,
        _process_id: &crate::ProcessId,
        _request: &crate::CancelRequest,
        _key: &str,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    async fn publish_process_terminal(
        &self,
        _process_id: &crate::ProcessId,
        _output: &crate::ProcessAwaitOutput,
        _key: &str,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}
