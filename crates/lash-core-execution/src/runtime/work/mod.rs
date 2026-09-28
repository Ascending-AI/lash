//! The deployment ports an engine serves work through: session drives
//! ([`SessionWorkEngine`], [`SessionDriver`]) and durable processes
//! ([`ProcessWorkSubstrate`], [`ProcessWorkWiring`]), with the engine-neutral
//! pieces every engine shares: the registry awaiter, the wake-delivery driver
//! and their pacing.

use std::sync::Arc;

mod awaiter;
mod cadence;
mod wake_delivery;

pub use awaiter::ProcessRegistryAwaiter;
pub use cadence::{WorkCadenceError, WorkCadencePolicy};
pub use wake_delivery::{WakeDeliveryDriveReport, WakeDeliveryDriver};

use super::process::{ProcessRegistry, WatchedRegistry};
use crate::{PluginError, ProcessAwaitOutput, SessionId};

/// Deployment port for **session work** (ADR 0104 O1/O2, FIG-3600): the
/// engine that runs each session's drive.
///
/// Acceptance is the store's: an item is durable before anyone is told about
/// it, and its admission transaction records the drive it owes as an ingress
/// obligation (ADR 0109 §3). The producer then asks the engine for that drive
/// through [`request_drive`](Self::request_drive); the obligation relay
/// retries an ask that did not reach the engine. The engine serializes
/// drives per session (one authorized drive at a time) and dedupes a request
/// id across its runs, so a repeated ask for the same request never drives
/// twice, and a drive admits whatever is pending, not only the item that
/// asked.
///
/// The engine runs the kernel's drive through the [`SessionDriver`] the core
/// installs; it never decides what a drive admits.
#[async_trait::async_trait]
pub trait SessionWorkEngine: Send + Sync {
    /// Ask the engine to drive `session` for `request`. Returns once the ask
    /// is handed to the engine, not once the drive ran.
    fn schedule_drive(&self, session: &SessionId, request: crate::engine::DriveRequestId);

    /// Ask the engine to drive `session` for `request` and answer once the
    /// engine accepted the ask (ADR 0109): the delivery of an obligation
    /// whose effect is a drive, never fire-and-forget. Idempotent under a
    /// repeated `request`: the engine dedupes it as
    /// [`schedule_drive`](Self::schedule_drive) does. A refusal is the
    /// obligation's attempt failing; its relay retries it.
    ///
    /// The default accepts the ask once it is scheduled.
    async fn request_drive(
        &self,
        session: &SessionId,
        request: crate::engine::DriveRequestId,
    ) -> Result<(), crate::engine::EngineRefusal> {
        self.schedule_drive(session, request);
        Ok(())
    }

    /// Install the core's drive: get-or-init. One engine can back several
    /// cores, and exactly one driver serves it, so a caller hands in a
    /// candidate and uses whatever comes back (the precedent is
    /// [`EffectHost::install_tool_child_host`](crate::EffectHost::install_tool_child_host)).
    ///
    /// The caller keeps what comes back for as long as it serves drives. It
    /// may be an installation wrapping the driver
    /// ([`SessionDriver::runs_on`] tells whose), whose life, not that of a
    /// drive still running on the driver, decides whether the install holds
    /// (FIG-4017).
    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver>;

    /// The engine half of the control verbs over this engine's executions
    /// (FIG-3600 S7). An engine that holds no execution across calls has
    /// nothing to release.
    fn control(&self) -> Arc<dyn crate::engine::SessionControlEngine> {
        Arc::new(crate::engine::NoEngineControl)
    }

    /// Wait until a drive of `session` that began after `request` was
    /// scheduled has stopped, and answer how it stopped.
    ///
    /// Idempotent, and it never drives twice for one request id: an ask the
    /// engine lost is re-issued under the same id. This is a **wake
    /// barrier**, not the resolution of anything the request followed: a
    /// drive may stop before an input's root settled (another driver holds
    /// it, or the root parked), so a caller reads the outcome from the store
    /// and uses this only to learn that a drive ran, or that the engine
    /// refused one.
    ///
    /// The default is an engine that runs no drives: it refuses with
    /// [`SessionWorkUnavailable`](crate::RuntimeErrorCode::SessionWorkUnavailable).
    async fn await_drive(
        &self,
        session: &SessionId,
        request: &crate::engine::DriveRequestId,
    ) -> Result<crate::engine::DriveOutcome, crate::engine::DriveAbort> {
        Err(session_work_unavailable(session, request))
    }
}

/// The refusal of an engine that runs no drives, asked to wait for one.
fn session_work_unavailable(
    session: &SessionId,
    request: &crate::engine::DriveRequestId,
) -> crate::engine::DriveAbort {
    crate::engine::DriveAbort::Refused(crate::RuntimeError::new(
        crate::RuntimeErrorCode::SessionWorkUnavailable,
        format!(
            "drive `{}` of session `{session}` cannot be awaited: this deployment runs no session work",
            request.as_str()
        ),
    ))
}

/// The kernel's drive of one session, as the core installs it on its
/// [`SessionWorkEngine`].
///
/// The engine splits the drive over its own handlers: it calls
/// [`admit`](Self::admit) from its per-session handler and
/// [`run_root`](Self::run_root) from its per-root handler, each on a
/// controller over that handler's own journal.
#[async_trait::async_trait]
pub trait SessionDriver: Send + Sync {
    /// Whether this driver owns the deployment recovery pass.
    fn owns_reconciliation(&self) -> bool {
        false
    }

