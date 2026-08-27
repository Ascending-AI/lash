use std::sync::Arc;

use crate::{PluginError, ProcessAdmissionReport, WatchedRegistry};

#[cfg(any(test, feature = "testing"))]
use crate::{ProcessAwaitOutput, ProcessEvent, ProcessRegistry};

use super::{NativeProcessAwaiter, ProcessTerminalWait, ProcessWorkSubstrate};
use crate::runtime::DurableProcessWorker;

/// First-party process-work port backed by the native worker and awaiter.
#[derive(Clone)]
pub struct NativeProcessWork {
    worker: NativeProcessWorker,
    terminal_awaiter: NativeProcessAwaiter,
}

#[derive(Clone)]
enum NativeProcessWorker {
    Durable(DurableProcessWorker),
    #[cfg(any(test, feature = "testing"))]
    RegistryOnly,
}

impl NativeProcessWork {
    /// Construct native process work over an already-watched registry.
    pub fn new(watched: &WatchedRegistry, worker: DurableProcessWorker) -> Self {
        Self {
            worker: NativeProcessWorker::Durable(worker),
            terminal_awaiter: NativeProcessAwaiter::new(
                Arc::clone(watched.registry()),
                watched.hub().clone(),
            ),
        }
    }

    /// Construct registry-only native process work for test support.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_registry(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            worker: NativeProcessWorker::RegistryOnly,
            terminal_awaiter: NativeProcessAwaiter::for_registry(registry),
        }
    }

    /// Test-support terminal wait over the registry-only native port.
    #[cfg(any(test, feature = "testing"))]
    pub async fn await_terminal(
        &self,
        process_id: &str,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        self.terminal_awaiter.await_terminal(process_id).await
    }

    /// Test-support event wait over the registry-only native port.
    #[cfg(any(test, feature = "testing"))]
    pub async fn await_event(
        &self,
        process_id: &str,
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
        process_id: &str,
    ) -> Result<ProcessTerminalWait, PluginError> {
        self.terminal_awaiter
            .await_terminal(process_id)
            .await
            .map(ProcessTerminalWait::Terminal)
    }
}
