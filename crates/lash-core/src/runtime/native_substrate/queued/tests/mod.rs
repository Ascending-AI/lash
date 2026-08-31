use lash_sansio::sync::MutexExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::QueuedWorkExecutionConcurrency;
use super::*;

struct BurstRunHandle {
    hydrations: AtomicUsize,
    pending: Mutex<VecDeque<usize>>,
    observed: Mutex<Vec<usize>>,
    reasons: Mutex<Vec<String>>,
    completed: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for BurstRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        Ok(Some(!self.pending.lock_recover().is_empty()))
    }

    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        self.reasons.lock_recover().push(request.reason);
        let drained = {
            let mut pending = self.pending.lock_recover();
            let limit = pending.len().min(3);
            pending.drain(..limit).collect::<Vec<_>>()
        };
        self.observed.lock_recover().extend(drained);
        self.completed.notify_one();
        Ok(())
    }

    async fn claim_and_run_pending_with_progress(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
        self.claim_and_run_pending(session_id, reason).await?;
        Ok(QueuedWorkRunProgress::Claimed)
    }
}

#[tokio::test]
async fn burst_for_one_session_drains_ordered_batches_with_coalesced_hydrations() {
    const SIGNALS: usize = 8;
    let handle = Arc::new(BurstRunHandle {
        hydrations: AtomicUsize::new(0),
        pending: Mutex::new((0..SIGNALS).collect()),
        observed: Mutex::new(Vec::new()),
        reasons: Mutex::new(Vec::new()),
        completed: tokio::sync::Notify::new(),
    });
    let driver = NativeQueuedWork::new(handle.clone());
    for index in 0..SIGNALS {
        let reason = if index % 2 == 0 {
            "queued_turn_input"
        } else {
            "process_wake"
        };
        driver.notify_pending_work(Some("session-burst"), reason);
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let completed = handle.completed.notified();
            if handle.observed.lock_recover().len() == SIGNALS {
                break;
            }
            completed.await;
        }
    })
    .await
    .expect("all queued work drains in bounded batches");
    assert_eq!(handle.hydrations.load(Ordering::SeqCst), 3);
    assert_eq!(
        *handle.observed.lock_recover(),
        (0..SIGNALS).collect::<Vec<_>>()
    );
    assert_eq!(
        *handle.reasons.lock_recover(),
        vec![
            "queued_turn_input,process_wake",
            "queued_turn_input,process_wake",
            "queued_turn_input,process_wake",
        ]
    );
}

struct AdmissionRunHandle {
    active: AtomicUsize,
    max_active: AtomicUsize,
    entered: AtomicUsize,
    completed: AtomicUsize,
    changed: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for AdmissionRunHandle {
    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        self.release
            .acquire()
            .await
            .expect("release remains open")
            .forget();
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        Ok(())
    }
}

