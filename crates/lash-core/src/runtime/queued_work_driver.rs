use std::sync::Arc;
use std::time::Duration;

use crate::PluginError;

const WAKE_RETRY_INITIAL: Duration = Duration::from_millis(25);
const WAKE_RETRY_MAX: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct QueuedWorkRunRequest {
    pub session_id: Option<String>,
    pub reason: String,
    pub trace_idle: bool,
}

impl QueuedWorkRunRequest {
    fn new(session_id: Option<String>, reason: impl Into<String>, trace_idle: bool) -> Self {
        Self {
            session_id,
            reason: reason.into(),
            trace_idle,
        }
    }
}

/// Operational evidence that a best-effort queued-work wake needs retry.
///
/// A wake failure is never an enqueue failure: the input is already durable,
/// and the driver's retry loop re-enters the idempotent pending-work claim
/// path until it accepts the wake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkWakeFailure {
    pub session_id: Option<String>,
    pub reason: String,
    pub attempt: u32,
    pub retry_after_ms: u64,
    pub error: String,
}

#[async_trait::async_trait]
pub trait QueuedWorkRunHandle: Send + Sync {
    async fn run_queued_work(&self, request: QueuedWorkRunRequest) -> Result<(), PluginError>;

    /// Host-driven single pass: claim and submit ready queued work, optionally
    /// narrowed to one session. The symmetric counterpart to
    /// [`ProcessRunHandle::claim_and_run_pending`](super::ProcessRunHandle::claim_and_run_pending).
    ///
    /// Idempotency is the store scheduler's job, not a same-process memory
    /// guard. Hosts call this on an event (enqueue, process wake, turn
    /// completion) instead of polling.
    async fn claim_and_run_pending(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<(), PluginError> {
        let request =
            QueuedWorkRunRequest::new(session_id.map(str::to_string), reason.to_string(), false);
        self.run_queued_work(request).await
    }
}

#[derive(Clone)]
pub struct QueuedWorkDriver {
    run_handle: Arc<dyn QueuedWorkRunHandle>,
}

impl QueuedWorkDriver {
    pub fn new(run_handle: Arc<dyn QueuedWorkRunHandle>) -> Self {
        Self { run_handle }
    }

    pub async fn claim_and_run_pending(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<(), PluginError> {
        if let Err(err) = self
            .run_handle
            .claim_and_run_pending(session_id, reason)
            .await
        {
            tracing::warn!("queued work drive ({reason}) failed: {err}");
            return Err(err);
        }
        Ok(())
    }

    /// Wake pending work without coupling dispatch success to the durable write
    /// that requested it.
    ///
    /// The first claim happens on a spawned task so callers can return their
    /// durable acceptance receipt immediately. Operational failures are
    /// recorded as typed telemetry and retried with bounded backoff; the
    /// underlying store scheduler remains the idempotency authority.
    pub fn wake_pending(&self, session_id: Option<&str>, reason: &str) {
        let driver = self.clone();
        let session_id = session_id.map(str::to_string);
        let reason = reason.to_string();
        crate::task::spawn(async move {
            let mut attempt = 1_u32;
            let mut retry_after = WAKE_RETRY_INITIAL;
            loop {
                match driver
                    .run_handle
                    .claim_and_run_pending(session_id.as_deref(), &reason)
                    .await
                {
                    Ok(()) => return,
                    Err(err) => {
                        let failure = QueuedWorkWakeFailure {
                            session_id: session_id.clone(),
                            reason: reason.clone(),
                            attempt,
                            retry_after_ms: retry_after.as_millis() as u64,
                            error: err.to_string(),
                        };
                        tracing::warn!(
                            session_id = failure.session_id.as_deref(),
                            reason = %failure.reason,
                            attempt = failure.attempt,
                            retry_after_ms = failure.retry_after_ms,
                            error = %failure.error,
                            event = "queued_work.wake_retry",
                            "queued-work wake failed; retrying the pending-work claim"
                        );
                    }
                }
                tokio::time::sleep(retry_after).await;
                retry_after = retry_after.saturating_mul(2).min(WAKE_RETRY_MAX);
                attempt = attempt.saturating_add(1);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FailOnceRunHandle {
        attempts: Arc<AtomicUsize>,
        accepted: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for FailOnceRunHandle {
        async fn run_queued_work(&self, _request: QueuedWorkRunRequest) -> Result<(), PluginError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(PluginError::Session("transient wake failure".to_string()));
            }
            self.accepted.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn best_effort_wake_reenters_pending_claim_without_an_external_event() {
        let handle = Arc::new(FailOnceRunHandle {
            attempts: Arc::new(AtomicUsize::new(0)),
            accepted: tokio::sync::Notify::new(),
        });
        let accepted = handle.accepted.notified();
        let driver = QueuedWorkDriver::new(handle.clone());

        driver.wake_pending(Some("session-1"), "queued_turn_input");

        tokio::time::timeout(Duration::from_secs(1), accepted)
            .await
            .expect("the failed wake must retry without another enqueue");
        assert_eq!(handle.attempts.load(Ordering::SeqCst), 2);
    }
}
