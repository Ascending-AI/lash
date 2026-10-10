//! Live faults abort an opaque attempt without becoming a recorded outcome.

use crate::RuntimeEffectControllerError;
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct EffectAttempt {
    trace_id: lash_trace::TraceAttemptId,
    fault: Arc<Mutex<Option<RuntimeEffectControllerError>>>,
    changed: Arc<tokio::sync::Notify>,
}

impl Default for EffectAttempt {
    fn default() -> Self {
        Self {
            trace_id: lash_trace::TraceAttemptId::new(format!(
                "attempt:{}",
                uuid::Uuid::new_v4().simple()
            )),
            fault: Arc::default(),
            changed: Arc::default(),
        }
    }
}

impl EffectAttempt {
    pub(crate) fn trace_id(&self) -> lash_trace::TraceAttemptId {
        self.trace_id.clone()
    }
    pub fn fault_attempt(&self, fault: RuntimeEffectControllerError) {
        let mut stored = self.fault.lock_recover();
        if stored.is_none() {
            *stored = Some(fault);
            self.changed.notify_waiters();
        }
    }

    /// End the attempt at `fault`, its own
    /// ([`RuntimeEffectControllerError::is_attempt_fault`]): latch it and
    /// never answer, so the work that met it is dropped there and nothing
    /// it would do with the failure runs. The attempt's owner answers the
    /// fault, never the attempt's body (FIG-4632, FIG-5368).
    pub async fn end_at(&self, fault: RuntimeEffectControllerError) -> std::convert::Infallible {
        self.fault_attempt(fault);
        std::future::pending().await
    }

    pub fn attempt_fault(&self) -> Option<RuntimeEffectControllerError> {
        self.fault.lock_recover().clone()
    }

    pub async fn attempt_faulted(&self) -> RuntimeEffectControllerError {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(fault) = self.attempt_fault() {
                return fault;
            }
            notified.await;
        }
    }
}

pub struct RecordedEffectExecution {
    pub outcome: Result<crate::RuntimeEffectOutcome, RuntimeEffectControllerError>,
    pub attempt_fault: Option<RuntimeEffectControllerError>,
}