#[tokio::test]
async fn default_slot_supplier_releases_permits_and_preserves_admission_bound() {
    const SIGNALS: usize = 8;
    const CONCURRENCY: usize = 2;
    let handle = Arc::new(AdmissionRunHandle {
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        entered: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        changed: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let driver = NativeQueuedWork::with_execution_concurrency(handle.clone(), CONCURRENCY)
        .expect("valid concurrency");
    for index in 0..SIGNALS {
        driver.notify_pending_work(Some(&format!("session-{index}")), "queued_turn_input");
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let changed = handle.changed.notified();
            if handle.entered.load(Ordering::SeqCst) == CONCURRENCY {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("the configured number of executions is admitted");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(handle.entered.load(Ordering::SeqCst), CONCURRENCY);
    assert_eq!(handle.max_active.load(Ordering::SeqCst), CONCURRENCY);

    handle.release.add_permits(SIGNALS);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let changed = handle.changed.notified();
            if handle.completed.load(Ordering::SeqCst) == SIGNALS {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("all retained demand executes after permits are released");
    assert_eq!(handle.max_active.load(Ordering::SeqCst), CONCURRENCY);
}

#[tokio::test]
async fn external_engine_submitters_do_not_inherit_the_native_admission_bound() {
    const SIGNALS: usize = 8;
    let handle = Arc::new(AdmissionRunHandle {
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        entered: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        changed: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let driver = NativeQueuedWork::new(handle.clone());
    for index in 0..SIGNALS {
        driver.notify_pending_work(Some(&format!("engine-session-{index}")), "engine_submit");
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let changed = handle.changed.notified();
            if handle.entered.load(Ordering::SeqCst) == SIGNALS {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("the engine substrate admits every submitted session");
    assert_eq!(handle.max_active.load(Ordering::SeqCst), SIGNALS);
    handle.release.add_permits(SIGNALS);
}

struct ParkAwareRunHandle {
    first_parked: tokio::sync::Notify,
    second_entered: tokio::sync::Notify,
    resume_first: tokio::sync::Semaphore,
    completed: AtomicUsize,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for ParkAwareRunHandle {
    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        match request.session_id.as_deref() {
            Some("session-parked") => {
                self.first_parked.notify_one();
                crate::runtime::process_worker::release_process_execution_permit_while(async {
                    self.resume_first
                        .acquire()
                        .await
                        .expect("resume semaphore remains open")
                        .forget();
                })
                .await;
            }
            Some("session-runnable") => self.second_entered.notify_one(),
            session => panic!("unexpected session: {session:?}"),
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn native_admission_slot_is_released_while_a_turn_is_parked() {
    let handle = Arc::new(ParkAwareRunHandle {
        first_parked: tokio::sync::Notify::new(),
        second_entered: tokio::sync::Notify::new(),
        resume_first: tokio::sync::Semaphore::new(0),
        completed: AtomicUsize::new(0),
    });
    let driver =
        NativeQueuedWork::with_execution_concurrency(handle.clone(), 1).expect("valid concurrency");
    driver.notify_pending_work(Some("session-parked"), "queued_turn_input");
    handle.first_parked.notified().await;

    driver.notify_pending_work(Some("session-runnable"), "queued_turn_input");
    tokio::time::timeout(Duration::from_secs(1), handle.second_entered.notified())
        .await
        .expect("a parked native turn releases its queued-work slot");
    handle.resume_first.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), async {
        while handle.completed.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the parked turn reacquires its slot and completes");
}

struct RerunRunHandle {
    pending: Mutex<VecDeque<usize>>,
    observed: Mutex<Vec<usize>>,
    reasons: Mutex<Vec<String>>,
    runs: AtomicUsize,
    first_entered: tokio::sync::Notify,
    release_first: tokio::sync::Semaphore,
    completed: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for RerunRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        Ok(Some(!self.pending.lock_recover().is_empty()))
    }

    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        self.reasons.lock_recover().push(request.reason);
        let drained = self.pending.lock_recover().drain(..).collect::<Vec<_>>();
        self.observed.lock_recover().extend(drained);
        if run == 0 {
            self.first_entered.notify_one();
            self.release_first
                .acquire()
                .await
                .expect("release remains open")
                .forget();
        }
        self.completed.notify_one();
        Ok(())
    }
}

#[tokio::test]
async fn signal_during_an_inflight_run_schedules_exactly_one_rerun() {
    let handle = Arc::new(RerunRunHandle {
        pending: Mutex::new(VecDeque::from([0])),
        observed: Mutex::new(Vec::new()),
        reasons: Mutex::new(Vec::new()),
        runs: AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
        release_first: tokio::sync::Semaphore::new(0),
        completed: tokio::sync::Notify::new(),
    });
    let driver = NativeQueuedWork::new(handle.clone());
    driver.notify_pending_work(Some("session-rerun"), "first");
    handle.first_entered.notified().await;

    handle.pending.lock_recover().push_back(1);
    driver.notify_pending_work(Some("session-rerun"), "second");
    driver.notify_pending_work(Some("session-rerun"), "second");
    handle.release_first.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), async {
        while handle.runs.load(Ordering::SeqCst) < 2 {
            handle.completed.notified().await;
        }
    })
    .await
    .expect("the in-flight signal causes one rerun");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(handle.runs.load(Ordering::SeqCst), 2);
    assert_eq!(*handle.observed.lock_recover(), vec![0, 1]);
    assert_eq!(
        *handle.reasons.lock_recover(),
        vec!["first", "second"],
        "the in-flight signal is attributed to the rerun it schedules"
    );
}

struct EmptyPeekRunHandle {
    peeks: AtomicUsize,
    hydrations: AtomicUsize,
    peeked: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for EmptyPeekRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        self.peeks.fetch_add(1, Ordering::SeqCst);
        self.peeked.notify_one();
        Ok(Some(false))
    }

    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn empty_claimable_peek_skips_hydration() {
    let handle = Arc::new(EmptyPeekRunHandle {
        peeks: AtomicUsize::new(0),
        hydrations: AtomicUsize::new(0),
        peeked: tokio::sync::Notify::new(),
    });
    let driver = NativeQueuedWork::new(handle.clone());
    driver.notify_pending_work(Some("session-empty"), "queued_turn_input");
    handle.peeked.notified().await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(handle.peeks.load(Ordering::SeqCst), 1);
    assert_eq!(handle.hydrations.load(Ordering::SeqCst), 0);
}

struct CreateOnlyFactory {
    inner: crate::InMemorySessionStoreFactory,
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for CreateOnlyFactory {
    async fn live_attachment_refs(
        &self,
        cutoff: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        crate::AttachmentRootSet::live_attachment_refs(&self.inner, cutoff).await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        cutoff: u64,
    ) -> Result<bool, crate::StoreError> {
        crate::AttachmentRootSet::has_live_attachment_ref(&self.inner, id, cutoff).await
    }
}

#[async_trait::async_trait]
impl crate::SessionStoreFactory for CreateOnlyFactory {
    async fn create_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        self.inner.create_store(request).await
    }

    async fn session_was_deleted(&self, session_id: &str) -> Result<bool, String> {
        crate::SessionStoreFactory::session_was_deleted(&self.inner, session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &str,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }
}

#[tokio::test]
async fn create_only_factory_treats_claimability_as_unknown_and_runs() {
    let factory = CreateOnlyFactory {
        inner: crate::InMemorySessionStoreFactory::new(),
    };
    let request = crate::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: "create-only-factory".to_string(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    };

    assert_eq!(
        crate::SessionStoreFactory::has_claimable_queued_work(&factory, &request, 0)
            .await
            .expect("the conservative default succeeds"),
        None,
        "a factory that cannot inspect an existing store must preserve unknown claimability"
    );
}

struct PublicProbeRunHandle {
    peeks: AtomicUsize,
    hydrations: AtomicUsize,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for PublicProbeRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        self.peeks.fetch_add(1, Ordering::SeqCst);
        Ok(Some(true))
    }

    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_single_pass_handle_never_eagerly_rehydrates_a_positive_peek() {
    let handle = Arc::new(PublicProbeRunHandle {
        peeks: AtomicUsize::new(0),
        hydrations: AtomicUsize::new(0),
    });
    let driver = NativeQueuedWork::new(handle.clone());

    driver.notify_pending_work(Some("session-public-probe"), "queued_turn_input");
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(handle.hydrations.load(Ordering::SeqCst), 1);
    assert_eq!(handle.peeks.load(Ordering::SeqCst), 1);
}

struct ContendedRunHandle {
    peeks: AtomicUsize,
    hydrations: AtomicUsize,
    blocked: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for ContendedRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        self.peeks.fetch_add(1, Ordering::SeqCst);
        Ok(Some(true))
    }

    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn claim_and_run_pending_with_progress(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
        self.claim_and_run_pending(session_id, reason).await?;
        self.blocked.notify_one();
        Ok(QueuedWorkRunProgress::Blocked)
    }
}

#[tokio::test]
async fn one_notification_during_live_lease_contention_has_bounded_hydrations() {
    let handle = Arc::new(ContendedRunHandle {
        peeks: AtomicUsize::new(0),
        hydrations: AtomicUsize::new(0),
        blocked: tokio::sync::Notify::new(),
    });
    let driver =
        NativeQueuedWork::with_execution_concurrency(handle.clone(), 1).expect("valid concurrency");

    driver.notify_pending_work(Some("session-contended"), "queued_turn_input");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let hydrations = handle.hydrations.load(Ordering::SeqCst);
    assert!(
        (3..=5).contains(&hydrations),
        "one contended notification must back off, got {hydrations} hydrations"
    );
    assert_eq!(handle.peeks.load(Ordering::SeqCst), hydrations);
}

mod final_regressions;
mod slow_wake_telemetry;

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
    #[cfg(feature = "otel-trace")]
    let metrics = crate::operational_metrics::TestMetrics::install();
    let handle = Arc::new(FailOnceRunHandle {
        attempts: Arc::new(AtomicUsize::new(0)),
        accepted: tokio::sync::Notify::new(),
    });
    let accepted = handle.accepted.notified();
    let driver = NativeQueuedWork::new(handle.clone());

    driver.notify_pending_work(Some("session-1"), "queued_turn_input");

    tokio::time::timeout(Duration::from_secs(1), accepted)
        .await
        .expect("the failed wake must retry without another enqueue");
    assert_eq!(handle.attempts.load(Ordering::SeqCst), 2);
    #[cfg(feature = "otel-trace")]
    assert_eq!(metrics.counter_value("lash.queued_work.wake_retries"), 1);
}

struct AlwaysFailRunHandle {
    attempts: Arc<AtomicUsize>,
    class: QueuedWorkRunErrorClass,
}

#[test]
fn terminal_constructor_rejects_zero_retry_delay_directly() {
    let work_cadence = WorkCadencePolicy {
        retry_initial: Duration::ZERO,
        ..WorkCadencePolicy::default()
    };

    let Err(error) = NativeQueuedWork::from_parts_with_work_cadence(
        Arc::new(AlwaysFailRunHandle {
            attempts: Arc::new(AtomicUsize::new(0)),
            class: QueuedWorkRunErrorClass::Transient,
        }),
        CancellationToken::new(),
        None,
        work_cadence,
    ) else {
        panic!("terminal queued-work construction must reject zero-delay retries");
    };
    assert!(
        error.to_string().contains("work_cadence.retry_initial"),
        "error must identify the rejected retry field: {error}"
    );
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
    let driver = NativeQueuedWork::new(Arc::new(AlwaysFailRunHandle {
        attempts: Arc::clone(&attempts),
        class: QueuedWorkRunErrorClass::Terminal,
    }));

    driver.notify_pending_work(Some("session-terminal"), "queued_turn_input");
    tokio::time::timeout(Duration::from_secs(1), async {
        while attempts.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal wake attempted");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transient_wake_error_stops_at_the_attempt_limit() {
    let max_attempts = WorkCadencePolicy::default().max_transient_attempts.get() as usize;
    let attempts = Arc::new(AtomicUsize::new(0));
    let driver = NativeQueuedWork::new(Arc::new(AlwaysFailRunHandle {
        attempts: Arc::clone(&attempts),
        class: QueuedWorkRunErrorClass::Transient,
    }));

    driver.notify_pending_work(Some("session-exhausted"), "queued_turn_input");
    tokio::time::timeout(Duration::from_secs(5), async {
        while attempts.load(Ordering::SeqCst) < max_attempts {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("transient wake reaches the attempt limit");

    assert_eq!(attempts.load(Ordering::SeqCst), max_attempts);
}

#[tokio::test]
async fn work_cadence_policy_limits_transient_wake_attempts() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let work_cadence = WorkCadencePolicy {
        max_transient_attempts: std::num::NonZeroU32::MIN,
        ..WorkCadencePolicy::default()
    };
    let driver = NativeQueuedWork::from_parts_with_work_cadence(
        Arc::new(AlwaysFailRunHandle {
            attempts: Arc::clone(&attempts),
            class: QueuedWorkRunErrorClass::Transient,
        }),
        CancellationToken::new(),
        None,
        work_cadence,
    )
    .expect("configured work cadence is valid");

    driver.notify_pending_work(Some("session-configured-limit"), "queued_turn_input");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let dispatcher_running = driver.inner.scheduler.lock_state().dispatcher_running;
            if attempts.load(Ordering::SeqCst) > 0 && !dispatcher_running {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the configured retry budget terminates the wake");

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "max_transient_attempts=1 must stop after the first failure"
    );
}

struct BlockingRunHandle {
    entered: tokio::sync::Notify,
    dropped: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl QueuedWorkRunHandle for BlockingRunHandle {
    async fn run_queued_work(
        &self,
        _request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        self.entered.notify_one();
        struct DropProbe(Arc<AtomicBool>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _probe = DropProbe(Arc::clone(&self.dropped));
        std::future::pending().await
    }
}

#[tokio::test]
async fn dropping_the_driver_cancels_an_inflight_wake() {
    let dropped = Arc::new(AtomicBool::new(false));
    let handle = Arc::new(BlockingRunHandle {
        entered: tokio::sync::Notify::new(),
        dropped: Arc::clone(&dropped),
    });
    let entered = handle.entered.notified();
    let driver = NativeQueuedWork::new(handle.clone());
    driver.notify_pending_work(Some("session-shutdown"), "queued_turn_input");
    entered.await;

    drop(driver);

    tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("driver drop must cancel and drop the in-flight wake");
}
