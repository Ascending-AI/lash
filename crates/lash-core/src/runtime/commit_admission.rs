//! Process-local admission for same-session commit attempts.
//!
//! The durable head compare-and-swap remains the only publication authority.
//! This coordinator removes redundant same-process races by admitting one
//! head load, intent build, and commit attempt per session. Callers acquire a
//! process-wide lane before creating any head-derived state. The durable store
//! CAS still decides whether the attempt advances the head.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lash_sansio::sync::MutexExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Maximum number of waiting attempts retained for one hot session.
const COMMIT_ADMISSION_MAX_WAITERS: usize = 64;

/// Maximum residence of one waiting attempt in the process-local FIFO.
const COMMIT_ADMISSION_WAIT_TTL: Duration = Duration::from_secs(30);

/// The only data retained for a queued commit attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CommitAdmissionClaim {
    session_id: String,
    work_identity: String,
}

impl CommitAdmissionClaim {
    fn new(session_id: impl Into<String>, work_identity: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            work_identity: work_identity.into(),
        }
    }
}

/// A typed refusal from the process-local admission guardrail.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
enum CommitAdmissionError {
    #[error(
        "commit admission queue for session `{session_id}` is full at {queue_depth} waiters (limit {max_waiters}); work `{work_identity}` was not admitted"
    )]
    QueueFull {
        session_id: String,
        work_identity: String,
        queue_depth: usize,
        max_waiters: usize,
    },
    #[error(
        "commit admission for session `{session_id}` timed out after {waited_ms}ms; work `{work_identity}` remains durable and may be retried"
    )]
    TimedOut {
        session_id: String,
        work_identity: String,
        waited_ms: u64,
    },
    #[error(
        "commit admission for session `{session_id}` was cancelled; work `{work_identity}` remains durable"
    )]
    Cancelled {
        session_id: String,
        work_identity: String,
    },
}

/// Admission facts recorded without retaining any head-derived state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CommitAdmissionObservation {
    waited: Duration,
    /// Number of waiters ahead of or including this claim when it queued.
    queue_depth: usize,
}

#[derive(Clone)]
struct CommitAdmissionCoordinator {
    inner: Arc<CommitAdmissionInner>,
}

struct CommitAdmissionInner {
    state: Mutex<CommitAdmissionState>,
    max_waiters: usize,
    wait_ttl: Duration,
}

#[derive(Default)]
struct CommitAdmissionState {
    next_waiter_id: u64,
    sessions: HashMap<String, SessionAdmissionState>,
}

#[derive(Default)]
struct SessionAdmissionState {
    waiters: VecDeque<Arc<CommitAdmissionWaiter>>,
}

struct CommitAdmissionWaiter {
    waiter_id: u64,
    work_identity: String,
    admitted: Notify,
}

impl Default for CommitAdmissionCoordinator {
    fn default() -> Self {
        Self::with_limits(COMMIT_ADMISSION_MAX_WAITERS, COMMIT_ADMISSION_WAIT_TTL)
    }
}

impl CommitAdmissionCoordinator {
    fn with_limits(max_waiters: usize, wait_ttl: Duration) -> Self {
        assert!(max_waiters > 0, "commit admission depth must be nonzero");
        assert!(!wait_ttl.is_zero(), "commit admission TTL must be nonzero");
        Self {
            inner: Arc::new(CommitAdmissionInner {
                state: Mutex::new(CommitAdmissionState::default()),
                max_waiters,
                wait_ttl,
            }),
        }
    }

