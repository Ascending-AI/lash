use std::sync::Arc;

mod awaiter;
pub use lash_core_effect::queued_lane_wait as lane_wait;
mod policy;
mod process_work;
mod wake_delivery;

pub use awaiter::NativeProcessAwaiter;
pub use policy::{
    NativeSubstrateConfig, NativeSubstrateConfigError, WorkCadencePolicy, WorkerSweepPolicy,
};
pub use process_work::{NativeProcessAdmissionDriver, NativeProcessWork};
pub use wake_delivery::{WakeDeliveryDriveReport, WakeDeliveryDriver};

use super::process::{ProcessAdmissionReport, ProcessRegistry, WatchedRegistry};
use crate::{PluginError, ProcessAwaitOutput, SessionId};

/// Deployment port for **session work** (ADR 0104 O1/O2, FIG-3600): the
/// engine that runs each session's drive.
///
/// Acceptance is the store's: an item is durable before anyone is told about
/// it. The engine is then asked to drive the session, fire-and-forget; it
/// serializes drives per session (one authorized drive at a time) and dedupes
/// a request id across its runs, so a repeated ask for the same request never
/// drives twice. A lost ask is healed by the session's next ask (a drive
/// admits whatever is pending, not only the item that asked) or by the
/// reconcile sweep, `drive::reconcile_session_work`; its production caller is
/// the engine-neutral reconcile pass (S7), not yet wired.
///
/// The engine runs the kernel's drive through the [`SessionDriver`] the core
/// installs; it never decides what a drive admits.
#[async_trait::async_trait]
pub trait SessionWorkEngine: Send + Sync {
    /// Ask the engine to drive `session` for `request`. Returns once the ask
    /// is handed to the engine, not once the drive ran.
    fn schedule_drive(&self, session: &SessionId, request: crate::engine::DriveRequestId);

    /// Install the core's drive: get-or-init. One engine can back several
    /// cores, and exactly one driver serves it, so a caller hands in a
    /// candidate and uses whatever comes back (the precedent is
    /// [`EffectHost::install_tool_child_host`](crate::EffectHost::install_tool_child_host)).
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
/// An engine that drives in process calls [`drive`](Self::drive). An engine
/// that splits the drive over its own handlers calls
/// [`admit`](Self::admit) from its per-session handler and
/// [`run_root`](Self::run_root) from its per-root handler, each on a
/// controller over that handler's own journal; the kernel bodies are the
/// same either way.
#[async_trait::async_trait]
pub trait SessionDriver: Send + Sync {
    /// Whether this driver owns the deployment recovery pass.
    fn owns_reconciliation(&self) -> bool {
        false
    }

    /// One bounded recovery pass, invoked by the engine's durable scheduler.
    async fn reconcile(
        &self,
        _cursor: &crate::engine::ReconcileCursor,
        _page: std::num::NonZeroUsize,
        _tick: &str,
    ) -> Result<crate::engine::ReconcileCursor, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionDriver::reconcile",
        })
    }

    /// Drive `request` to a stop on the driver's own effect host: admit,
    /// seal and run roots until admission answers something other than an
    /// admitted root.
    async fn drive(
        &self,
        request: crate::engine::DriveRequest,
    ) -> Result<crate::engine::DriveOutcome, crate::engine::DriveAbort>;

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

