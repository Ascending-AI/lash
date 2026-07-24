use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::PluginError;

const WAKE_RETRY_INITIAL: Duration = Duration::from_millis(25);
const WAKE_RETRY_MAX: Duration = Duration::from_secs(1);
const WAKE_MAX_ATTEMPTS: u32 = 8;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkRunErrorClass {
    Transient,
    Terminal,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{error}")]
pub struct QueuedWorkRunError {
    pub class: QueuedWorkRunErrorClass,
    pub error: PluginError,
}

impl QueuedWorkRunError {
    pub fn transient(error: PluginError) -> Self {
        Self {
            class: QueuedWorkRunErrorClass::Transient,
            error,
        }
    }

    pub fn terminal(error: PluginError) -> Self {
        Self {
            class: QueuedWorkRunErrorClass::Terminal,
            error,
        }
    }
}

impl From<PluginError> for QueuedWorkRunError {
    fn from(error: PluginError) -> Self {
        Self::terminal(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkWakeDisposition {
    Retrying,
    Terminal,
    Exhausted,
}

/// Operational evidence that a best-effort queued-work wake needs retry.
///
/// A wake failure is never an enqueue failure: the input is already durable,
/// and transient failures re-enter the idempotent pending-work claim path up
/// to the driver's bounded retry limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkWakeFailure {
    pub session_id: Option<String>,
    pub reason: String,
    pub attempt: u32,
    pub retry_after_ms: u64,
    pub disposition: QueuedWorkWakeDisposition,
    pub error: String,
}

#[async_trait::async_trait]
pub trait QueuedWorkRunHandle: Send + Sync {
    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError>;

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
    ) -> Result<(), QueuedWorkRunError> {
        let request =
            QueuedWorkRunRequest::new(session_id.map(str::to_string), reason.to_string(), false);
        self.run_queued_work(request).await
    }
}

#[derive(Clone)]
pub struct QueuedWorkDriver {
    inner: Arc<QueuedWorkDriverInner>,
}

struct QueuedWorkDriverInner {
    run_handle: Arc<dyn QueuedWorkRunHandle>,
    shutdown: CancellationToken,
    wake_tasks: TaskTracker,
}

impl Drop for QueuedWorkDriverInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.wake_tasks.close();
    }
}

impl QueuedWorkDriver {
    pub fn new(run_handle: Arc<dyn QueuedWorkRunHandle>) -> Self {
        Self::with_shutdown_token(run_handle, CancellationToken::new())
    }

