use super::support::*;
use lash_sansio::sync::MutexExt;

#[derive(Debug)]
pub struct ProviderRateLimiter {
    state: Mutex<ProviderRateLimiterState>,
}

#[derive(Debug)]
struct ProviderRateLimiterState {
    // Capacity of the installed gate, not an admission-policy authority.
    semaphore_capacity: Option<usize>,
    clock: Arc<dyn crate::Clock>,
    semaphore: Option<Arc<tokio::sync::Semaphore>>,
    request_bucket: WindowBucket,
    token_bucket: WindowBucket,
}

#[derive(Clone, Debug)]
struct WindowBucket {
    used: u32,
    reset_at: std::time::Instant,
}

impl WindowBucket {
    fn new(reset_at: std::time::Instant) -> Self {
        Self { used: 0, reset_at }
    }
}

#[derive(Debug)]
pub struct ProviderRateLimitPermit {
    _concurrency: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Default for ProviderRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRateLimiter {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::SystemClock))
    }

    pub fn with_clock(clock: Arc<dyn crate::Clock>) -> Self {
        let now = clock.now();
        Self {
            state: Mutex::new(ProviderRateLimiterState {
                semaphore_capacity: None,
                clock,
                semaphore: None,
                request_bucket: WindowBucket::new(now),
                token_bucket: WindowBucket::new(now),
            }),
        }
    }

    pub(super) fn set_clock(&self, clock: Arc<dyn crate::Clock>) {
        let mut state = self.state.lock_recover();
        if Arc::ptr_eq(&state.clock, &clock) {
            return;
        }
        let old_now = state.clock.now();
        let now = clock.now();
        // Preserve remaining windows even when the clocks use different epochs.
        state.request_bucket.reset_at = now
            + state
                .request_bucket
                .reset_at
                .saturating_duration_since(old_now);
        state.token_bucket.reset_at = now
            + state
                .token_bucket
                .reset_at
                .saturating_duration_since(old_now);
        state.clock = clock;
    }

    fn concurrency_gate(
        &self,
        policy: &ProviderRateLimitPolicy,
    ) -> Option<Arc<tokio::sync::Semaphore>> {
        let mut state = self.state.lock_recover();
        let limit = policy.max_concurrency.filter(|limit| *limit > 0);
        if state.semaphore_capacity != limit {
            state.semaphore = limit.map(tokio::sync::Semaphore::new).map(Arc::new);
            state.semaphore_capacity = limit;
        }
        state.semaphore.clone()
    }

    pub fn clock(&self) -> Arc<dyn crate::Clock> {
        Arc::clone(&self.state.lock_recover().clock)
    }

    pub async fn admit(
        &self,
        provider: &dyn Provider,
        request: &LlmRequest,
    ) -> ProviderRateLimitPermit {
        let semaphore = self.concurrency_gate(&provider.options().reliability.rate_limits);
        let concurrency = match semaphore {
            Some(semaphore) => Some(semaphore.acquire_owned().await.expect("semaphore open")),
            None => None,
        };
        self.wait_for_buckets(provider, 1, estimate_request_tokens(request))
            .await;
        ProviderRateLimitPermit {
            _concurrency: concurrency,
        }
    }

    async fn wait_for_buckets(&self, provider: &dyn Provider, requests: u32, tokens: u32) {
        loop {
            let policy = provider.options().reliability.rate_limits;
            let wait = {
                let mut state = self.state.lock_recover();
                let now = state.clock.now();
                let request_wait = bucket_wait(
                    &mut state.request_bucket,
                    now,
                    policy.requests_per_window,
                    policy.request_window_ms,
                    requests,
                );
                let token_wait = bucket_wait(
                    &mut state.token_bucket,
                    now,
                    policy.tokens_per_window,
                    policy.token_window_ms,
                    tokens,
                );
                match (request_wait, token_wait) {
                    (None, None) => return,
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) | (None, Some(a)) => Some(a),
                }
            };
            if let Some(wait) = wait {
                self.clock().sleep(wait).await;
            }
        }
    }
}

fn bucket_wait(
    bucket: &mut WindowBucket,
    now: std::time::Instant,
    limit: Option<u32>,
    window_ms: Option<u64>,
    cost: u32,
) -> Option<Duration> {
    let limit = limit.filter(|limit| *limit > 0)?;
    let window = Duration::from_millis(window_ms.unwrap_or(60_000).max(1));
    if now >= bucket.reset_at {
        bucket.used = 0;
        bucket.reset_at = now + window;
    }
    if bucket.used.saturating_add(cost.min(limit)) <= limit {
        bucket.used = bucket.used.saturating_add(cost.min(limit));
        None
    } else {
        Some(bucket.reset_at.saturating_duration_since(now))
    }
}

