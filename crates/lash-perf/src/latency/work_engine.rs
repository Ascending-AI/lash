//! `await_shift` fixtures for the follower's tail behaviours.
//!
//! A `SendHandle` follower waits on three wakes: live replay, the engine's
//! shift attach (`SessionWorkEngine::await_shift`) and the store poll that
//! backs off 25 ms → 1 s. The `poll` and `grace` cases isolate the last two
//! against a live server by substituting only the host's `await_shift`:
//!
//! * [`AwaitShiftMode::Answered`] answers the attach immediately, so the
//!   follower treats the shift as already stopped and settlement detection
//!   rides the poll cadence alone — the measured `settle→complete` tail is
//!   the realized 25 ms..1 s backoff.
//! * [`AwaitShiftMode::Pending`] never answers the attach, exactly as a
//!   shift that outlives the measured run does. The follower's 5 s
//!   live-report grace binds only while a run in the host may still deposit
//!   a report, so a run the worker ran answers with the durable thin report
//!   once the store shows it settled.
//!
//! Both still deliver the shift: `schedule_shift`/`request_shift` forward
//! untouched, only the host-side wait is stubbed, and the real shift runs to
//! completion on the worker either way.

use std::sync::Arc;

use lash_core::engine::{ShiftAbort, ShiftOutcome, ShiftRequestId, ShiftStop};
use lash_sansio::SessionId;

/// How a host's `await_shift` answers in a latency case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AwaitShiftMode {
    /// The engine's own attach — production behaviour.
    Real,
    /// Answer immediately as if the shift already stopped; polls alone
    /// carry settlement detection.
    Answered,
    /// Never answer, as a shift that outlives the run does.
    Pending,
}

/// A `SessionWorkEngine` that forwards everything but `await_shift`, which
/// answers per `mode`. Installed on the host's backend through
/// `LayeredBackend::with_session_work`.
pub(crate) struct LatencySessionWork {
    inner: Arc<dyn lash_core::SessionWorkEngine>,
    mode: AwaitShiftMode,
}

impl LatencySessionWork {
    /// Wrap `inner`'s port with `mode`'s `await_shift` answer.
    pub(crate) fn wrap(
        backend: &lash::Backend,
        mode: AwaitShiftMode,
    ) -> Arc<dyn lash_core::SessionWorkEngine> {
        let inner = backend.session_work();
        Arc::new(Self { inner, mode })
    }
}

#[async_trait::async_trait]
impl lash_core::SessionWorkEngine for LatencySessionWork {
    fn schedule_shift(&self, session: &SessionId, request: ShiftRequestId) {
        self.inner.schedule_shift(session, request);
    }

    async fn request_shift(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Result<(), lash_core::engine::EngineRefusal> {
        self.inner.request_shift(session, request).await
    }

    fn install_session_shifts(
        &self,
        shifts: Arc<dyn lash_core::SessionShifts>,
    ) -> Arc<dyn lash_core::SessionShifts> {
        self.inner.install_session_shifts(shifts)
    }

    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        self.inner.control()
    }

    async fn await_shift(
        &self,
        session: &SessionId,
        request: &ShiftRequestId,
    ) -> Result<ShiftOutcome, ShiftAbort> {
        match self.mode {
            AwaitShiftMode::Real => self.inner.await_shift(session, request).await,
            AwaitShiftMode::Answered => Ok(ShiftOutcome {
                ran: Vec::new(),
                stop: ShiftStop::Idle,
            }),
            AwaitShiftMode::Pending => std::future::pending().await,
        }
    }
}
