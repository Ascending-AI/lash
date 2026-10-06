//! The work ports of the durable backend (ADR 0132 §1): a submission is a
//! wake of the actor that owns the work, never a send to an engine.
//!
//! A session's shift ask wakes its session actor ([`Backend::wake_session`],
//! L3s); a process start wakes its process actor ([`Backend::wake_process`],
//! L6). Waits and cancels become wait rows and mail, filled by their owners.

use std::sync::{Arc, OnceLock};

use super::{ProcessTerminalWait, ProcessWorkSubstrate, SessionShifts, SessionWorkEngine};
use crate::{Backend, PluginError, SessionId};

/// Session work over the durable backend: every ask wakes the session actor.
pub struct DurableSessionWork {
    backend: Backend,
    shifts: OnceLock<Arc<dyn SessionShifts>>,
}

impl DurableSessionWork {
    /// Session work over `backend`.
    #[must_use]
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            shifts: OnceLock::new(),
        }
    }
}

impl std::fmt::Debug for DurableSessionWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DurableSessionWork")
    }
}

#[async_trait::async_trait]
impl SessionWorkEngine for DurableSessionWork {
    fn schedule_shift(&self, _session: &SessionId, _request: crate::engine::ShiftRequestId) {
        todo!("L3s (FIG-5196): wake the session actor; its activation drains the session's mail")
    }

    async fn request_shift(
        &self,
        session: &SessionId,
        _request: crate::engine::ShiftRequestId,
    ) -> Result<(), crate::engine::EngineRefusal> {
        self.backend.wake_session(session).await.map_err(|error| {
            crate::engine::EngineRefusal::retryable(
                crate::RuntimeErrorCode::SessionWorkUnavailable,
                error.to_string(),
            )
        })
    }

    fn install_session_shifts(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts> {
        Arc::clone(self.shifts.get_or_init(|| shifts))
    }

    async fn await_shift(
        &self,
        _session: &SessionId,
        _request: &crate::engine::ShiftRequestId,
    ) -> Result<crate::engine::ShiftOutcome, crate::engine::ShiftAbort> {
        todo!("L3s (FIG-5196): answer once the session actor's activation for the ask released")
    }
}

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
    async fn deliver_process_start(
        &self,
        record: &crate::ProcessRecord,
    ) -> Result<(), PluginError> {
        self.backend
            .wake_process(&record.id)
            .await
            .map_err(|error| PluginError::Invoke(error.to_string()))
    }

    async fn await_process_terminal(
        &self,
        _process_id: &crate::ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError> {
        todo!("L5 (FIG-5173): await the process's terminal through a process-terminal wait row")
    }

    async fn deliver_cancel(
        &self,
        _process_id: &crate::ProcessId,
        _request: &crate::CancelRequest,
        _key: &str,
    ) -> Result<(), PluginError> {
        todo!("L6 (FIG-5175): request the process's cancel as mail; the first request wins")
    }

    async fn publish_process_terminal(
        &self,
        _process_id: &crate::ProcessId,
        _output: &crate::ProcessAwaitOutput,
        _key: &str,
    ) -> Result<(), PluginError> {
        todo!("L5 (FIG-5173): resolve the process's terminal waits in its terminal transaction")
    }
}
