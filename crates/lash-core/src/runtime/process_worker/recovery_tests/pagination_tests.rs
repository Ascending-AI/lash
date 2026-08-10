use super::*;

fn injected_worklist_error(label: &str) -> PluginError {
    PluginError::Session(format!("injected worklist failure: {label}"))
}

struct ReserveFutureDrop(Arc<AtomicBool>);

impl Drop for ReserveFutureDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct BlockingSlotSupplier {
    reserve_started: Arc<tokio::sync::Notify>,
    reserve_dropped: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl crate::WorkerSlotSupplier for BlockingSlotSupplier {
    async fn reserve_slot(&self, _kind: crate::WorkerSlotKind) -> crate::WorkerSlotPermit {
        let _drop = ReserveFutureDrop(Arc::clone(&self.reserve_dropped));
        self.reserve_started.notify_one();
        std::future::pending().await
    }

    fn try_reserve_slot(&self, _kind: crate::WorkerSlotKind) -> Option<crate::WorkerSlotPermit> {
        None
    }

    fn available_slots(&self, _kind: crate::WorkerSlotKind) -> usize {
        1
    }
}

#[tokio::test]
async fn process_slot_reservation_is_cancelled_when_the_dispatcher_shuts_down() {
    let reserve_started = Arc::new(tokio::sync::Notify::new());
    let reserve_dropped = Arc::new(AtomicBool::new(false));
    let supplier = Arc::new(BlockingSlotSupplier {
        reserve_started: Arc::clone(&reserve_started),
        reserve_dropped: Arc::clone(&reserve_dropped),
    });
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (worker, registry, run_handle, env_ref, _) =
        worker_with_engine_registry_timings_and_supplier(
            1,
            Arc::new(GatedSuccessEngine {
                started: Arc::new(AtomicUsize::new(0)),
                started_changed: Arc::new(tokio::sync::Notify::new()),
                release: Arc::new(tokio::sync::Semaphore::new(1)),
            }),
            run_handle,
            None,
            Some(supplier),
        )
        .await;
    registry
        .register_process(engine_registration(
            "cancel-blocked-process-slot",
            "gated-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register blocked process-slot fixture");
    run_handle
        .enable_and_drive()
        .await
        .expect("start process dispatcher");
    reserve_started.notified().await;

    worker.execution_scheduler.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), async {
        while worker
            .execution_scheduler
            .state
            .lock_recover()
            .dispatcher_running
            || !reserve_dropped.load(Ordering::SeqCst)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown drops the supplier reservation and clears the latch");
}

#[tokio::test]
async fn continuation_fetch_failure_is_typed_and_the_next_drive_resumes_the_sweep() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(2));
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (_worker, registry, run_handle, env_ref, test_registry) = worker_with_engine_and_registry(
        1,
        Arc::new(GatedSuccessEngine {
            started,
            started_changed,
            release,
        }),
        run_handle,
    )
    .await;
    for index in 0..2 {
        registry
            .register_process(engine_registration(
                format!("continuation-recovery-{index}"),
                "gated-success",
                env_ref.clone(),
                serde_json::Value::Null,
            ))
            .await
            .expect("register continuation recovery process");
    }
    test_registry
        .set_worklist_page_errors_for_testing(
            1,
            (0..WORKLIST_FETCH_ATTEMPTS)
                .map(|_| injected_worklist_error("continuation"))
                .collect(),
        )
        .await;

