use std::sync::Arc;

mod awaiter;
pub(crate) mod lane_wait;
mod policy;
mod process_work;
mod queued;
mod wake_delivery;

pub(crate) use awaiter::NativeProcessAwaiter;
pub use policy::{
    NativeSubstrateConfig, NativeSubstrateConfigError, WorkCadencePolicy, WorkerSweepPolicy,
};
pub use process_work::NativeProcessWork;
pub use queued::*;
pub use wake_delivery::{WakeDeliveryDriveReport, WakeDeliveryDriver};

use super::ProcessAdmissionReport;
use super::process::{ProcessRegistry, WatchedRegistry};
use crate::{PluginError, ProcessAwaitOutput};

/// Deployment port for durable **queued session work**: work already committed
/// to a session's durable queue that something must be told about and drain.
#[async_trait::async_trait]
pub trait QueuedWorkSubstrate: Send + Sync {
    /// Signal, fire-and-forget, that durable queued work may be claimable.
    /// Contentless and coalesced by the implementation.
    fn notify_session_work(&self, target: SessionWorkTarget, reason: &str);

    /// Run one pass that claims and submits ready queued work. Idempotency
    /// belongs to the store scheduler, not to a same-process memory guard.
    async fn drain_session_work(
        &self,
        target: SessionWorkTarget,
        reason: &str,
    ) -> Result<SessionDrainOutcome, PluginError>;
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

    /// Wait for `process_id` to reach a terminal state.
    ///
    /// There is no polling fallback and no "attach if provided". [`ProcessTerminalWait::Reattach`]
    /// is recoverable: the port bounded one transport attachment while the
    /// durable wait stayed live, so the caller re-enters with the same explicit
    /// `process_id` (never an implicit "latest" process). The caller owns the
    /// overall wait bound through its cancellation select.
    async fn await_process_terminal(
        &self,
        process_id: &str,
    ) -> Result<ProcessTerminalWait, PluginError>;
}

/// Which sessions a queued-work operation addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionWorkTarget {
    /// All sessions with claimable queued work.
    Any,
    /// One identified session.
    Session(String),
}

impl SessionWorkTarget {
    fn as_session_id(&self) -> Option<&str> {
        match self {
            Self::Any => None,
            Self::Session(session_id) => Some(session_id),
        }
    }
}

/// Whether a drain pass actually claimed and ran durable work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionDrainOutcome {
    /// The pass ran: an authoritative head may have moved.
    Ran,
    /// This deployment has no queued lane; the durable row stays pending and
    /// nothing was read or written.
    Deferred,
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

    /// Apply the validated native polling cadence to process-event waits.
    ///
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

    pub(crate) fn registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.watched.registry()
    }

    pub(crate) fn watched(&self) -> &WatchedRegistry {
        &self.watched
    }

    pub(crate) fn port(&self) -> &Arc<dyn ProcessWorkSubstrate> {
        &self.port
    }

    pub(crate) fn event_awaiter(&self) -> &NativeProcessAwaiter {
        &self.event_awaiter
    }
}

/// Explicit queued-work port for deployments with no queued lane.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoQueuedWork;

impl NoQueuedWork {
    /// Construct a disabled queued-work port.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl QueuedWorkSubstrate for NoQueuedWork {
    fn notify_session_work(&self, target: SessionWorkTarget, reason: &str) {
        tracing::trace!(
            ?target,
            reason,
            "queued work deferred: deployment has no queued lane"
        );
    }

    async fn drain_session_work(
        &self,
        target: SessionWorkTarget,
        reason: &str,
    ) -> Result<SessionDrainOutcome, PluginError> {
        tracing::trace!(
            ?target,
            reason,
            "queued work deferred: deployment has no queued lane"
        );
        Ok(SessionDrainOutcome::Deferred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_queued_work_defers_without_running_a_queue() {
        let port = NoQueuedWork::new();

        port.notify_session_work(
            SessionWorkTarget::Session("deferred-session".to_string()),
            "test-notify",
        );
        let outcome = port
            .drain_session_work(
                SessionWorkTarget::Session("deferred-session".to_string()),
                "session_command",
            )
            .await
            .expect("disabled queued lane cannot add a failure path");

        assert_eq!(outcome, SessionDrainOutcome::Deferred);
    }
}
