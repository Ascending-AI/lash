use std::sync::Arc;

use crate::{PluginError, ProcessAdmissionReport, WatchedRegistry};

#[cfg(any(test, feature = "testing"))]
use crate::{ProcessAwaitOutput, ProcessEvent, ProcessId, ProcessRegistry};

use super::{NativeProcessAwaiter, ProcessTerminalWait, ProcessWorkSubstrate};

#[async_trait::async_trait]
pub trait NativeProcessAdmissionDriver: Send + Sync {
    fn native_work_cadence(&self) -> super::WorkCadencePolicy;
    async fn drive_pending_processes(&self) -> Result<ProcessAdmissionReport, PluginError>;
}

/// First-party process-work port backed by the native worker and awaiter.
#[derive(Clone)]
pub struct NativeProcessWork {
    worker: NativeProcessWorker,
    terminal_awaiter: NativeProcessAwaiter,
}

#[derive(Clone)]
enum NativeProcessWorker {
    Durable(Arc<dyn NativeProcessAdmissionDriver>),
    #[cfg(any(test, feature = "testing"))]
    RegistryOnly,
}

impl NativeProcessWork {
    /// Construct native process work over an already-watched registry.
    pub fn new<W: NativeProcessAdmissionDriver + 'static>(
        watched: &WatchedRegistry,
        worker: W,
    ) -> Self {
        let work_cadence = worker.native_work_cadence();
        Self {
            worker: NativeProcessWorker::Durable(Arc::new(worker)),
            terminal_awaiter: NativeProcessAwaiter::new_with_work_cadence(
                Arc::clone(watched.registry()),
                watched.hub().clone(),
                work_cadence,
            ),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_registry(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            worker: NativeProcessWorker::RegistryOnly,
            terminal_awaiter: NativeProcessAwaiter::for_registry(registry),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn await_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        self.terminal_awaiter.await_terminal(process_id).await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn await_event(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<ProcessEvent, PluginError> {
        self.terminal_awaiter
            .await_event(process_id, event_type, after_sequence)
            .await
    }
}

#[async_trait::async_trait]
impl ProcessWorkSubstrate for NativeProcessWork {
    async fn admit_pending_processes(
        &self,
        reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError> {
        match &self.worker {
            NativeProcessWorker::Durable(worker) => match worker.drive_pending_processes().await {
                Ok(report) => Ok(report),
                Err(error) => {
                    tracing::warn!("process work drive ({reason}) failed: {error}");
                    Err(error)
                }
            },
            #[cfg(any(test, feature = "testing"))]
            NativeProcessWorker::RegistryOnly => Ok(ProcessAdmissionReport::default()),
        }
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
        // A native execution watches the registry for its cancel request;
        // `apply_parent_end_plan` records that request right after this call,
        // which is the delivery itself.
        Ok(())
    }

    async fn publish_process_terminal(
        &self,
        _process_id: &crate::ProcessId,
        _output: &crate::ProcessAwaitOutput,
        _key: &str,
    ) -> Result<(), PluginError> {
        // A native waiter reads the registry: the terminal commit it follows
        // is the publication (ADR 0109 §3), so there is nothing to send.
        Ok(())
    }
}