    run_handle
        .enable_and_drive()
        .await
        .expect("initial worklist page succeeds");
    tokio::time::timeout(Duration::from_secs(2), async {
        while test_registry.worklist_page_reads_for_testing().await.len()
            < 1 + WORKLIST_FETCH_ATTEMPTS
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("continuation retry budget is exhausted");

    let error = run_handle
        .enable_and_drive()
        .await
        .expect_err("the next drive observes the typed incomplete scan");
    assert!(error.to_string().contains("injected worklist failure"));
    wait_for_terminal_count(&registry, 2, "resumed continuation sweep").await;
}

#[tokio::test]
async fn retry_exhaustion_does_not_strand_an_in_flight_retryable_execution() {
    let retry_started = Arc::new(tokio::sync::Notify::new());
    let fail_retry = Arc::new(tokio::sync::Notify::new());
    let retry_runs = Arc::new(AtomicUsize::new(0));
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (worker, registry, run_handle, env_ref, test_registry) = worker_with_engine_and_registry(
        2,
        Arc::new(FailFirstRetryEngine {
            retry_started: Arc::clone(&retry_started),
            fail_retry: Arc::clone(&fail_retry),
            retry_runs: Arc::clone(&retry_runs),
        }),
        run_handle,
    )
    .await;
    for process_id in ["a-fast", "b-retry", "c-later-page"] {
        registry
            .register_process(engine_registration(
                process_id,
                "fail-first-retry",
                env_ref.clone(),
                serde_json::Value::Null,
            ))
            .await
            .expect("register retry-exhaustion fixture");
    }
    test_registry
        .set_worklist_page_errors_for_testing(
            1,
            (0..WORKLIST_FETCH_ATTEMPTS)
                .map(|_| injected_worklist_error("in-flight retry"))
                .collect(),
        )
        .await;

    run_handle
        .enable_and_drive()
        .await
        .expect("start retry-exhaustion drive");
    retry_started.notified().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while test_registry.worklist_page_reads_for_testing().await.len()
            < 1 + WORKLIST_FETCH_ATTEMPTS
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("continuation retry budget is exhausted while retryable work is active");

    fail_retry.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while worker
            .execution_scheduler
            .slots
            .available_slots(crate::WorkerSlotKind::Process)
            < 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the transiently failed execution releases its worker slot");

    run_handle
        .enable_and_drive()
        .await
        .expect_err("the next drive observes the exhausted scan");
    wait_for_terminal_count(&registry, 3, "retry after continuation exhaustion").await;
    assert_eq!(retry_runs.load(Ordering::SeqCst), 2);
    assert_eq!(worker.execution_scheduler.state.lock_recover().active, 0);
}

#[tokio::test]
async fn concurrent_drive_rescan_survives_the_initial_fetch_error() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (_worker, registry, run_handle, env_ref, test_registry) = worker_with_engine_and_registry(
        1,
        Arc::new(GatedSuccessEngine {
            started,
            started_changed,
            release,
        }),
        run_handle,
    )
    .await;
    registry
        .register_process(engine_registration(
            "concurrent-rescan-survivor",
            "gated-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register concurrent rescan process");
    test_registry
        .set_worklist_page_errors_for_testing(0, vec![injected_worklist_error("initial")])
        .await;
    let pause = test_registry.pause_next_worklist_page_for_testing();
    let first_drive = {
        let run_handle = Arc::clone(&run_handle);
        crate::task::spawn(async move { run_handle.enable_and_drive().await })
    };
    pause.wait_until_validated().await;
    run_handle
        .enable_and_drive()
        .await
        .expect("concurrent drive records its rescan intent");
    pause.resume();
    assert!(
        first_drive
            .await
            .expect("initial drive task joins")
            .is_err(),
        "the initial caller still receives its typed fetch failure"
    );
    wait_for_terminal_count(&registry, 1, "concurrent rescan survivor").await;
}

#[tokio::test]
async fn worklist_intake_fetches_next_page_only_after_dispatch_capacity_frees() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (worker, registry, run_handle, env_ref, test_registry) = worker_with_engine_and_registry(
        1,
        Arc::new(GatedSuccessEngine {
            started: Arc::clone(&started),
            started_changed: Arc::clone(&started_changed),
            release: Arc::clone(&release),
        }),
        run_handle,
    )
    .await;
    for index in 0..4 {
        registry
            .register_process(engine_registration(
                format!("bounded-intake-{index}"),
                "gated-success",
                env_ref.clone(),
                serde_json::Value::Null,
            ))
            .await
            .expect("register bounded-intake process");
    }

    run_handle
        .enable_and_drive()
        .await
        .expect("start bounded worklist drive");
    tokio::time::timeout(Duration::from_secs(1), async {
        while started.load(Ordering::SeqCst) == 0 {
            started_changed.notified().await;
        }
    })
    .await
    .expect("first process starts");
    let reads = test_registry.worklist_page_reads_for_testing().await;
    assert_eq!(reads.len(), 1, "a saturated worker must not fetch page two");
    assert_eq!(reads[0].0, 1, "the first page is bounded by free slots");

    release.add_permits(4);
    wait_for_terminal_count(&registry, 4, "bounded intake backlog").await;
    let reads = test_registry.worklist_page_reads_for_testing().await;
    assert!(
        reads.len() >= 4,
        "one-slot dispatch must intake the four-row backlog incrementally"
    );
    drop(worker);
}

struct GatedSuccessEngine {
    started: Arc<AtomicUsize>,
    started_changed: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

struct FailFirstRetryEngine {
    retry_started: Arc<tokio::sync::Notify>,
    fail_retry: Arc<tokio::sync::Notify>,
    retry_runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::ProcessEngine for FailFirstRetryEngine {
    fn kind(&self) -> &'static str {
        "fail-first-retry"
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        if context.registration().id == "b-retry"
            && self.retry_runs.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.retry_started.notify_one();
            self.fail_retry.notified().await;
            return Err(crate::ProcessInfraError::new(PluginError::Session(
                "injected transient process execution failure".to_string(),
            )));
        }
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"process_id": context.registration().id}),
            control: None,
        }
        .into())
    }
}

#[async_trait::async_trait]
impl crate::ProcessEngine for GatedSuccessEngine {
    fn kind(&self) -> &'static str {
        "gated-success"
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.started_changed.notify_waiters();
        self.release
            .acquire()
            .await
            .expect("test release semaphore remains open")
            .forget();
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"process_id": context.registration().id}),
            control: None,
        }
        .into())
    }
}
