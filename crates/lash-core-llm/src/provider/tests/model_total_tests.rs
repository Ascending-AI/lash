//! L-C2: the model total is a hard cap over throttle, backoff and every
//! provider attempt, and a nested call is clipped to the time its enclosing
//! stretch has left (spec v3 Part C, FIG-5171). A route's bounds are the
//! runtime's provider attempt limits unless it states its own within them
//! (FIG-5441).

use super::*;
use crate::provider::{LlmTimeouts, RouteBound};

const SECOND: Duration = Duration::from_secs(1);

/// lash's clock on tokio's paused time: every sleep advances it.
#[derive(Debug)]
struct PausedClock {
    epoch: tokio::time::Instant,
}

impl PausedClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: tokio::time::Instant::now(),
        })
    }

    fn elapsed(&self) -> Duration {
        tokio::time::Instant::now() - self.epoch
    }
}

#[async_trait::async_trait]
impl crate::Clock for PausedClock {
    fn now(&self) -> std::time::Instant {
        tokio::time::Instant::now().into_std()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(std::time::UNIX_EPOCH + Duration::from_secs(1_000) + self.elapsed())
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

/// Every attempt runs `stall` of provider time, then fails retryably,
/// throttled with `retry_after` when it is set.
#[derive(Clone, Debug)]
struct StallingProvider {
    options: ProviderOptions,
    attempts: Arc<AtomicUsize>,
    stall: Duration,
    retry_after: Option<Duration>,
}

#[async_trait::async_trait]
impl Provider for StallingProvider {
    fn kind(&self) -> &'static str {
        "stalling"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Object(Default::default())
    }

    async fn send(
        &mut self,
        _body: &LiveRequestBody,
        _context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.stall).await;
        let failure = LlmTransportError::new("temporarily unavailable");
        Err(match self.retry_after {
            Some(wait) => failure
                .with_kind(ProviderFailureKind::Quota)
                .with_headers([("retry-after", wait.as_secs().to_string())])
                .with_retry_verdict(TransportRetryVerdict::RetryableThrottle {
                    retry_after: Some(wait),
                }),
            None => failure
                .with_kind(ProviderFailureKind::Transport)
                .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

fn budgets() -> lash_sansio::ExecutionBudgets {
    lash_sansio::ExecutionBudgets::new(lash_sansio::ExecutionBudgetsConfig {
        model_total: 60 * SECOND,
        provider: lash_sansio::ProviderAttemptLimits::new(40 * SECOND, 20 * SECOND, 20 * SECOND, 4)
            .expect("valid provider limits"),
        ..lash_sansio::ExecutionBudgetsConfig::default()
    })
    .expect("valid budgets")
}

struct Settled {
    error: ProviderCompletionError,
    elapsed: Duration,
    attempts: usize,
}

async fn complete_under(
    stall: Duration,
    retry_after: Option<Duration>,
    bounds: ModelCallBounds,
) -> Settled {
    let clock = PausedClock::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = StallingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default(),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        stall,
        retry_after,
    };
    let mut handle =
        ProviderHandle::new(ProviderComponents::new(Box::new(provider))).with_clock(clock.clone());
    let mut request = empty_request();
    let sideband = handle.prepare_completion(&mut request);
    let template = Arc::new(handle.lower(&request).await.expect("the request lowers"));
    let error = handle
        .complete_prepared(
            ResponseContext::of_request(&request),
            &template,
            &NoSlotDeliveries,
            sideband,
            crate::ChargeSafetyPolicy::default(),
            &lash_trace::telemetry::metrics::TelemetryMetrics::default(),
            None,
            bounds,
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("a provider that never answers cannot complete"));
    Settled {
        error,
        elapsed: clock.elapsed(),
        attempts: attempts.load(Ordering::SeqCst),
    }
}

fn assert_model_total_exceeded(settled: &Settled, cap: Duration) {
    let budgets = budgets();
    assert!(
        settled.elapsed <= cap + budgets.stop_grace(),
        "the call ran {:?}, past its {cap:?} cap",
        settled.elapsed
    );
    let error = &settled.error.error;
    assert_eq!(
        error.code,
        Some(FailureCode::lash(TurnFailureCode::ModelTotalExceeded)),
        "{error:?}"
    );
    assert_eq!(error.kind, ProviderFailureKind::Timeout);
    assert!(!error.is_retryable());
    let last = settled
        .error
        .call_record
        .attempts
        .last()
        .expect("an attempt was recorded");
    assert_eq!(
        last.retry_decision,
        Some(RetryDecision::Declined(RetryDeclineCause::TimedOut {
            limit: lash_sansio::LimitCause::ExecutionTotal,
        }))
    );
}

#[tokio::test(start_paused = true)]
async fn throttle_waits_never_carry_a_model_call_past_its_total() {
    let budgets = budgets();
    let settled = complete_under(
        Duration::ZERO,
        Some(30 * SECOND),
        ModelCallBounds {
            budgets: budgets.clone(),
            enclosing: None,
        },
    )
    .await;
    assert_model_total_exceeded(&settled, budgets.model_total());
    assert_eq!(
        settled.attempts, 2,
        "the wait that would outlast the total is skipped, not slept"
    );
    assert_eq!(settled.elapsed, 30 * SECOND);
}

#[tokio::test(start_paused = true)]
async fn slow_attempts_and_backoff_are_cut_at_the_model_total() {
    let budgets = budgets();
    let settled = complete_under(
        50 * SECOND,
        None,
        ModelCallBounds {
            budgets: budgets.clone(),
            enclosing: None,
        },
    )
    .await;
    assert_model_total_exceeded(&settled, budgets.model_total());
    assert_eq!(settled.attempts, 2, "the second attempt is cut mid-request");
    assert_eq!(settled.elapsed, budgets.model_total());
}

#[tokio::test(start_paused = true)]
async fn a_nested_model_call_is_cut_at_its_enclosing_limit() {
    let budgets = budgets();
    let enclosing = lash_sansio::ExecutionLimit::starting_at(1_000_000, 20 * SECOND, 20 * SECOND);
    let settled = complete_under(
        50 * SECOND,
        None,
        ModelCallBounds {
            budgets,
            enclosing: Some(enclosing),
        },
    )
    .await;
    assert_model_total_exceeded(&settled, 20 * SECOND);
    assert_eq!(settled.attempts, 1);
    assert_eq!(settled.elapsed, 20 * SECOND);
}

/// Every attempt fails retryably at once, recording the timeouts it was
/// sent with.
#[derive(Clone, Debug)]
struct RecordingProvider {
    options: ProviderOptions,
    sent: Arc<Mutex<Vec<LlmTimeouts>>>,
}

#[async_trait::async_trait]
impl Provider for RecordingProvider {
    fn kind(&self) -> &'static str {
        "recording"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Object(Default::default())
    }

    async fn send(
        &mut self,
        _body: &LiveRequestBody,
        _context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        self.sent.lock_recover().push(self.options.llm_timeouts());
        Err(LlmTransportError::new("temporarily unavailable")
            .with_kind(ProviderFailureKind::Transport)
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// Complete one call on a route of `reliability` under budgets whose
/// provider limits are 40 s per request, 15 s to start, 10 s of chunk
/// silence and 3 attempts: the error it settles with and the timeouts each
/// attempt was sent with.
async fn complete_route(
    reliability: ProviderReliability,
) -> (ProviderCompletionError, Vec<LlmTimeouts>) {
    let budgets = lash_sansio::ExecutionBudgets::new(lash_sansio::ExecutionBudgetsConfig {
        model_total: 600 * SECOND,
        provider: lash_sansio::ProviderAttemptLimits::new(40 * SECOND, 15 * SECOND, 10 * SECOND, 3)
            .expect("valid provider limits"),
        ..lash_sansio::ExecutionBudgetsConfig::default()
    })
    .expect("valid budgets");
    let sent = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingProvider {
        options: ProviderOptions {
            reliability,
            ..ProviderOptions::default()
        },
        sent: Arc::clone(&sent),
    };
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(provider)))
        .with_clock(PausedClock::new());
    let mut request = empty_request();
    let sideband = handle.prepare_completion(&mut request);
    let template = Arc::new(handle.lower(&request).await.expect("the request lowers"));
    let error = handle
        .complete_prepared(
            ResponseContext::of_request(&request),
            &template,
            &NoSlotDeliveries,
            sideband,
            crate::ChargeSafetyPolicy::default(),
            &lash_trace::telemetry::metrics::TelemetryMetrics::default(),
            None,
            ModelCallBounds {
                budgets,
                enclosing: None,
            },
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("a provider that always fails cannot complete"));
    let sent = sent.lock_recover().clone();
    (error, sent)
}

/// A route that states no timeout or attempt count runs every attempt
/// under the runtime's provider attempt limits, as many attempts as they
/// allow; a route bound above its limit refuses the call before any
/// attempt, typed, instead of running under a clipped bound.
#[tokio::test(start_paused = true)]
async fn an_unset_route_bound_is_the_runtimes_and_one_above_it_is_refused() {
    let (_, sent) = complete_route(
        ProviderReliability::default()
            .base_delay_ms(0)
            .max_delay_ms(0),
    )
    .await;
    let runtime = LlmTimeouts {
        request_timeout: Some(40 * SECOND),
        response_start_timeout: Some(15 * SECOND),
        chunk_timeout: Some(10 * SECOND),
    };
    assert_eq!(
        sent,
        vec![runtime; 3],
        "three attempts under the runtime's limits"
    );

    let (_, sent) = complete_route(
        ProviderReliability::default()
            .request_timeout_ms(Some(30_000))
            .base_delay_ms(0)
            .max_delay_ms(0),
    )
    .await;
    assert_eq!(
        sent[0].request_timeout,
        Some(30 * SECOND),
        "a route bound within the limit is its own"
    );

    for (route, bound) in [
        (
            ProviderReliability::default().request_timeout_ms(Some(41_000)),
            RouteBound::RequestTimeout,
        ),
        (
            ProviderReliability::default().response_start_timeout_ms(Some(16_000)),
            RouteBound::ResponseStartTimeout,
        ),
        (
            ProviderReliability::default().stream_chunk_timeout_ms(Some(11_000)),
            RouteBound::ChunkTimeout,
        ),
        (
            ProviderReliability::default().max_attempts(Some(4)),
            RouteBound::MaxAttempts,
        ),
    ] {
        let (error, sent) = complete_route(route).await;
        assert!(sent.is_empty(), "{bound}: no attempt was sent: {sent:?}");
        assert_eq!(
            error.error.code,
            Some(FailureCode::lash(TurnFailureCode::ProviderRouteAboveBudget)),
            "{bound}: {error:?}"
        );
        assert!(
            error.error.message.contains(&bound.to_string()),
            "{bound}: {}",
            error.error.message
        );
    }
}
