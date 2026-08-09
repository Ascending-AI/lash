use super::support::*;
use lash_sansio::sync::MutexExt;

#[derive(Debug)]
pub struct ProviderRateLimiter {
    state: Mutex<ProviderRateLimiterState>,
    clock: Arc<dyn crate::Clock>,
}

#[derive(Debug)]
struct ProviderRateLimiterState {
    policy: ProviderRateLimitPolicy,
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

impl ProviderRateLimiter {
    pub fn new(policy: ProviderRateLimitPolicy) -> Self {
        Self::with_clock(policy, Arc::new(crate::SystemClock))
    }

    pub fn with_clock(policy: ProviderRateLimitPolicy, clock: Arc<dyn crate::Clock>) -> Self {
        let semaphore = policy
            .max_concurrency
            .filter(|limit| *limit > 0)
            .map(tokio::sync::Semaphore::new)
            .map(Arc::new);
        let now = clock.now();
        Self {
            state: Mutex::new(ProviderRateLimiterState {
                policy,
                semaphore,
                request_bucket: WindowBucket::new(now),
                token_bucket: WindowBucket::new(now),
            }),
            clock,
        }
    }

    pub fn configure(&self, policy: ProviderRateLimitPolicy) {
        let mut state = self.state.lock_recover();
        if state.policy.max_concurrency != policy.max_concurrency {
            state.semaphore = policy
                .max_concurrency
                .filter(|limit| *limit > 0)
                .map(tokio::sync::Semaphore::new)
                .map(Arc::new);
        }
        state.policy = policy;
    }

    pub fn clock(&self) -> Arc<dyn crate::Clock> {
        Arc::clone(&self.clock)
    }

    pub async fn admit(&self, request: &LlmRequest) -> ProviderRateLimitPermit {
        let semaphore = self.state.lock_recover().semaphore.clone();
        let concurrency = match semaphore {
            Some(semaphore) => Some(semaphore.acquire_owned().await.expect("semaphore open")),
            None => None,
        };
        self.wait_for_buckets(1, estimate_request_tokens(request))
            .await;
        ProviderRateLimitPermit {
            _concurrency: concurrency,
        }
    }

    async fn wait_for_buckets(&self, requests: u32, tokens: u32) {
        loop {
            let wait = {
                let mut state = self.state.lock_recover();
                let now = self.clock.now();
                let policy = state.policy.clone();
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
                self.clock.sleep(wait).await;
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
        let limiter = ProviderRateLimiter::new(ProviderRateLimitPolicy {
            max_concurrency: Some(0),
            ..Default::default()
        });
        assert!(limiter.state.lock_recover().semaphore.is_none());
        limiter.configure(ProviderRateLimitPolicy {
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
