//! `await_drive` fixtures for the follower's tail behaviours.
//!
//! A `SendHandle` follower waits on three wakes: live replay, the engine's
//! drive attach (`SessionWorkEngine::await_drive`) and the store poll that
//! backs off 25 ms → 1 s. The `poll` and `grace` cases isolate the last two
//! against a live server by substituting only the host's `await_drive`:
//!
//! * [`AwaitDriveMode::Answered`] answers the attach immediately, so the
//!   follower treats the drive as already stopped and settlement detection
//!   rides the poll cadence alone — the measured `settle→complete` tail is
//!   the realized 25 ms..1 s backoff.
//! * [`AwaitDriveMode::Pending`] never answers the attach, exactly as a
//!   drive that outlives the measured root does, so a settled root with no
//!   live report waits the follower's 5 s live-report grace before the
//!   durable thin report answers.
//!
//! Both still deliver the drive: `schedule_drive`/`request_drive` forward
//! untouched, only the host-side wait is stubbed, and the real drive runs to
//! completion on the worker either way.

use std::sync::Arc;

use lash_core::engine::{DriveAbort, DriveOutcome, DriveRequestId, DriveStop};
use lash_sansio::SessionId;

/// How a host's `await_drive` answers in a latency case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AwaitDriveMode {
    /// The engine's own attach — production behaviour.
    Real,
    /// Answer immediately as if the drive already stopped; polls alone
    /// carry settlement detection.
    Answered,
    /// Never answer; the follower waits out the live-report grace before
    /// reading the durable report.
    Pending,
}

/// A `SessionWorkEngine` that forwards everything but `await_drive`, which
/// answers per `mode`. Installed on the host's backend through
/// `LayeredBackend::with_session_work`.
pub(crate) struct LatencySessionWork {
    inner: Arc<dyn lash_core::SessionWorkEngine>,
    mode: AwaitDriveMode,
}

impl LatencySessionWork {
    /// Wrap `inner`'s port with `mode`'s `await_drive` answer.
    pub(crate) fn wrap(
        backend: &lash::Backend,
        mode: AwaitDriveMode,
    ) -> Arc<dyn lash_core::SessionWorkEngine> {
        let inner = backend
            .session_work()
            .unwrap_or_else(|| Arc::new(lash_core::NoSessionWork::new()));
        Arc::new(Self { inner, mode })
    }
}

#[async_trait::async_trait]
impl lash_core::SessionWorkEngine for LatencySessionWork {
    fn schedule_drive(&self, session: &SessionId, request: DriveRequestId) {
        self.inner.schedule_drive(session, request);
    }

    async fn request_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<(), lash_core::engine::EngineRefusal> {
        self.inner.request_drive(session, request).await
    }

    fn install_session_driver(
        &self,
        driver: Arc<dyn lash_core::SessionDriver>,
    ) -> Arc<dyn lash_core::SessionDriver> {
        self.inner.install_session_driver(driver)
    }

    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        self.inner.control()
    }

    async fn await_drive(
        &self,
        session: &SessionId,
        request: &DriveRequestId,
    ) -> Result<DriveOutcome, DriveAbort> {
        match self.mode {
            AwaitDriveMode::Real => self.inner.await_drive(session, request).await,
            AwaitDriveMode::Answered => Ok(DriveOutcome {
                ran: Vec::new(),
                stop: DriveStop::Idle,
            }),
            AwaitDriveMode::Pending => std::future::pending().await,
        }
    }

    async fn session_work_in_flight(&self, session: &SessionId) -> bool {
        self.inner.session_work_in_flight(session).await
    }
}
