//! The deployment ports an engine serves work through: session shifts
//! ([`SessionWorkEngine`], [`SessionShifts`]) and durable processes
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
/// engine that runs each session's shift.
///
/// Acceptance is the store's: an item is durable before anyone is told about
/// it, and its admission transaction records the shift it owes as an ingress
/// obligation (ADR 0109 §3). The producer then asks the engine for that execute
/// through [`request_shift`](Self::request_shift); the obligation relay
/// retries an ask that did not reach the engine. The engine serializes
/// executes per session (one authorized shift at a time) and dedupes a request
/// id across its runs, so a repeated ask for the same request never executes
/// twice, and a shift admits whatever is pending, not only the item that
/// asked.
///
/// The engine runs the kernel's shift through the [`SessionShifts`] the core
/// installs; it never decides what a shift admits.
#[async_trait::async_trait]
pub trait SessionWorkEngine: Send + Sync {
    /// Ask the engine to work `session` for `request`. Returns once the ask
    /// is handed to the engine, not once the shift ran.
    fn schedule_shift(&self, session: &SessionId, request: crate::engine::ShiftRequestId);

    /// Ask the engine to work `session` for `request` and answer once the
    /// engine accepted the ask (ADR 0109): the delivery of an obligation
    /// whose effect is a shift, never fire-and-forget. Idempotent under a
    /// repeated `request`: the engine dedupes it as
    /// [`schedule_shift`](Self::schedule_shift) does. A refusal is the
    /// obligation's attempt failing; its relay retries it.
    ///
    /// The default accepts the ask once it is scheduled.
    async fn request_shift(
        &self,
        session: &SessionId,
        request: crate::engine::ShiftRequestId,
    ) -> Result<(), crate::engine::EngineRefusal> {
        self.schedule_shift(session, request);
        Ok(())
    }

    /// Install the core's shift: get-or-init. One engine can back several
    /// cores, and exactly one `SessionShifts` serves it, so a caller hands in a
    /// candidate and uses whatever comes back.
    ///
    /// The caller keeps what comes back for as long as it serves shifts. It
    /// may be an installation wrapping the `SessionShifts`
    /// ([`SessionShifts::runs_on`] tells whose), whose life, not that of a
    /// shift still running on the `SessionShifts`, decides whether the install holds
    /// (FIG-4017).
    fn install_session_shifts(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts>;

    /// The engine half of the control verbs over this engine's executions
    /// (FIG-3600 S7). An engine that holds no execution across calls has
    /// nothing to release.
    fn control(&self) -> Arc<dyn crate::engine::SessionControlEngine> {
        Arc::new(crate::engine::NoEngineControl)
    }

    /// Wait until a shift of `session` that began after `request` was
    /// scheduled has stopped, and answer how it stopped.
    ///
    /// Idempotent, and it never executes twice for one request id: an ask the
    /// engine lost is re-issued under the same id. This is a **wake
    /// barrier**, not the resolution of anything the request followed: a
    /// shift may stop before an input's run settled (another `SessionShifts` holds
    /// it, or the run parked), so a caller reads the outcome from the store
    /// and uses this only to learn that a shift ran, or that the engine
    /// refused one.
    ///
    /// The default is an engine that runs no shifts: it refuses with
    /// [`SessionWorkUnavailable`](crate::RuntimeErrorCode::SessionWorkUnavailable).
    async fn await_shift(
        &self,
        session: &SessionId,
        request: &crate::engine::ShiftRequestId,
    ) -> Result<crate::engine::ShiftOutcome, crate::engine::ShiftAbort> {
        Err(session_work_unavailable(session, request))
    }
}

/// The refusal of an engine that runs no shifts, asked to wait for one.
fn session_work_unavailable(
    session: &SessionId,
    request: &crate::engine::ShiftRequestId,
) -> crate::engine::ShiftAbort {
    crate::engine::ShiftAbort::Refused(crate::RuntimeError::new(
        crate::RuntimeErrorCode::SessionWorkUnavailable,
        format!(
            "shift `{}` of session `{session}` cannot be awaited: this deployment runs no session work",
            request.as_str()
        ),
    ))
}

/// The kernel's shift of one session, as the core installs it on its
/// [`SessionWorkEngine`].
///
/// The engine splits the shift over its own handlers: it calls
/// [`admit`](Self::admit) from its per-session handler and
/// [`execute_run`](Self::execute_run) from its per-run handler, each on a
/// controller over that handler's own journal.
#[async_trait::async_trait]
pub trait SessionShifts: Send + Sync {
    /// Whether this `SessionShifts` owns the deployment recovery pass.
    fn owns_reconciliation(&self) -> bool {
        false
    }

    /// Whether the shifts this `SessionShifts` serves run on `shifts`: it is `shifts`
    /// itself, or an engine's installation of it
    /// ([`SessionWorkEngine::install_session_shifts`]).
    fn runs_on(&self, shifts: &dyn SessionShifts) -> bool {
        std::ptr::addr_eq(self, shifts)
    }

