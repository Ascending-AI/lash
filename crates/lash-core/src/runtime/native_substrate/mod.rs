use std::sync::Arc;

use super::process::{ProcessAwaiter, ProcessChangeHub, ProcessRegistry};
use super::{
    DurableProcessWorker, InlineProcessRunHandle, ProcessAdmissionReport, ProcessWorkDriver,
    QueuedWorkDriver, QueuedWorkExecutionConcurrencyError, QueuedWorkRunHandle, WorkerSlotSupplier,
};
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
    registry: Arc<dyn ProcessRegistry>,
    port: Arc<dyn ProcessWorkSubstrate>,
    event_awaiter: ProcessAwaiter,
}

impl ProcessWorkWiring {
    /// Pair a watched registry and its change hub with the process port bound
    /// to exactly that handle. This constructs core's one event awaiter; the
    /// caller that created the port owns the pairing contract.
    pub fn new(
        registry: Arc<dyn ProcessRegistry>,
        hub: ProcessChangeHub,
        port: Arc<dyn ProcessWorkSubstrate>,
    ) -> Self {
        let event_awaiter = ProcessAwaiter::new(Arc::clone(&registry), hub);
        Self {
            registry,
            port,
            event_awaiter,
        }
    }

    pub(crate) fn registry(&self) -> &Arc<dyn ProcessRegistry> {
        &self.registry
    }

    pub(crate) fn port(&self) -> &Arc<dyn ProcessWorkSubstrate> {
        &self.port
    }

    pub(crate) fn event_awaiter(&self) -> &ProcessAwaiter {
        &self.event_awaiter
    }
}

/// First-party queued-work port backed by the existing native dispatcher.
#[derive(Clone)]
pub struct NativeQueuedWork {
    driver: QueuedWorkDriver,
}

impl NativeQueuedWork {
    /// Construct the native queued-work port.
    pub fn new(run_handle: Arc<dyn QueuedWorkRunHandle>) -> Self {
        Self {
            driver: QueuedWorkDriver::new(run_handle),
        }
    }

    /// Construct the native queued-work port with a host-selected admission bound.
    pub fn with_execution_concurrency(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        concurrency: usize,
    ) -> Result<Self, QueuedWorkExecutionConcurrencyError> {
        Ok(Self {
            driver: QueuedWorkDriver::with_execution_concurrency(run_handle, concurrency)?,
        })
    }

    /// Construct the native queued-work port admitted by `supplier`.
    #[doc(hidden)]
    pub fn with_worker_slot_supplier(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        supplier: Arc<dyn WorkerSlotSupplier>,
    ) -> Self {
        Self {
            driver: QueuedWorkDriver::with_worker_slot_supplier(run_handle, supplier),
        }
    }

    /// Validate a host-selected queued-work execution concurrency.
    pub fn validate_execution_concurrency(
        concurrency: usize,
    ) -> Result<(), QueuedWorkExecutionConcurrencyError> {
        QueuedWorkDriver::validate_execution_concurrency(concurrency)
    }

    #[allow(dead_code)]
    pub(crate) fn from_driver(driver: QueuedWorkDriver) -> Self {
        Self { driver }
    }
}

#[async_trait::async_trait]
impl QueuedWorkSubstrate for NativeQueuedWork {
    fn notify_session_work(&self, target: SessionWorkTarget, reason: &str) {
        self.driver
            .notify_pending_work(target.as_session_id(), reason);
    }

    async fn drain_session_work(
        &self,
        target: SessionWorkTarget,
        reason: &str,
    ) -> Result<SessionDrainOutcome, PluginError> {
        self.driver
            .claim_and_run_pending(target.as_session_id(), reason)
            .await?;
        Ok(SessionDrainOutcome::Ran)
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

/// First-party process-work port backed by the existing native worker and awaiter.
#[derive(Clone)]
pub struct NativeProcessWork {
    driver: ProcessWorkDriver,
}

impl NativeProcessWork {
    /// Construct native process work over an already-watched registry.
    pub fn new(
        registry: Arc<dyn ProcessRegistry>,
        hub: ProcessChangeHub,
        worker: DurableProcessWorker,
    ) -> Self {
        Self {
            driver: ProcessWorkDriver::from_watched(
                registry,
                hub,
                Arc::new(InlineProcessRunHandle::new(worker)),
            ),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    pub(crate) fn from_driver(driver: ProcessWorkDriver) -> Self {
        Self { driver }
    }

    /// Construct a registry-only process port for tests that do not admit work.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_registry(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            driver: ProcessWorkDriver::from_watched(
                registry,
                ProcessChangeHub::new(),
                Arc::new(RegistryOnlyRunHandle),
            ),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
struct RegistryOnlyRunHandle;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl super::ProcessRunHandle for RegistryOnlyRunHandle {
    async fn claim_and_run_pending(&self) -> Result<ProcessAdmissionReport, PluginError> {
        Ok(ProcessAdmissionReport::default())
    }
}

#[async_trait::async_trait]
impl ProcessWorkSubstrate for NativeProcessWork {
    async fn admit_pending_processes(
        &self,
        reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError> {
        self.driver.claim_and_run_pending(reason).await
    }

    async fn await_process_terminal(
        &self,
        process_id: &str,
    ) -> Result<ProcessTerminalWait, PluginError> {
        self.driver
            .await_terminal(process_id)
            .await
            .map(ProcessTerminalWait::Terminal)
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
