use async_trait::async_trait;
use lash_core::runtime::{Clock, SystemClock};
use lash_sansio::sync::RwLockExt;
use std::fmt::{Debug, Display};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A cloneable credential whose formatting implementations redact secrets.
pub trait Credential: Clone + Debug + Display + Send + Sync + 'static {
    fn expires_at(&self) -> Option<SystemTime>;
}

#[async_trait]
pub trait CredentialRefresher<C: Credential>: Send + Sync {
    async fn refresh(&self, current: &C, cause: RefreshCause) -> Result<C, CredentialError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshCause {
    Proactive,
    Rejected,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{kind}")]
pub struct CredentialError {
    pub kind: CredentialErrorKind,
    pub retryable: bool,
}

impl CredentialError {
    pub const fn new(kind: CredentialErrorKind, retryable: bool) -> Self {
        Self { kind, retryable }
    }

    pub const fn invalid_grant() -> Self {
        Self::new(CredentialErrorKind::InvalidGrant, false)
    }

    pub const fn transient() -> Self {
        Self::new(CredentialErrorKind::Transient, true)
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredentialErrorKind {
    #[error("credential refresh was rejected; sign in again")]
    InvalidGrant,
    #[error("credential refresh failed transiently")]
    Transient,
    #[error("credential refresh failed")]
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct CredentialPolicy {
    pub refresh_before: Duration,
    pub skew: Duration,
}

impl Default for CredentialPolicy {
    fn default() -> Self {
        Self {
            refresh_before: Duration::from_secs(5 * 60),
            skew: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Lease<C: Credential> {
    pub value: C,
    pub generation: u64,
}

/// A provider call failure annotated with whether replay is still safe.
#[derive(Debug)]
pub enum CredentialCallError<E> {
    /// Authentication was rejected before any output escaped.
    PreOutputAuth(E),
    /// The attempt failed before output, but not due to authentication.
    Failed(E),
    /// At least one output event escaped; replay would duplicate output.
    PostOutput(E),
}

impl<E> CredentialCallError<E> {
    fn into_inner(self) -> E {
        match self {
            Self::PreOutputAuth(error) | Self::Failed(error) | Self::PostOutput(error) => error,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialExecuteError<E> {
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error("provider call failed")]
    Call(E),
}

type PersistFuture = Pin<Box<dyn Future<Output = Result<(), CredentialError>> + Send>>;
type PersistCallback<C> = dyn Fn(C) -> PersistFuture + Send + Sync;

struct State<C> {
    value: C,
    generation: u64,
    failure_latch: Option<CredentialError>,
}

struct Inner<C: Credential> {
    state: RwLock<State<C>>,
    refresh_gate: tokio::sync::Mutex<()>,
    refresher: Arc<dyn CredentialRefresher<C>>,
    persist: Option<Arc<PersistCallback<C>>>,
    clock: Arc<dyn Clock>,
    policy: CredentialPolicy,
}

pub struct CredentialManager<C: Credential> {
    inner: Arc<Inner<C>>,
}

impl<C: Credential> Clone for CredentialManager<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C: Credential> Debug for CredentialManager<C> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.state.read_recover();
        formatter
            .debug_struct("CredentialManager")
            .field("credential", &state.value)
            .field("generation", &state.generation)
            .field("failure_latched", &state.failure_latch.is_some())
            .field("policy", &self.inner.policy)
            .finish()
    }
}

impl<C: Credential> CredentialManager<C> {
    pub fn new(value: C, refresher: Arc<dyn CredentialRefresher<C>>) -> Self {
        Self::with_clock_and_policy(
            value,
            refresher,
            Arc::new(SystemClock),
            CredentialPolicy::default(),
        )
    }

    pub fn with_clock_and_policy(
        value: C,
        refresher: Arc<dyn CredentialRefresher<C>>,
        clock: Arc<dyn Clock>,
        policy: CredentialPolicy,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: RwLock::new(State {
                    value,
                    generation: 0,
                    failure_latch: None,
                }),
                refresh_gate: tokio::sync::Mutex::new(()),
                refresher,
                persist: None,
                clock,
                policy,
            }),
        }
    }

    pub fn with_persist<F, Fut>(mut self, persist: F) -> Self
    where
        F: Fn(C) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), CredentialError>> + Send + 'static,
    {
        let callback = move |credential: C| -> PersistFuture { Box::pin(persist(credential)) };
        Arc::get_mut(&mut self.inner)
            .expect("persist callback must be configured before cloning the manager")
            .persist = Some(Arc::new(callback));
        self
    }

    pub async fn lease(&self) -> Result<Lease<C>, CredentialError> {
        let lease = self.current_lease()?;
        if self.needs_proactive_refresh(&lease.value) {
            self.refresh_if_current(lease.generation, RefreshCause::Proactive)
                .await
        } else {
            Ok(lease)
        }
    }

    pub async fn refresh_if_current(
        &self,
        generation: u64,
        cause: RefreshCause,
    ) -> Result<Lease<C>, CredentialError> {
        let _gate = self.inner.refresh_gate.lock().await;
        let current = self.current_lease()?;
        if current.generation != generation {
            return Ok(current);
        }
        if let Some(error) = self.inner.state.read_recover().failure_latch.clone() {
            return Err(error);
        }

        let refreshed = match self.inner.refresher.refresh(&current.value, cause).await {
            Ok(value) => value,
            Err(error) => {
                self.inner.state.write_recover().failure_latch = Some(error.clone());
                return Err(error);
            }
        };

        let persist_result = if let Some(persist) = &self.inner.persist {
            persist(refreshed.clone()).await
        } else {
            Ok(())
        };

        let next = {
            let mut state = self.inner.state.write_recover();
            state.value = refreshed;
            state.generation = state.generation.saturating_add(1);
            // Persistence failure is returned to the refresh leader below, but
            // must not poison later leases: the rotated in-memory credential is
            // live and falling back to (or indefinitely blocking on) the dead
            // credential would defeat the refresh.
            state.failure_latch = None;
            Lease {
                value: state.value.clone(),
                generation: state.generation,
            }
        };
        persist_result.map(|()| next)
    }

    pub async fn execute<T, E, F, Fut>(&self, mut call: F) -> Result<T, CredentialExecuteError<E>>
    where
        F: FnMut(Lease<C>) -> Fut,
        Fut: Future<Output = Result<T, CredentialCallError<E>>>,
    {
        let first = self.lease().await?;
        match call(first.clone()).await {
            Ok(value) => Ok(value),
            Err(CredentialCallError::PreOutputAuth(_)) => {
                let refreshed = self
                    .refresh_if_current(first.generation, RefreshCause::Rejected)
                    .await?;
                call(refreshed)
                    .await
                    .map_err(|error| CredentialExecuteError::Call(error.into_inner()))
            }
            Err(error) => Err(CredentialExecuteError::Call(error.into_inner())),
        }
    }

    pub fn snapshot(&self) -> C {
        self.inner.state.read_recover().value.clone()
    }

    fn current_lease(&self) -> Result<Lease<C>, CredentialError> {
        let state = self.inner.state.read_recover();
        if let Some(error) = &state.failure_latch {
            return Err(error.clone());
        }
        Ok(Lease {
            value: state.value.clone(),
            generation: state.generation,
        })
    }

    fn needs_proactive_refresh(&self, credential: &C) -> bool {
        let Some(expires_at) = credential.expires_at() else {
            return false;
        };
        let margin = self
            .inner
            .policy
            .refresh_before
            .saturating_add(self.inner.policy.skew);
        let refresh_at = expires_at.checked_sub(margin).unwrap_or(UNIX_EPOCH);
        let now = UNIX_EPOCH + Duration::from_millis(self.inner.clock.timestamp_ms());
        now >= refresh_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Instant;
    use tokio::sync::{Barrier, Notify};

    #[derive(Clone)]
    struct TestCredential {
        secret: String,
        expires_at: Option<SystemTime>,
    }

    impl Debug for TestCredential {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("TestCredential([REDACTED])")
        }
    }

    impl Display for TestCredential {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("[REDACTED]")
        }
    }

    impl Credential for TestCredential {
        fn expires_at(&self) -> Option<SystemTime> {
            self.expires_at
        }
    }

    #[derive(Debug)]
    struct TestClock(AtomicU64);

    #[async_trait]
    impl Clock for TestClock {
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn timestamp_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        fn timestamp_rfc3339(&self) -> String {
            String::new()
        }
        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::UNIX_EPOCH
        }
        async fn sleep(&self, _duration: Duration) {}
        async fn sleep_until(&self, _deadline: Instant) {}
    }

    struct TestRefresher {
        calls: AtomicUsize,
        result: Result<TestCredential, CredentialError>,
    }

    #[async_trait]
    impl CredentialRefresher<TestCredential> for TestRefresher {
        async fn refresh(
            &self,
            _current: &TestCredential,
            _cause: RefreshCause,
        ) -> Result<TestCredential, CredentialError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn credential(secret: &str, expires_secs: u64) -> TestCredential {
        TestCredential {
            secret: secret.to_string(),
            expires_at: Some(UNIX_EPOCH + Duration::from_secs(expires_secs)),
        }
    }

    fn manager(refresher: Arc<TestRefresher>, now_secs: u64) -> CredentialManager<TestCredential> {
        CredentialManager::with_clock_and_policy(
            credential("old", 100),
            refresher,
            Arc::new(TestClock(AtomicU64::new(now_secs * 1000))),
            CredentialPolicy {
                refresh_before: Duration::ZERO,
                skew: Duration::ZERO,
            },
        )
    }

    #[tokio::test]
    async fn herd_refreshes_once_per_generation() {
        let refresher = Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            result: Ok(credential("new", 1000)),
        });
        let manager = manager(Arc::clone(&refresher), 200);
        let barrier = Arc::new(Barrier::new(16));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let manager = manager.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                manager.lease().await.unwrap().generation
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), 1);
        }
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn proactive_refreshes_inside_policy_window() {
        let refresher = Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            result: Ok(credential("new", 1000)),
        });
        let manager = CredentialManager::with_clock_and_policy(
            credential("old", 400),
            refresher.clone(),
            Arc::new(TestClock(AtomicU64::new(100_000))),
            CredentialPolicy {
                refresh_before: Duration::from_secs(250),
                skew: Duration::from_secs(50),
            },
        );
        assert_eq!(manager.lease().await.unwrap().generation, 1);
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pre_output_401_refreshes_and_replays_once() {
        let refresher = Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            result: Ok(credential("new", 1000)),
        });
        let manager = manager(Arc::clone(&refresher), 0);
        let calls = AtomicUsize::new(0);
        let result = manager
            .execute(|lease| {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(CredentialCallError::PreOutputAuth("401"))
                    } else {
                        Ok((lease.value.secret, lease.generation))
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(result, ("new".to_string(), 1));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn post_output_failure_is_never_replayed() {
        let refresher = Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            result: Ok(credential("new", 1000)),
        });
        let manager = manager(Arc::clone(&refresher), 0);
        let calls = AtomicUsize::new(0);
        let result: Result<(), _> = manager
            .execute(|_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(CredentialCallError::PostOutput("stream failed")) }
            })
            .await;
        assert!(matches!(
            result,
            Err(CredentialExecuteError::Call("stream failed"))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn persistence_is_awaited_before_publish_and_failure_keeps_new_value() {
        let refresher = Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            result: Ok(credential("new", 1000)),
        });
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let manager = manager(Arc::clone(&refresher), 0).with_persist({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |_| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                async move {
                    started.notify_one();
                    release.notified().await;
                    Err(CredentialError::transient())
                }
            }
        });
        let task = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.refresh_if_current(0, RefreshCause::Rejected).await })
        };
        started.notified().await;
        assert_eq!(manager.snapshot().secret, "old");
        release.notify_one();
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            CredentialError::transient()
        );
        assert_eq!(manager.snapshot().secret, "new");
    }

    #[tokio::test]
    async fn invalid_grant_is_typed_and_non_retryable() {
        let refresher = Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            result: Err(CredentialError::invalid_grant()),
        });
        let error = manager(refresher, 200).lease().await.unwrap_err();
        assert_eq!(error.kind, CredentialErrorKind::InvalidGrant);
        assert!(!error.retryable);
    }
}