/// Deployment port for durable **process work**: admission of pending process
/// rows, and the only sanctioned way to wait on a started one (ADR 0016).
#[async_trait::async_trait]
pub trait ProcessWorkSubstrate: Send + Sync {
    /// Admit every pending (non-terminal) process this owner can take.
    /// Admission, not completion -- see [`ProcessAdmissionReport`].
    async fn admit_pending_processes(
        &self,
        reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError>;

    /// There is no polling fallback and no "attach if provided". [`ProcessTerminalWait::Reattach`]
    /// is recoverable: the port bounded one transport attachment while the
    /// durable wait stayed live, so the caller re-enters with the same explicit
    /// `process_id`. The caller owns the
    /// overall wait bound through its cancellation select.
    async fn await_process_terminal(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError>;
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
    event_awaiter: NativeProcessAwaiter,
}

impl ProcessWorkWiring {
    /// Pair a watched registry and its change hub with the process port bound
    /// to exactly that handle. This constructs core's one event awaiter; the
    /// caller that created the port owns the pairing contract.
    pub fn new(watched: WatchedRegistry, port: Arc<dyn ProcessWorkSubstrate>) -> Self {
        let event_awaiter =
            NativeProcessAwaiter::new(Arc::clone(watched.registry()), watched.hub().clone());
        Self {
            watched,
            port,
            event_awaiter,
        }
    }

    /// Use the same [`WorkCadencePolicy`] passed to native process, queued-work,
    /// and wake-delivery drivers so an externally supplied process port does
    /// not leave the event awaiter on hidden hardcoded pacing.
    pub fn with_work_cadence(
        mut self,
        work_cadence: WorkCadencePolicy,
    ) -> Result<Self, NativeSubstrateConfigError> {
        work_cadence.validate()?;
        self.event_awaiter = self.event_awaiter.with_work_cadence(work_cadence);
        Ok(self)
    }

    /// Ask the bound substrate to admit this owner's claimable pending processes.
    ///
    /// A host calls this after its process endpoint is ready, or whenever
    /// deployment recovery should be driven. The returned
    /// [`ProcessAdmissionReport`] describes admission, not completion: admitted
    /// processes may still be running when this future resolves.
    pub async fn admit_pending_processes(
        &self,
        reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError> {
        self.port.admit_pending_processes(reason).await
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

    pub fn event_awaiter(&self) -> &NativeProcessAwaiter {
        &self.event_awaiter
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

/// The session work of a host that runs every accepted input itself: an ask
/// schedules nothing, and a caller waiting on a drive runs it in its own task
/// through the installed driver (FIG-3600 S5b, D1 §2.4).
///
/// It stands in for an engine where a host drains queued work by hand and
/// wants no background drive competing with it; a waiter still gets its
/// input driven. One drive of a session runs at a time, and a request id is
/// driven once: a second waiter on it is answered at once and reads the
/// outcome from the store. Nothing retries a drive a caller's task ran, so a
/// failed attempt is that caller's answer.
pub struct InlineSessionWork {
    /// The generation of the build the waiter's drive runs on.
    build_generation: crate::engine::BuildGeneration,
    driver: std::sync::OnceLock<Arc<dyn SessionDriver>>,
    sessions: std::sync::Mutex<
        std::collections::HashMap<SessionId, Arc<tokio::sync::Mutex<InlineDrives>>>,
    >,
}

/// The request ids an inline engine drove for one session, oldest first.
#[derive(Default)]
struct InlineDrives {
    drove: std::collections::VecDeque<String>,
}

/// Request ids an inline engine remembers per session.
const INLINE_DROVE_CAPACITY: usize = 1024;

impl InlineSessionWork {
    /// Inline session work for a build of `build_generation`: the
    /// generation the backend the driver runs on carries.
    pub fn new(build_generation: crate::engine::BuildGeneration) -> Self {
        Self {
            build_generation,
            driver: std::sync::OnceLock::new(),
            sessions: std::sync::Mutex::default(),
        }
    }

    fn session_drives(&self, session: &SessionId) -> Arc<tokio::sync::Mutex<InlineDrives>> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(sessions.entry(session.clone()).or_default())
    }
}

impl std::fmt::Debug for InlineSessionWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InlineSessionWork")
    }
}

#[async_trait::async_trait]
impl SessionWorkEngine for InlineSessionWork {
    fn schedule_drive(&self, session: &SessionId, request: crate::engine::DriveRequestId) {
        tracing::trace!(
            session_id = session.as_str(),
            request = request.as_str(),
            "session drive not scheduled: a waiter drives it inline"
        );
    }

    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        Arc::clone(self.driver.get_or_init(|| driver))
    }

    async fn await_drive(
        &self,
        session: &SessionId,
        request: &crate::engine::DriveRequestId,
    ) -> Result<crate::engine::DriveOutcome, crate::engine::DriveAbort> {
        let Some(driver) = self.driver.get() else {
            return Err(session_work_unavailable(session, request));
        };
        let drives = self.session_drives(session);
        let mut drives = drives.lock().await;
        if drives.drove.iter().any(|drove| drove == request.as_str()) {
            return Ok(crate::engine::DriveOutcome {
                ran: Vec::new(),
                stop: crate::engine::DriveStop::Idle,
            });
        }
        let outcome = driver
            .drive(crate::engine::DriveRequest {
                session: session.clone(),
                request: request.clone(),
                build_generation: self.build_generation.clone(),
            })
            .await
            .map_err(|abort| match abort {
                crate::engine::DriveAbort::Retry(error) => {
                    crate::engine::DriveAbort::Refused(error)
                }
                abort => abort,
            })?;
        if drives.drove.len() >= INLINE_DROVE_CAPACITY {
            drives.drove.pop_front();
        }
        drives.drove.push_back(request.as_str().to_owned());
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe;

    #[async_trait::async_trait]
    impl SessionDriver for Probe {
        async fn drive(
            &self,
            _request: crate::engine::DriveRequest,
        ) -> Result<crate::engine::DriveOutcome, crate::engine::DriveAbort> {
            unreachable!("the probe never drives")
        }

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
