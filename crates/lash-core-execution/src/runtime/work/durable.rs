//! The process work port of the durable backend (ADR 0132 §1): a
//! submission is a wake of the actor that owns the work, never a send to an
//! engine.
//!
//! A process start wakes its process actor ([`Backend::wake_process`], L6).
//! Waits and cancels become wait rows and mail, filled by their owners.

use super::{ProcessTerminalWait, ProcessWorkSubstrate};
use crate::{Backend, PluginError};

/// Process work over the durable backend: a start wakes the process actor.
pub struct DurableProcessWork {
    backend: Backend,
}

impl DurableProcessWork {
    /// Process work over `backend`.
    #[must_use]
    pub fn new(backend: Backend) -> Self {
        Self { backend }
    }
}

impl std::fmt::Debug for DurableProcessWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DurableProcessWork")
    }
}

#[async_trait::async_trait]
impl ProcessWorkSubstrate for DurableProcessWork {
    /// A caller outside an actor has no wait row to own: it reads the
    /// process's terminal from the registry the terminal transaction
    /// writes. An actor awaits through `waits::await_process`, bounded and
    /// cancellable.
    async fn await_process_terminal(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError> {
        super::ProcessRegistryAwaiter::for_registry(self.backend.process_registry())
            .await_terminal(process_id)
            .await
            .map(ProcessTerminalWait::Terminal)
    }

    async fn deliver_cancel(
        &self,
        process_id: &crate::ProcessId,
        request: &crate::CancelRequest,
        _key: &str,
    ) -> Result<(), PluginError> {
        // The registry recorded the first request and its mail; this is the
        // same request again, which answers `AlreadyRequested` (or
        // `AlreadyEnded`) and wakes the actor.
        let mut tx = lash_durable::MailTx::new();
        tx.write(lash_durable::domain::MailDomainWrite::RequestProcessCancel(
            lash_durable::domain::CancelRequest {
                process: process_id.clone(),
                origin: request.origin,
                requester: request.requester.clone(),
            },
        ));
        self.backend
            .durable()
            .commit_mail(tx, lash_durable::CommitLabel::PROCESS_CANCEL)
            .await
            .map(drop)
            .map_err(|error| PluginError::Invoke(error.to_string()))
    }
}
