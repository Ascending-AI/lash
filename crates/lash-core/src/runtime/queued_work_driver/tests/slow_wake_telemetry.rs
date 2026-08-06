use super::*;

struct HungRunHandle {
    entered: AtomicUsize,
    dropped: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for HungRunHandle {
    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _probe = DropProbe(Arc::clone(&self.dropped));
        self.entered.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

/// Wall-clock timing made this load-sensitive: the heartbeat count came from
/// a 30ms real sleep against a 10ms threshold, and `available_permits` was
/// only `Some(0)` when both admissions landed inside the first threshold
/// window. Under CI load a heartbeat could fire between the two admissions
/// and read `Some(1)` (FIG-957). The paused clock removes both races: nothing
/// fires until this test advances virtual time, and it only advances after
/// confirming both executions are admitted and parked, so every heartbeat is
/// observed in the fully-saturated steady state. The `yield_now` spins keep
/// the runtime busy so tokio's auto-advance cannot move the clock behind our
/// back.
#[tokio::test(start_paused = true)]
async fn hung_executions_are_bounded_warn_when_slow_and_shutdown_cleanly() {
    const CONCURRENCY: usize = 2;
    const SIGNALS: usize = 8;
    const THRESHOLD: Duration = Duration::from_millis(10);
    const HEARTBEAT_WINDOWS: usize = 3;
    const SPIN_YIELDS: usize = 256;

    let handle = Arc::new(HungRunHandle {
        entered: AtomicUsize::new(0),
        dropped: Arc::new(AtomicUsize::new(0)),
    });
    let captured_handle = Arc::clone(&handle);
    let ((), capture) = crate::runtime::tests::trace_capture::capturing(|| async move {
        let driver = QueuedWorkDriver::from_parts(
            captured_handle.clone(),
            CancellationToken::new(),
            Some(QueuedWorkExecutionConcurrency::new(CONCURRENCY).expect("valid concurrency")),
            THRESHOLD,
        );
        for index in 0..SIGNALS {
            driver.notify_pending_work(Some(&format!("hung-{index}")), "process_wake");
        }
        // Admission needs no timer, so spinning on `yield_now` reaches the
        // saturated state without letting virtual time move.
        for _ in 0..SPIN_YIELDS {
            if captured_handle.entered.load(Ordering::SeqCst) == CONCURRENCY {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            captured_handle.entered.load(Ordering::SeqCst),
            CONCURRENCY,
            "only the bounded executions enter"
        );

        // Each advance crosses exactly one threshold window, so every
        // admitted execution owes exactly one heartbeat per window.
        for _ in 0..HEARTBEAT_WINDOWS {
            tokio::time::advance(THRESHOLD).await;
            for _ in 0..SPIN_YIELDS {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(captured_handle.entered.load(Ordering::SeqCst), CONCURRENCY);

        drop(driver);
        for _ in 0..SPIN_YIELDS {
            if captured_handle.dropped.load(Ordering::SeqCst) >= CONCURRENCY {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            captured_handle.dropped.load(Ordering::SeqCst),
            CONCURRENCY,
            "shutdown drops every admitted hung execution"
        );
    })
    .await;

    let slow = capture.named("queued_work.wake_slow");
    // Exact, not a floor: a heartbeat that stops repeating undershoots and a
    // heartbeat that stops re-arming against the threshold overshoots.
    assert_eq!(
        slow.len(),
        CONCURRENCY * HEARTBEAT_WINDOWS,
        "each wedged wake must emit one slow-wake heartbeat per threshold window"
    );
    assert!(slow.iter().all(|event| {
        event.level == "WARN"
            && event.target == "lash_core::queued_work"
            && event.field("threshold_ms") == "10"
            && event.field("reason") == "process_wake"
            && event.field("available_permits") == "Some(0)"
            && event.field("admission_limit") == "Some(2)"
    }));
    // The heartbeat must attribute itself to a distinct wedged session per
    // admitted execution, so a single looping demand cannot satisfy the count.
    let sessions = slow
        .iter()
        .map(|event| event.field("session_id").to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        sessions.len(),
        CONCURRENCY,
        "the heartbeats must come from every admitted execution, not one repeat"
    );
}
