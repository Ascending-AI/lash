use super::*;

fn default_wake_attempts() -> u32 {
    WorkCadencePolicy::default().max_transient_attempts.get()
}

fn default_retry_max() -> Duration {
    WorkCadencePolicy::default().retry_max
}

struct ContentionThenFailureRunHandle {
    passes: AtomicUsize,
    transient_attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for ContentionThenFailureRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        Ok(Some(true))
    }

    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        unreachable!("the progress-aware entrypoint is used")
    }

    async fn claim_and_run_pending_with_progress(
        &self,
        _session_id: Option<&str>,
        _reason: &str,
    ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
        let pass = self.passes.fetch_add(1, Ordering::SeqCst);
        if pass < default_wake_attempts() as usize {
            return Ok(QueuedWorkRunProgress::Blocked);
        }
        self.transient_attempts.fetch_add(1, Ordering::SeqCst);
        Err(QueuedWorkRunError::transient(PluginError::Session(
            "transient failure after contention".to_string(),
        )))
    }
}

#[tokio::test(start_paused = true)]
async fn contention_preserves_the_full_transient_error_attempt_budget() {
    let handle = Arc::new(ContentionThenFailureRunHandle {
        passes: AtomicUsize::new(0),
        transient_attempts: AtomicUsize::new(0),
    });
    let driver =
        NativeQueuedWork::with_execution_concurrency(handle.clone(), 1).expect("valid concurrency");

    driver.notify_pending_work(Some("session-contended-then-failing"), "queued_turn_input");
    for _ in 0..(default_wake_attempts() * 3) {
        tokio::task::yield_now().await;
        tokio::time::advance(default_retry_max()).await;
    }

    assert_eq!(
        handle.transient_attempts.load(Ordering::SeqCst),
        default_wake_attempts() as usize,
        "contention must not consume any transient-error attempts"
    );
}

struct UnknownClaimabilityTransientRunHandle {
    attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for UnknownClaimabilityTransientRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        Ok(None)
    }

    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        unreachable!("the progress-aware entrypoint is used")
    }

    async fn claim_and_run_pending_with_progress(
        &self,
        _session_id: Option<&str>,
        _reason: &str,
    ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(QueuedWorkRunError::transient(PluginError::Session(
            "transient failure with unknown claimability".to_string(),
        )))
    }
}

#[tokio::test(start_paused = true)]
async fn unknown_claimability_transient_errors_exhaust_then_notification_rearms() {
    let handle = Arc::new(UnknownClaimabilityTransientRunHandle {
        attempts: AtomicUsize::new(0),
    });
    let driver =
        NativeQueuedWork::with_execution_concurrency(handle.clone(), 1).expect("valid concurrency");

    driver.notify_pending_work(Some("session-unknown-failing"), "first_enqueue");
    for _ in 0..(default_wake_attempts() * 2) {
        tokio::task::yield_now().await;
        tokio::time::advance(default_retry_max()).await;
    }
    assert_eq!(
        handle.attempts.load(Ordering::SeqCst),
        default_wake_attempts() as usize,
        "one unknown-claimability notification receives one bounded transient ladder"
    );

    tokio::time::advance(default_retry_max() * 4).await;
    tokio::task::yield_now().await;
    assert_eq!(
        handle.attempts.load(Ordering::SeqCst),
        default_wake_attempts() as usize,
        "exhausted unknown-claimability demand must remain idle"
    );

    driver.notify_pending_work(Some("session-unknown-failing"), "second_enqueue");
    for _ in 0..(default_wake_attempts() * 2) {
        tokio::task::yield_now().await;
        tokio::time::advance(default_retry_max()).await;
    }
    assert_eq!(
        handle.attempts.load(Ordering::SeqCst),
        (default_wake_attempts() * 2) as usize,
        "a later notification must re-arm one fresh bounded transient ladder"
    );
}

#[tokio::test(start_paused = true)]
async fn indefinite_contention_emits_repeating_typed_heartbeats() {
    let handle = Arc::new(ContendedRunHandle {
        peeks: AtomicUsize::new(0),
        hydrations: AtomicUsize::new(0),
        blocked: tokio::sync::Notify::new(),
    });
    let captured_handle = Arc::clone(&handle);
    let ((), capture) =
        crate::runtime::tests::trace_capture::capturing_with_capture(|capture| async move {
            let driver = NativeQueuedWork::from_parts(
                captured_handle,
                CancellationToken::new(),
                Some(QueuedWorkExecutionConcurrency::new(1).expect("valid concurrency")),
                Duration::from_millis(50),
            );
            let mut blocked = handle.blocked.notified();
            driver.notify_pending_work(Some("session-contended"), "queued_turn_input");
            // Each blocked pass is the rendezvous that proves the prior virtual
            // retry window completed. The extra final pass proves the preceding
            // retry reacquired its queued-work permit before capture is released.
            for _ in 0..7 {
                blocked.await;
                blocked = handle.blocked.notified();
                tokio::time::advance(default_retry_max()).await;
            }
            // The permit emits synchronously when reacquisition is polled, but
            // keep the observation itself as the final rendezvous before the
            // capture layer is released.
            for _ in 0..256 {
                if capture
                    .named("queued_work_execution_permit.reacquire")
                    .len()
                    >= 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;

    let contended = capture.named("queued_work.wake_contended");
    assert!(
        contended.len() >= 2,
        "indefinite contention must emit repeating typed heartbeats"
    );
    assert!(contended.iter().all(|event| {
        event.level == "WARN"
            && event.target == "lash_core::queued_work"
            && event.field("reason") == "queued_turn_input"
            && event.field("threshold_ms") == "50"
            && event.field("admission_limit") == "Some(1)"
    }));
    assert!(
        capture
            .named("process_execution_permit.reacquire")
            .is_empty(),
        "queued-work slots must not emit process-permit telemetry"
    );
    assert!(
        !capture
            .named("queued_work_execution_permit.reacquire")
            .is_empty(),
        "queued-work park/reacquire telemetry keeps its own event name"
    );
}