fn estimate_request_tokens(request: &LlmRequest) -> u32 {
    let mut chars = request.model.len();
    for message in &request.messages {
        for block in message.blocks.iter() {
            match block {
                LlmContentBlock::Text { text, .. } => chars += text.len(),
                LlmContentBlock::ToolCall { input_json, .. } => chars += input_json.len(),
                LlmContentBlock::ToolResult { content, .. } => chars += content.len(),
                LlmContentBlock::Reasoning { text, .. } => chars += text.len(),
                LlmContentBlock::Attachment { .. } => chars += 256,
            }
        }
    }
    chars = chars.saturating_add(
        request
            .attachments
            .iter()
            .filter_map(|source| request.attachment_bytes(source))
            .map(|bytes| bytes.len() / 4)
            .sum(),
    );
    ((chars / 4).max(1)).try_into().unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_and_zero_limits_admit_without_consuming_a_bucket() {
        let now = std::time::Instant::now();
        for limit in [None, Some(0)] {
            let mut bucket = WindowBucket::new(now);
            assert_eq!(bucket_wait(&mut bucket, now, limit, Some(10), 1), None);
            assert_eq!(bucket.used, 0);
        }
    }

    #[test]
    fn exact_window_capacity_is_admitted_and_next_request_waits() {
        let now = std::time::Instant::now();
        let mut bucket = WindowBucket::new(now);
        assert_eq!(bucket_wait(&mut bucket, now, Some(2), Some(50), 1), None);
        assert_eq!(bucket_wait(&mut bucket, now, Some(2), Some(50), 1), None);
        assert_eq!(
            bucket_wait(&mut bucket, now, Some(2), Some(50), 1),
            Some(Duration::from_millis(50))
        );
    }

    #[test]
    fn an_oversized_single_cost_consumes_one_full_window_without_deadlock() {
        let now = std::time::Instant::now();
        let mut bucket = WindowBucket::new(now);
        assert_eq!(bucket_wait(&mut bucket, now, Some(3), Some(20), 99), None);
        assert_eq!(bucket.used, 3);
        assert_eq!(
            bucket_wait(&mut bucket, now, Some(3), Some(20), 1),
            Some(Duration::from_millis(20))
        );
    }

    #[test]
    fn reaching_the_window_boundary_resets_usage_before_admission() {
        let now = std::time::Instant::now();
        let mut bucket = WindowBucket::new(now);
        assert_eq!(bucket_wait(&mut bucket, now, Some(1), Some(10), 1), None);
        let boundary = now + Duration::from_millis(10);
        assert_eq!(
            bucket_wait(&mut bucket, boundary, Some(1), Some(10), 1),
            None
        );
        assert_eq!(bucket.used, 1);
        assert_eq!(bucket.reset_at, boundary + Duration::from_millis(10));
    }

    #[test]
    fn concurrency_zero_is_unlimited_and_positive_values_install_a_gate() {
        let limiter = ProviderRateLimiter::new();
        limiter.concurrency_gate(&ProviderRateLimitPolicy {
            max_concurrency: Some(0),
            ..Default::default()
        });
        assert!(limiter.state.lock_recover().semaphore.is_none());
        limiter.concurrency_gate(&ProviderRateLimitPolicy {
            max_concurrency: Some(2),
            ..Default::default()
        });
        assert_eq!(
            limiter
                .state
                .lock_recover()
                .semaphore
                .as_ref()
                .expect("configured semaphore")
                .available_permits(),
            2
        );
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::Clock;
    use futures_util::FutureExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Debug)]
    struct AdvancingClock {
        epoch: std::time::Instant,
        elapsed_ms: AtomicU64,
    }

    impl AdvancingClock {
        fn new(epoch: std::time::Instant) -> Self {
            Self {
                epoch,
                elapsed_ms: AtomicU64::new(0),
            }
        }
    }

    #[async_trait]
    impl Clock for AdvancingClock {
        fn now(&self) -> std::time::Instant {
            self.epoch + Duration::from_millis(self.timestamp_ms())
        }
        fn timestamp_ms(&self) -> u64 {
            self.elapsed_ms.load(Ordering::SeqCst)
        }
        fn timestamp_rfc3339(&self) -> String {
            self.timestamp_datetime().to_rfc3339()
        }
        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from(
                std::time::UNIX_EPOCH + Duration::from_millis(self.timestamp_ms()),
            )
        }
        async fn sleep(&self, duration: Duration) {
            self.elapsed_ms
                .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
        }
        async fn sleep_until(&self, deadline: std::time::Instant) {
            self.sleep(deadline.saturating_duration_since(self.now()))
                .await;
        }
    }

    #[derive(Clone, Debug)]
    struct MutatingAdmissionProvider(ProviderOptions, bool);

    #[async_trait]
    impl Provider for MutatingAdmissionProvider {
        fn kind(&self) -> &'static str {
            "admission-test"
        }
        fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
            ProviderRouteIdentity::new(self.kind(), self.kind(), model)
        }
        fn options(&self) -> ProviderOptions {
            self.0.clone()
        }
        fn set_options(&mut self, options: ProviderOptions) {
            self.0 = options;
        }
        fn serialize_config(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        async fn complete(
            &mut self,
            _request: LlmRequest,
        ) -> Result<LlmResponse, LlmTransportError> {
            self.0.reliability.rate_limits.requests_per_window = Some(1);
            if std::mem::take(&mut self.1) {
                return Err(LlmTransportError::new("retry after option mutation")
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient));
            }
            Ok(LlmResponse::default())
        }
        fn clone_boxed(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
    }

    fn components(fail_first: bool) -> super::super::handle::ProviderComponents {
        let options = ProviderOptions {
            reliability: ProviderReliability {
                retry: ProviderRetryPolicy {
                    max_attempts: 2,
                    base_delay_ms: 0,
                    max_delay_ms: 0,
                    jitter_ms: 0,
                    ..Default::default()
                },
                rate_limits: ProviderRateLimitPolicy {
                    requests_per_window: Some(2),
                    request_window_ms: Some(1000),
                    max_concurrency: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        super::super::handle::ProviderComponents::new(Box::new(MutatingAdmissionProvider(
            options, fail_first,
        )))
    }

    #[tokio::test]
    async fn admission_reads_options_mutated_inside_completion() {
        let clock = Arc::new(AdvancingClock::new(std::time::Instant::now()));
        let mut handle =
            super::super::handle::ProviderHandle::new(components(true).with_clock(clock.clone()));
        let completion = handle
            .complete(super::super::tests::empty_request())
            .await
            .unwrap();
        assert_eq!(completion.call_record.attempts.len(), 2);
        assert_eq!(
            handle.options().reliability.rate_limits.requests_per_window,
            Some(1)
        );
        assert_eq!(
            clock.timestamp_ms(),
            1000,
            "second admission must honor the provider's mutated one-request window"
        );
    }

    #[tokio::test]
    async fn cloned_bindings_and_clock_injection_preserve_limiter_and_usage() {
        let epoch = std::time::Instant::now();
        let clock = Arc::new(AdvancingClock::new(epoch));
        let components = components(false).with_clock(clock);
        let limiter = Arc::clone(&components.rate_limiter);
        let mut first = super::super::handle::ProviderHandle::new(components.clone());
        first
            .complete(super::super::tests::empty_request())
            .await
            .unwrap();
        let gate = limiter.state.lock_recover().semaphore.clone().unwrap();
        let held = Arc::clone(&gate).acquire_owned().await.unwrap();
        let replacement = Arc::new(AdvancingClock::new(epoch + Duration::from_secs(30)));
        let second = components.clone().with_clock(replacement.clone());
        assert!(Arc::ptr_eq(&limiter, &second.rate_limiter));
        let mut second_handle = first.clone().with_clock(replacement.clone());
        {
            let state = limiter.state.lock_recover();
            assert_eq!(state.request_bucket.used, 1);
            assert_eq!(
                state.request_bucket.reset_at,
                replacement.now() + Duration::from_secs(1)
            );
            assert!(Arc::ptr_eq(&gate, state.semaphore.as_ref().unwrap()));
            assert_eq!(gate.available_permits(), 0);
        }
        let request = super::super::tests::empty_request();
        assert!(
            second_handle
                .complete(request.clone())
                .now_or_never()
                .is_none(),
            "the cloned binding shares the outstanding concurrency permit"
        );
        drop(held);
        second_handle.complete(request).await.unwrap();
        assert_eq!(replacement.timestamp_ms(), 1000);
        assert_eq!(gate.available_permits(), 1);
    }
}
