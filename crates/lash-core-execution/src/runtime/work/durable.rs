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
    changes: Option<crate::runtime::process::ProcessChangeHub>,
    work_cadence: super::WorkCadencePolicy,
}

impl DurableProcessWork {
    /// Process work over `backend`.
    #[must_use]
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            changes: None,
            work_cadence: super::WorkCadencePolicy::standard(),
        }
    }

    /// Wake terminal waits from `changes`, the hub that ticks when a commit
    /// grew a process's log. Without it a wait only polls on the work
    /// cadence.
    #[must_use]
    pub fn with_process_changes(
        mut self,
        changes: crate::runtime::process::ProcessChangeHub,
    ) -> Self {
        self.changes = Some(changes);
        self
    }
}

impl DurableProcessWork {
    /// Configure the registry waits this port actually runs.
    pub fn with_work_cadence(
        mut self,
        cadence: super::WorkCadencePolicy,
    ) -> Result<Self, super::WorkCadenceError> {
        cadence.validate()?;
        self.work_cadence = cadence;
        Ok(self)
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
        let registry = self.backend.process_registry();
        let awaiter = match &self.changes {
            Some(changes) => super::ProcessRegistryAwaiter::new(registry, changes.clone()),
            None => super::ProcessRegistryAwaiter::for_registry(registry),
        };
        awaiter
            .with_work_cadence(self.work_cadence.clone())
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