    /// One bounded recovery pass, invoked on the engine's own schedule.
    async fn reconcile(
        &self,
        _cursor: &crate::engine::ReconcileCursor,
        _page: std::num::NonZeroUsize,
    ) -> Result<crate::engine::ReconcileCursor, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionShifts::reconcile",
        })
    }

    /// Hold `session`'s runtime open for one attempt of one shift
    /// invocation (FIG-3825).
    ///
    /// The engine takes the hold before the attempt's first admission and
    /// drops it when the attempt ends. While it is held, every run the
    /// attempt calls that runs in this process shares one runtime, opened by
    /// the first of them. An attempt ends where the engine stops polling
    /// it, so nothing held crosses into the next attempt: that one opens
    /// the session afresh, as a redrive in a fresh process does. An
    /// admission runs on no runtime, so a replayed one never waits for a
    /// run still running on the held runtime (FIG-4729, FIG-4755). The
    /// default holds nothing.
    fn hold_shift(&self, _session: &SessionId) -> crate::engine::ShiftHold {
        crate::engine::ShiftHold::empty()
    }

    /// Admission `ordinal` of `request`: one recorded `AdmitShift` step
    /// through `controller`, which serves
    /// [`shift_admission_scope`](crate::engine::shift_admission_scope).
    /// The step reads the session's store and takes no runtime's writer:
    /// an admission never waits for a run running in this process
    /// (FIG-4755).
    ///
    /// `admitting_generation` is the generation of the build that
    /// admits, which the admitted run is stamped with (FIG-4742): an engine
    /// whose shift requests cross builds supplies its own generation independently
    /// of the request's intended lane.
    ///
    /// `draining` is the build generation whose drain this admission hands
    /// over for (FIG-4639, ADR 0106 §1): the generation of the build the
    /// engine's shift invocation is pinned to, named for every admission
    /// after the run the invocation's shift started on. When that
    /// generation is marked draining and work is pending, the step admits
    /// nothing and records
    /// [`AdmitVerdict::Draining`](crate::engine::AdmitVerdict::Draining).
    /// `None` admits whatever drains: the shift's first admission, whose
    /// run always runs on the build that took it.
    async fn admit(
        &self,
        controller: crate::ScopedEffectController<'_>,
        request: &crate::engine::ShiftRequest,
        admitting_generation: &crate::engine::BuildGeneration,
        ordinal: u32,
        draining: Option<&crate::engine::BuildGeneration>,
    ) -> Result<crate::engine::AdmitVerdict, crate::engine::ShiftAbort>;

    /// Run `admitted`'s run to its terminal through `controller`, which
    /// serves [`shift_run_scope`](crate::engine::shift_run_scope): the
    /// recorded `SealShiftAdmission` step, then the run's turns and commits.
    ///
    /// The run does not close the run's scope. When it made the run's
    /// terminal evidence durable, [`RunEnd::owed_close`] names the run
    /// whose close the engine then runs through
    /// [`close_run`](Self::close_run) (FIG-4035).
    ///
    /// [`RunEnd::owed_close`]: crate::engine::RunEnd::owed_close
    async fn execute_run(
        &self,
        controller: crate::ScopedEffectController<'_>,
        admitted: crate::engine::Admitted,
    ) -> crate::engine::RunEnd;

    /// Close the lifetime scope of `run` of `session`, whose terminal
    /// evidence is durable: the run's recorded `CloseRunScope` step
    /// through `controller`, which serves the
    /// [`shift_run_scope`](crate::engine::shift_run_scope) of the admitted
    /// run whose execution owed it (FIG-4035).
    ///
    /// It opens no runtime of the session, so it runs beside the session's
    /// next run: the engine runs it in a journal of its own, never one the
    /// session's shift awaits. The close is the immediate delivery of the
    /// `ScopeClose` obligation the terminal commit armed (ADR 0109 §3), so a
    /// close this never runs is still delivered once, by the relay.
    async fn close_run(
        &self,
        controller: crate::ScopedEffectController<'_>,
        session: &SessionId,
        run: &crate::TurnId,
    ) -> Result<(), crate::engine::ShiftAbort>;
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
    /// A process whose next execution the newest build refused, parked for
    /// `generation`, has no live execution to wake: an engine that routes by
    /// generation sends that execution to a build of `generation` instead
    /// ([`Self::resend_refused_successor`]), and leaves an execution already
    /// running there to finish.
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

    /// Send `process_id`'s next execution, which the newest build refused, to
    /// a build of the generation that sent it (FIG-4750): the generation its
    /// park names. `true` when it was sent.
    ///
    /// The refusal is the whole reason to send: the newest build cannot run
    /// the execution, and a build of the sender's generation can, whether or
    /// not that generation is draining and whichever build holds the
    /// recovery lease (FIG-4739). `false` when the process is not such a
    /// refusal: it is not parked for another generation, its park names an
    /// execution the engine still holds — one that stopped on its own
    /// journal and is its engine's to resume, never a second send's — or its
    /// next execution already started. Idempotent: a repeated send names the
    /// first. When no build of the generation is left the call is refused
    /// typed and the park stands.
    ///
    /// An engine that routes no work by build generation refuses nothing and
    /// has nothing to send.
    async fn resend_refused_successor(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<bool, PluginError> {
        let _ = process_id;
        Ok(false)
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

/// Explicit session-work engine for deployments that run no shifts: an ask
/// is dropped (the rows stay pending for a host that executes them itself), and
/// the `SessionShifts` a core installs is kept only so the get-or-init answer holds.
#[derive(Default)]
pub struct NoSessionWork {
    shifts: std::sync::OnceLock<Arc<dyn SessionShifts>>,
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
    fn schedule_shift(&self, session: &SessionId, request: crate::engine::ShiftRequestId) {
        tracing::trace!(
            session_id = session.as_str(),
            request = request.as_str(),
            "session shift not scheduled: deployment runs no session work"
        );
    }

    fn install_session_shifts(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts> {
        Arc::clone(self.shifts.get_or_init(|| shifts))
    }
}