    async fn run_head_advancing_attempt<T, E, F, Fut>(
        &self,
        session_id: impl Into<String>,
        work_identity: impl Into<String>,
        cancellation: CancellationToken,
        attempt: F,
    ) -> Result<T, E>
    where
        E: From<crate::StoreError>,
        F: FnOnce(Duration, usize) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let guard = self
            .acquire_typed(
                CommitAdmissionClaim::new(session_id, work_identity),
                cancellation,
            )
            .await
            // Admission refusal uses the existing retryable busy/resource
            // path. The private typed distinction remains available to core
            // telemetry and tests without growing the persistence contract.
            .map_err(|_error| E::from(crate::StoreError::Contended))?;
        let result = attempt(guard.observation.waited, guard.observation.queue_depth).await;
        if result.is_ok() {
            guard.release_after_head_advance();
        }
        result
    }

    async fn acquire_typed(
        &self,
        claim: CommitAdmissionClaim,
        cancellation: CancellationToken,
    ) -> Result<CommitAdmissionGuard, CommitAdmissionError> {
        let queued_at = Instant::now();
        let queued = {
            let mut state = self.inner.state.lock_recover();
            if !state.sessions.contains_key(&claim.session_id) {
                state
                    .sessions
                    .insert(claim.session_id.clone(), SessionAdmissionState::default());
                None
            } else {
                let queue_depth = state
                    .sessions
                    .get(&claim.session_id)
                    .map_or(0, |session| session.waiters.len());
                if queue_depth >= self.inner.max_waiters {
                    tracing::warn!(
                        session_id = %claim.session_id,
                        work_identity = %claim.work_identity,
                        queue_depth,
                        max_waiters = self.inner.max_waiters,
                        event = "commit_admission.queue_full",
                        "same-session commit admission queue shed a waiter"
                    );
                    return Err(CommitAdmissionError::QueueFull {
                        session_id: claim.session_id,
                        work_identity: claim.work_identity,
                        queue_depth,
                        max_waiters: self.inner.max_waiters,
                    });
                }
                state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
                let waiter = Arc::new(CommitAdmissionWaiter {
                    waiter_id: state.next_waiter_id,
                    work_identity: claim.work_identity.clone(),
                    admitted: Notify::new(),
                });
                let session = state
                    .sessions
                    .get_mut(&claim.session_id)
                    .expect("session admission state was observed above");
                session.waiters.push_back(Arc::clone(&waiter));
                let queue_depth = session.waiters.len();
                tracing::debug!(
                    session_id = %claim.session_id,
                    work_identity = %claim.work_identity,
                    queue_depth,
                    event = "commit_admission.queued",
                    "same-session commit attempt entered the FIFO"
                );
                Some((waiter, queue_depth))
            }
        };

        let Some((waiter, queue_depth)) = queued else {
            return Ok(CommitAdmissionGuard::new(
                Arc::clone(&self.inner),
                claim,
                CommitAdmissionObservation::default(),
            ));
        };

        enum WaitOutcome {
            Admitted,
            Cancelled,
            TimedOut,
        }
        let outcome = tokio::select! {
            () = waiter.admitted.notified() => WaitOutcome::Admitted,
            () = cancellation.cancelled() => WaitOutcome::Cancelled,
            () = tokio::time::sleep(self.inner.wait_ttl) => WaitOutcome::TimedOut,
        };

        if !matches!(outcome, WaitOutcome::Admitted)
            && self.withdraw_waiter(&claim.session_id, waiter.waiter_id)
        {
            let waited_ms = queued_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            return Err(match outcome {
                WaitOutcome::Cancelled => CommitAdmissionError::Cancelled {
                    session_id: claim.session_id,
                    work_identity: claim.work_identity,
                },
                WaitOutcome::TimedOut => CommitAdmissionError::TimedOut {
                    session_id: claim.session_id,
                    work_identity: claim.work_identity,
                    waited_ms,
                },
                WaitOutcome::Admitted => unreachable!("admitted waiters are not withdrawn"),
            });
        }

        let observation = CommitAdmissionObservation {
            waited: queued_at.elapsed(),
            queue_depth,
        };
        tracing::debug!(
            session_id = %claim.session_id,
            work_identity = %claim.work_identity,
            wait_ms = observation.waited.as_secs_f64() * 1000.0,
            queue_depth,
            event = "commit_admission.admitted",
            "same-session commit attempt left the FIFO"
        );
        Ok(CommitAdmissionGuard::new(
            Arc::clone(&self.inner),
            claim,
            observation,
        ))
    }

    fn withdraw_waiter(&self, session_id: &str, waiter_id: u64) -> bool {
        let mut state = self.inner.state.lock_recover();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return false;
        };
        let Some(index) = session
            .waiters
            .iter()
            .position(|waiter| waiter.waiter_id == waiter_id)
        else {
            // Release already transferred ownership to this waiter. Admission
            // wins the race so the newly active session cannot be orphaned.
            return false;
        };
        session.waiters.remove(index);
        true
    }

    #[cfg(test)]
    fn queued_work_identities(&self, session_id: &str) -> Vec<String> {
        self.inner
            .state
            .lock_recover()
            .sessions
            .get(session_id)
            .map(|session| {
                session
                    .waiters
                    .iter()
                    .map(|waiter| waiter.work_identity.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn active_session_count(&self) -> usize {
        self.inner.state.lock_recover().sessions.len()
    }
}

static PROCESS_COMMIT_ADMISSION: OnceLock<CommitAdmissionCoordinator> = OnceLock::new();

pub(super) fn record_product_commit_admission(
    path: &'static str,
    session_id: &str,
    work_identity: &str,
    waited: Duration,
    queue_depth: usize,
) {
    tracing::debug!(
        path,
        session_id,
        work_identity,
        waited_nanos = waited.as_nanos().min(u128::from(u64::MAX)) as u64,
        queue_depth = queue_depth as u64,
        event = "commit_admission.product_path",
        "product commit path entered after same-session admission"
    );
    #[cfg(test)]
    product_observations()
        .lock_recover()
        .entry(session_id.to_string())
        .or_default()
        .push(ProductCommitAdmissionObservation {
            path,
            work_identity: work_identity.to_string(),
            waited,
            queue_depth,
        });
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProductCommitAdmissionObservation {
    pub(super) path: &'static str,
    pub(super) work_identity: String,
    pub(super) waited: Duration,
    pub(super) queue_depth: usize,
}

#[cfg(test)]
fn product_observations() -> &'static Mutex<HashMap<String, Vec<ProductCommitAdmissionObservation>>>
{
    static OBSERVATIONS: OnceLock<Mutex<HashMap<String, Vec<ProductCommitAdmissionObservation>>>> =
        OnceLock::new();
    OBSERVATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(super) fn take_product_commit_admission_observations(
    session_id: &str,
) -> Vec<ProductCommitAdmissionObservation> {
    product_observations()
        .lock_recover()
        .remove(session_id)
        .unwrap_or_default()
}

#[cfg(test)]
pub(super) fn process_commit_admission_queue_depth(session_id: &str) -> usize {
    PROCESS_COMMIT_ADMISSION
        .get()
        .and_then(|coordinator| {
            coordinator
                .inner
                .state
                .lock_recover()
                .sessions
                .get(session_id)
                .map(|session| session.waiters.len())
        })
        .unwrap_or_default()
}

/// Run one head-advancing attempt after process-wide FIFO admission.
///
/// `attempt` is not invoked until this claim owns the session, so its durable
/// head load and intent build cannot execute while queued. `Ok` means the head
/// advanced and wakes exactly the next node; `Err` releases the lane without
/// claiming an advance. Cross-process publication remains store-CAS governed.
#[doc(hidden)]
pub async fn run_head_advancing_commit_attempt<T, E, F, Fut>(
    session_id: impl Into<String>,
    work_identity: impl Into<String>,
    cancellation: CancellationToken,
    attempt: F,
) -> Result<T, E>
where
    E: From<crate::StoreError>,
    F: FnOnce(Duration, usize) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    PROCESS_COMMIT_ADMISSION
        .get_or_init(CommitAdmissionCoordinator::default)
        .run_head_advancing_attempt(session_id, work_identity, cancellation, attempt)
        .await
}

struct CommitAdmissionGuard {
    inner: Arc<CommitAdmissionInner>,
    claim: CommitAdmissionClaim,
    observation: CommitAdmissionObservation,
    released: bool,
}

impl CommitAdmissionGuard {
    fn new(
        inner: Arc<CommitAdmissionInner>,
        claim: CommitAdmissionClaim,
        observation: CommitAdmissionObservation,
    ) -> Self {
        Self {
            inner,
            claim,
            observation,
            released: false,
        }
    }

    /// Release after the durable head advanced and wake exactly the next FIFO node.
    fn release_after_head_advance(mut self) {
        self.release(true);
    }

    fn release(&mut self, head_advanced: bool) {
        if self.released {
            return;
        }
        self.released = true;
        release_session(&self.inner, &self.claim, head_advanced);
    }
}

impl Drop for CommitAdmissionGuard {
    fn drop(&mut self) {
        self.release(false);
    }
}

fn release_session(
    inner: &CommitAdmissionInner,
    claim: &CommitAdmissionClaim,
    head_advanced: bool,
) {
    let next = {
        let mut state = inner.state.lock_recover();
        let session = state
            .sessions
            .get_mut(&claim.session_id)
            .expect("a live commit admission guard retains its session entry");
        match session.waiters.pop_front() {
            Some(waiter) => Some(waiter),
            None => {
                state.sessions.remove(&claim.session_id);
                None
            }
        }
    };
    tracing::debug!(
        session_id = %claim.session_id,
        work_identity = %claim.work_identity,
        head_advanced,
        next_work_identity = next.as_ref().map(|waiter| waiter.work_identity.as_str()),
        event = "commit_admission.released",
        "same-session commit admission released"
    );
    if let Some(waiter) = next {
        waiter.admitted.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_queue(
        coordinator: &CommitAdmissionCoordinator,
        session_id: &str,
        expected: &[&str],
    ) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if coordinator.queued_work_identities(session_id)
                    == expected
                        .iter()
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>()
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiters entered the admission queue");
    }

    #[tokio::test]
    async fn release_admits_exactly_one_waiter_in_fifo_order_and_cleans_up() {
        let coordinator = CommitAdmissionCoordinator::default();
        let first = coordinator
            .acquire_typed(
                CommitAdmissionClaim::new("session", "first"),
                CancellationToken::new(),
            )
            .await
            .expect("first admission");
        let (admitted_tx, mut admitted_rx) = tokio::sync::mpsc::unbounded_channel();
        for identity in ["second", "third"] {
            let spawned_coordinator = coordinator.clone();
            let admitted_tx = admitted_tx.clone();
            crate::task::spawn(async move {
                let guard = spawned_coordinator
                    .acquire_typed(
                        CommitAdmissionClaim::new("session", identity),
                        CancellationToken::new(),
                    )
                    .await
                    .expect("queued admission");
                admitted_tx
                    .send((identity, guard))
                    .expect("admission receiver remains live");
            });
            let expected = if identity == "second" {
                vec!["second"]
            } else {
                vec!["second", "third"]
            };
            wait_for_queue(&coordinator, "session", &expected).await;
        }

        first.release_after_head_advance();
        let (identity, second) = admitted_rx.recv().await.expect("second admitted");
        assert_eq!(identity, "second");
        assert_eq!(coordinator.queued_work_identities("session"), ["third"]);
        assert!(
            admitted_rx.try_recv().is_err(),
            "release wakes only one waiter"
        );

        second.release_after_head_advance();
        let (identity, third) = admitted_rx.recv().await.expect("third admitted");
        assert_eq!(identity, "third");
        third.release_after_head_advance();
        assert_eq!(coordinator.active_session_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_removes_only_its_fifo_node() {
        let coordinator = CommitAdmissionCoordinator::default();
        let first = coordinator
            .acquire_typed(
                CommitAdmissionClaim::new("session", "first"),
                CancellationToken::new(),
            )
            .await
            .expect("first admission");
        let cancellation = CancellationToken::new();
        let waiter = {
            let coordinator = coordinator.clone();
            let cancellation = cancellation.clone();
            crate::task::spawn(async move {
                coordinator
                    .acquire_typed(
                        CommitAdmissionClaim::new("session", "cancelled"),
                        cancellation,
                    )
                    .await
            })
        };
        wait_for_queue(&coordinator, "session", &["cancelled"]).await;
        cancellation.cancel();
        assert!(matches!(
            waiter.await.expect("waiter joined"),
            Err(CommitAdmissionError::Cancelled { .. })
        ));
        assert!(coordinator.queued_work_identities("session").is_empty());
        drop(first);
        assert_eq!(coordinator.active_session_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_and_depth_guardrails_shed_without_reordering() {
        let coordinator = CommitAdmissionCoordinator::with_limits(1, Duration::from_millis(10));
        let first = coordinator
            .acquire_typed(
                CommitAdmissionClaim::new("session", "first"),
                CancellationToken::new(),
            )
            .await
            .expect("first admission");
        let timeout_waiter = {
            let coordinator = coordinator.clone();
            crate::task::spawn(async move {
                coordinator
                    .acquire_typed(
                        CommitAdmissionClaim::new("session", "timeout"),
                        CancellationToken::new(),
                    )
                    .await
            })
        };
        wait_for_queue(&coordinator, "session", &["timeout"]).await;
        let full = coordinator
            .acquire_typed(
                CommitAdmissionClaim::new("session", "shed"),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            full,
            Err(CommitAdmissionError::QueueFull {
                queue_depth: 1,
                max_waiters: 1,
                ..
            })
        ));
        tokio::time::advance(Duration::from_millis(10)).await;
        assert!(matches!(
            timeout_waiter.await.expect("timeout waiter joined"),
            Err(CommitAdmissionError::TimedOut { .. })
        ));
        drop(first);
        assert_eq!(coordinator.active_session_count(), 0);
    }
}