    pub fn with_shutdown_token(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(QueuedWorkDriverInner {
                run_handle,
                shutdown: shutdown.child_token(),
                wake_tasks: TaskTracker::new(),
            }),
        }
    }

    pub async fn claim_and_run_pending(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<(), PluginError> {
        if let Err(err) = self
            .inner
            .run_handle
            .claim_and_run_pending(session_id, reason)
            .await
        {
            tracing::warn!("queued work drive ({reason}) failed: {err}");
            return Err(err.error);
        }
        Ok(())
    }

    /// Wake pending work without coupling dispatch success to the durable write
    /// that requested it.
    ///
    /// The first claim happens on a spawned task so callers can return their
    /// durable acceptance receipt immediately. Operational failures are
    /// recorded as typed telemetry. Only typed transient failures retry, and
    /// both attempts and task lifetime are bounded; the underlying store
    /// scheduler remains the idempotency authority.
    pub fn wake_pending(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> tokio::task::JoinHandle<()> {
        let run_handle = Arc::clone(&self.inner.run_handle);
        let shutdown = self.inner.shutdown.clone();
        let session_id = session_id.map(str::to_string);
        let reason = reason.to_string();
        self.inner.wake_tasks.spawn(async move {
            let mut attempt = 1_u32;
            let mut retry_after = WAKE_RETRY_INITIAL;
            loop {
                let result = tokio::select! {
                    () = shutdown.cancelled() => return,
                    result = run_handle.claim_and_run_pending(session_id.as_deref(), &reason) => result,
                };
                match result {
                    Ok(()) => return,
                    Err(err) => {
                        let disposition = match err.class {
                            QueuedWorkRunErrorClass::Terminal => {
                                QueuedWorkWakeDisposition::Terminal
                            }
                            QueuedWorkRunErrorClass::Transient
                                if attempt >= WAKE_MAX_ATTEMPTS =>
                            {
                                QueuedWorkWakeDisposition::Exhausted
                            }
                            QueuedWorkRunErrorClass::Transient => {
                                QueuedWorkWakeDisposition::Retrying
                            }
                        };
                        let failure = QueuedWorkWakeFailure {
                            session_id: session_id.clone(),
                            reason: reason.clone(),
                            attempt,
                            retry_after_ms: if matches!(
                                disposition,
                                QueuedWorkWakeDisposition::Retrying
                            ) {
                                retry_after.as_millis() as u64
                            } else {
                                0
                            },
                            disposition,
                            error: err.to_string(),
                        };
                        match failure.disposition {
                            QueuedWorkWakeDisposition::Retrying => tracing::warn!(
                                session_id = failure.session_id.as_deref(),
                                reason = %failure.reason,
                                attempt = failure.attempt,
                                retry_after_ms = failure.retry_after_ms,
                                error = %failure.error,
                                event = "queued_work.wake_retry",
                                "queued-work wake failed; retrying the pending-work claim"
                            ),
                            QueuedWorkWakeDisposition::Terminal => {
                                tracing::warn!(
                                    session_id = failure.session_id.as_deref(),
                                    reason = %failure.reason,
                                    attempt = failure.attempt,
                                    error = %failure.error,
                                    event = "queued_work.wake_terminal",
                                    "queued-work wake stopped after a terminal failure"
                                );
                                return;
                            }
                            QueuedWorkWakeDisposition::Exhausted => {
                                tracing::warn!(
                                    session_id = failure.session_id.as_deref(),
                                    reason = %failure.reason,
                                    attempt = failure.attempt,
                                    error = %failure.error,
                                    event = "queued_work.wake_exhausted",
                                    "queued-work wake exhausted its retry budget"
                                );
                                return;
                            }
                        }
                    }
                }
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(retry_after) => {}
                }
                retry_after = retry_after.saturating_mul(2).min(WAKE_RETRY_MAX);
                attempt = attempt.saturating_add(1);
            }
        })
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
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(QueuedWorkRunError::transient(PluginError::Session(
                    "transient wake failure".to_string(),
                )));
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

        let wake = driver.wake_pending(Some("session-1"), "queued_turn_input");

        tokio::time::timeout(Duration::from_secs(1), accepted)
            .await
            .expect("the failed wake must retry without another enqueue");
        wake.await.expect("wake task");
        assert_eq!(handle.attempts.load(Ordering::SeqCst), 2);
    }

    struct AlwaysFailRunHandle {
        attempts: Arc<AtomicUsize>,
        class: QueuedWorkRunErrorClass,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for AlwaysFailRunHandle {
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let error = PluginError::Session("persistent wake failure".to_string());
            Err(match self.class {
                QueuedWorkRunErrorClass::Transient => QueuedWorkRunError::transient(error),
                QueuedWorkRunErrorClass::Terminal => QueuedWorkRunError::terminal(error),
            })
        }
    }

    #[tokio::test]
    async fn terminal_wake_error_stops_after_one_attempt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let driver = QueuedWorkDriver::new(Arc::new(AlwaysFailRunHandle {
            attempts: Arc::clone(&attempts),
            class: QueuedWorkRunErrorClass::Terminal,
        }));

        driver
            .wake_pending(Some("session-terminal"), "queued_turn_input")
            .await
            .expect("terminal wake task");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_wake_error_stops_at_the_attempt_limit() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let driver = QueuedWorkDriver::new(Arc::new(AlwaysFailRunHandle {
            attempts: Arc::clone(&attempts),
            class: QueuedWorkRunErrorClass::Transient,
        }));

        driver
            .wake_pending(Some("session-exhausted"), "queued_turn_input")
            .await
            .expect("exhausted wake task");

        assert_eq!(attempts.load(Ordering::SeqCst), WAKE_MAX_ATTEMPTS as usize);
    }

    struct BlockingRunHandle {
        entered: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for BlockingRunHandle {
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn dropping_the_driver_cancels_an_inflight_wake() {
        let handle = Arc::new(BlockingRunHandle {
            entered: tokio::sync::Notify::new(),
        });
        let entered = handle.entered.notified();
        let driver = QueuedWorkDriver::new(handle.clone());
        let wake = driver.wake_pending(Some("session-shutdown"), "queued_turn_input");
        entered.await;

        drop(driver);

        tokio::time::timeout(Duration::from_secs(1), wake)
            .await
            .expect("driver drop must cancel the wake")
            .expect("wake task");
    }
}