    /// Whether the drives this driver serves run on `driver`: it is `driver`
    /// itself, or an engine's installation of it
    /// ([`SessionWorkEngine::install_session_driver`]).
    fn runs_on(&self, driver: &dyn SessionDriver) -> bool {
        std::ptr::addr_eq(self, driver)
    }

    /// One bounded recovery pass, invoked on the engine's own schedule.
    async fn reconcile(
        &self,
        _cursor: &crate::engine::ReconcileCursor,
        _page: std::num::NonZeroUsize,
    ) -> Result<crate::engine::ReconcileCursor, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionDriver::reconcile",
        })
    }

    /// Admission `ordinal` of `request`: one recorded `AdmitDrive` step
    /// through `controller`, which serves
    /// [`drive_admission_scope`](crate::engine::drive_admission_scope).
    async fn admit(
        &self,
        controller: crate::ScopedEffectController<'_>,
        request: &crate::engine::DriveRequest,
        ordinal: u32,
    ) -> Result<crate::engine::AdmitVerdict, crate::engine::DriveAbort>;

    /// Run `admitted`'s root to its terminal through `controller`, which
    /// serves [`drive_root_scope`](crate::engine::drive_root_scope): the
    /// recorded `SealDriveAdmission` step, then the root's turns and commits.
    async fn run_root(
        &self,
        controller: crate::ScopedEffectController<'_>,
        admitted: crate::engine::Admitted,
    ) -> Result<crate::engine::RootOutcome, crate::engine::DriveAbort>;
}

/// Deployment port for durable process work.
#[async_trait::async_trait]
pub trait ProcessWorkSubstrate: Send + Sync {
    /// Submit `record`'s registered process to the engine: the delivery of
    /// its `ProcessStart` obligation (ADR 0109). The relay supplies the armed
    /// row's record as of the claim, already filtered of terminal and
    /// externally owned processes; a workflow engine coalesces a repeated
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

    /// Wake `process_id`'s live execution so it hands its open signal wait
    /// to a successor on the newest build (FIG-3799): the drain of
    /// `generation` asks it of every process waiting on that generation.
    /// Only an execution admitted under `generation` hands over; one on
    /// another generation keeps waiting. Idempotent: a repeated wake of the
    /// same execution is a no-op, and a wake that lands while the execution
    /// is not waiting holds for its next wait.
    ///
    /// An engine that routes no work by build generation has nothing to hand
    /// over to and refuses.
    async fn deliver_hand_over(
        &self,
        process_id: &crate::ProcessId,
        generation: &crate::engine::BuildGeneration,
    ) -> Result<(), PluginError> {
        Err(PluginError::Invoke(format!(
            "this engine routes no work by build generation, so it cannot hand \
             process `{process_id}` over from generation {}",
            generation.as_str()
        )))
    }

    /// Publish `process`'s stored terminal `output` to the engine's waiters
    /// under `key`: the delivery of its `ProcessTerminal` obligation (ADR
    /// 0109 §3). `key` is the obligation's stable dedupe identity; a repeat
    /// must be a no-op, and a terminal already published stays as it was.
    ///
    /// The engine's waiters wait on the engine (Restate's in-journal awaits
    /// on the process's terminal promise), so it resolves them here. A port
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

    pub fn event_awaiter(&self) -> &ProcessRegistryAwaiter {
        &self.event_awaiter
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

/// Explicit session-work engine for deployments that run no drives: an ask
/// is dropped (the rows stay pending for a host that drives them itself), and
/// the driver a core installs is kept only so the get-or-init answer holds.
#[derive(Default)]
pub struct NoSessionWork {
    driver: std::sync::OnceLock<Arc<dyn SessionDriver>>,
}

impl NoSessionWork {
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for NoSessionWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NoSessionWork")
    }
}

impl SessionWorkEngine for NoSessionWork {
    fn schedule_drive(&self, session: &SessionId, request: crate::engine::DriveRequestId) {
        tracing::trace!(
            session_id = session.as_str(),
            request = request.as_str(),
            "session drive not scheduled: deployment runs no session work"
        );
    }

    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        Arc::clone(self.driver.get_or_init(|| driver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe;

    #[async_trait::async_trait]
    impl SessionDriver for Probe {
        async fn admit(
            &self,
            _controller: crate::ScopedEffectController<'_>,
            _request: &crate::engine::DriveRequest,
            _ordinal: u32,
        ) -> Result<crate::engine::AdmitVerdict, crate::engine::DriveAbort> {
            unreachable!("the probe never admits")
        }

        async fn run_root(
            &self,
            _controller: crate::ScopedEffectController<'_>,
            _admitted: crate::engine::Admitted,
        ) -> Result<crate::engine::RootOutcome, crate::engine::DriveAbort> {
            unreachable!("the probe never runs a root")
        }
    }

    #[test]
    fn no_session_work_drops_asks_and_keeps_the_first_installed_driver() {
        let engine = NoSessionWork::new();
        engine.schedule_drive(
            &SessionId::from("deferred-session"),
            crate::engine::DriveRequestId::new("ask"),
        );
        let first: Arc<dyn SessionDriver> = Arc::new(Probe);
        let second: Arc<dyn SessionDriver> = Arc::new(Probe);
        let installed = engine.install_session_driver(Arc::clone(&first));
        assert!(Arc::ptr_eq(&installed, &first));
        let again = engine.install_session_driver(second);
        assert!(
            Arc::ptr_eq(&again, &first),
            "install is get-or-init: the first driver stays"
        );
    }
}
