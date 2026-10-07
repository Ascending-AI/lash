//! L-C2: the model total is a hard cap over throttle, backoff and every
//! provider attempt, and a nested call is clipped to the time its enclosing
//! stretch has left (spec v3 Part C, FIG-5171).

use super::*;

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
        _request: LlmRequest,
        _body: &ProviderRequestBody,
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
            reliability: ProviderReliability::default().max_attempts(16),
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
    let body = handle.lower(&request).await.expect("the request lowers");
    let error = handle
        .complete_prepared(
            request,
            &body,
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
