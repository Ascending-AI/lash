use super::*;

fn injected_worklist_error(label: &str) -> PluginError {
    PluginError::Session(format!("injected worklist failure: {label}"))
}

fn default_fetch_attempts() -> usize {
    crate::WorkerSweepPolicy::default().fetch_attempts.get()
}

#[tokio::test]
async fn worker_sweep_policy_limits_worklist_fetch_attempts() {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let native_substrate = crate::NativeSubstrateConfig {
        worker_sweep: crate::WorkerSweepPolicy {
            fetch_attempts: std::num::NonZeroUsize::MIN,
            ..crate::WorkerSweepPolicy::default()
        },
        ..crate::NativeSubstrateConfig::default()
    };
    let (worker, _, _, _, test_registry) = worker_with_engine_registry_timings_supplier_and_sink(
        1,
        Arc::new(GatedSuccessEngine {
            started: Arc::new(AtomicUsize::new(0)),
            started_changed: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Semaphore::new(1)),
        }),
        run_handle,
        None,
        None,
        None,
        native_substrate,
    )
    .await;
    test_registry.set_worklist_page_errors(
        0,
        (0..default_fetch_attempts())
            .map(|_| injected_worklist_error("configured retry limit"))
            .collect(),
    );

    let result = worker
        .fetch_worklist_page_with_retry(
            std::num::NonZeroUsize::new(1).expect("literal is non-zero"),
            None,
        )
        .await;

    assert!(result.is_err(), "the one configured attempt fails");
    assert_eq!(
        test_registry.worklist_page_reads().len(),
        1,
        "fetch_attempts=1 must stop after the first failed read"
    );
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
    let run_handle = Arc::new(LateBoundProcessWork::default());
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
            "gated-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register blocked process-slot fixture");
    let _ = run_handle
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
            .dispatcher_running()
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
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (_worker, registry, run_handle, env_ref, test_registry, sink) =
        worker_with_engine_and_fault_sink(
            1,
            Arc::new(GatedSuccessEngine {
                started,
                started_changed,
                release,
            }),
            run_handle,
        )
        .await;
    for _index in 0..2 {
        registry
            .register_process(engine_registration(
                "gated-success",
                env_ref.clone(),
                serde_json::Value::Null,
            ))
            .await
            .expect("register continuation recovery process");
    }
    test_registry.set_worklist_page_errors(
        1,
        (0..default_fetch_attempts())
            .map(|_| injected_worklist_error("continuation"))
            .collect(),
    );

    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("initial worklist page succeeds");
    tokio::time::timeout(Duration::from_secs(2), async {
        while test_registry.worklist_page_reads().len() < 1 + default_fetch_attempts() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("continuation retry budget is exhausted");

    // The failed scan is reported by the pass that failed it, on the fault
    // surface — never folded into the next call's outcome.
    match sink.await_first_fault("the incomplete worklist scan").await {
        ProcessWorkerFault::WorklistScanIncomplete { error } => {
            assert!(error.contains("injected worklist failure"));
        }
        other => panic!("expected an incomplete-scan fault, got {other:?}"),
    }
    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("the next drive describes its own admission, not a prior scan");
    wait_for_terminal_count(&registry, 2, "resumed continuation sweep").await;
}

#[tokio::test]
async fn retry_exhaustion_does_not_strand_an_in_flight_retryable_execution() {
    let retry_started = Arc::new(tokio::sync::Notify::new());
    let fail_retry = Arc::new(tokio::sync::Notify::new());
    let retry_runs = Arc::new(AtomicUsize::new(0));
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (worker, registry, run_handle, env_ref, test_registry, sink) =
        worker_with_engine_and_fault_sink(
            2,
            Arc::new(FailFirstRetryEngine {
                retry_started: Arc::clone(&retry_started),
                fail_retry: Arc::clone(&fail_retry),
                retry_runs: Arc::clone(&retry_runs),
            }),
            run_handle,
        )
        .await;
    let mut ids = std::collections::BTreeMap::new();
    // Registered in worklist order: a minted id is time-ordered, so the
    // registration order is the id order the pages walk.
    for process_id in ["a-fast", "b-retry", "c-later-page"] {
        let registered = registry
            .register_process(engine_registration(
                "fail-first-retry",
                env_ref.clone(),
                serde_json::json!({ "name": process_id }),
            ))
            .await
            .expect("register retry-exhaustion fixture");
        ids.insert(process_id, registered.id.clone());
    }
    test_registry.set_worklist_page_errors(
        1,
        (0..default_fetch_attempts())
            .map(|_| injected_worklist_error("in-flight retry"))
            .collect(),
    );

    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("start retry-exhaustion drive");
    retry_started.notified().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while test_registry.worklist_page_reads().len() < 1 + default_fetch_attempts() {
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

    match sink.await_first_fault("the exhausted worklist scan").await {
        ProcessWorkerFault::WorklistScanIncomplete { error } => {
            assert!(error.contains("injected worklist failure"));
        }
        other => panic!("expected an incomplete-scan fault, got {other:?}"),
    }
    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("the next drive reports its own admission");
    wait_for_terminal_count(&registry, 3, "retry after continuation exhaustion").await;
    assert_eq!(retry_runs.load(Ordering::SeqCst), 2);
    // A terminal write lands in the durable registry before the execution that
    // wrote it leaves the scheduler, so the scheduler is read once it settles.
    tokio::time::timeout(Duration::from_secs(1), async {
        while worker
            .execution_scheduler
            .state
            .lock_recover()
            .running_count()
            != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("no execution stays stranded in the scheduler");
}

#[tokio::test]
async fn concurrent_drive_rescan_survives_the_initial_fetch_error() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let run_handle = Arc::new(LateBoundProcessWork::default());
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
            "gated-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register concurrent rescan process");
    test_registry.set_worklist_page_errors(0, vec![injected_worklist_error("initial")]);
    let pause = test_registry.pause_next_worklist_page();
    let first_drive = {
        let run_handle = Arc::clone(&run_handle);
        crate::task::spawn(async move { run_handle.enable_and_drive().await })
    };
    pause.wait_until_validated().await;
    let _ = run_handle
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

/// The error path consumes a pending rescan into the `Ready` restart it
/// schedules: the restarted dispatcher's fresh scan *is* that rescan, so the
/// flag must not survive to schedule a second, redundant pass.
#[tokio::test]
async fn a_failed_scan_consumes_the_pending_rescan_without_a_redundant_pass() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (_worker, registry, run_handle, env_ref, test_registry) =
        worker_with_engine_registry_timings_supplier_and_sink(
            1,
            Arc::new(GatedSuccessEngine {
                started,
                started_changed,
                release,
            }),
            run_handle,
            None,
            None,
            None,
            crate::NativeSubstrateConfig {
                worker_sweep: crate::WorkerSweepPolicy {
                    // Keep the idle-dispatcher sweep out of the read count.
                    rescan_interval: Duration::from_secs(3600),
                    ..crate::WorkerSweepPolicy::default()
                },
                ..crate::NativeSubstrateConfig::default()
            },
        )
        .await;
    registry
        .register_process(engine_registration(
            "gated-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register consumed-rescan process");
    test_registry.set_worklist_page_errors(0, vec![injected_worklist_error("initial")]);
    let pause = test_registry.pause_next_worklist_page();
    let first_drive = {
        let run_handle = Arc::clone(&run_handle);
        crate::task::spawn(async move { run_handle.enable_and_drive().await })
    };
    pause.wait_until_validated().await;
    let _ = run_handle
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
    wait_for_terminal_count(&registry, 1, "consumed rescan row").await;

    // The failed first read plus the rescan it owed: a third read means the
    // pending flag survived the error path and scheduled a redundant pass.
    let redundant = tokio::time::timeout(Duration::from_millis(500), async {
        while test_registry.worklist_page_reads().len() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let reads = test_registry.worklist_page_reads();
    assert!(
        redundant.is_err(),
        "the consumed rescan must not schedule a redundant pass: {reads:?}"
    );
    assert_eq!(reads.len(), 2, "one failed read plus the rescan it owed");
}

/// A dispatcher that unwinds mid-scan frees the latch and rewinds the scan to
/// the cursor it was reading; the next drive claims a replacement dispatcher
/// and the queued row still drains to terminal.
#[tokio::test]
async fn a_dispatcher_unwind_mid_scan_is_drained_by_the_replacement() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (worker, registry, run_handle, env_ref, _test_registry) = worker_with_engine_and_registry(
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
            "gated-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register unwind-drain process");

    // A dispatcher that died mid-fetch: latch held, scan fetching the head.
    {
        let mut state = worker.execution_scheduler.state.lock_recover();
        assert!(state.claim_dispatcher());
        state.extra = ProcessWorklistScan::Fetching {
            continuation: None,
            rescan: false,
        };
    }
    let task_scheduler = Arc::clone(&worker.execution_scheduler);
    let task = crate::task::spawn(async move {
        let _guard = ProcessExecutionDispatcherGuard::new(task_scheduler);
        panic!("test dispatcher unwind");
    });
    assert!(task.await.expect_err("dispatcher task panics").is_panic());
    assert!(
        !worker
            .execution_scheduler
            .state
            .lock_recover()
            .dispatcher_running(),
        "the unwind frees the dispatcher latch for a replacement"
    );

    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("the next drive claims a replacement dispatcher");
    wait_for_terminal_count(&registry, 1, "row drained after dispatcher unwind").await;
}

#[tokio::test]
async fn worklist_intake_fetches_next_page_only_after_dispatch_capacity_frees() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let run_handle = Arc::new(LateBoundProcessWork::default());
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
    for _index in 0..4 {
        registry
            .register_process(engine_registration(
                "gated-success",
                env_ref.clone(),
                serde_json::Value::Null,
            ))
            .await
            .expect("register bounded-intake process");
    }

    let _ = run_handle
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
    let reads = test_registry.worklist_page_reads();
    assert_eq!(reads.len(), 1, "a saturated worker must not fetch page two");
    assert_eq!(reads[0].0, 1, "the first page is bounded by free slots");

    release.add_permits(4);
    wait_for_terminal_count(&registry, 4, "bounded intake backlog").await;
    let reads = test_registry.worklist_page_reads();
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
        payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        if payload["name"] == "b-retry" && self.retry_runs.fetch_add(1, Ordering::SeqCst) == 0 {
            self.retry_started.notify_one();
            self.fail_retry.notified().await;
            return Err(crate::ProcessInfraError::new(PluginError::Session(
                "injected transient process execution failure".to_string(),
            )));
        }
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"process_id": context.process_id()}),
            ))
            .into(),
        )
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
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"process_id": context.process_id()}),
            ))
            .into(),
        )
    }
}

/// A call that finds a scan already in flight admits nothing of its own. Its
/// empty report used to be indistinguishable from "the worklist was empty";
/// the typed intake state is what separates the two.
#[tokio::test]
async fn a_drive_that_coalesces_onto_an_in_flight_scan_reports_no_intake() {
    let started = Arc::new(AtomicUsize::new(0));
    let started_changed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let run_handle = Arc::new(LateBoundProcessWork::default());
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
    let coalesced_intake_row = registry
        .register_process(engine_registration(
            "gated-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register the coalesced intake row")
        .id;

    let pause = test_registry.pause_next_worklist_page();
    let scanning_drive = {
        let run_handle = Arc::clone(&run_handle);
        crate::task::spawn(async move { run_handle.enable_and_drive().await })
    };
    pause.wait_until_validated().await;
    let coalesced = run_handle
        .enable_and_drive()
        .await
        .expect("the coalescing drive returns cleanly");
    assert_eq!(
        coalesced.intake,
        ProcessAdmissionIntake::Coalesced,
        "a call that never read the worklist must say so"
    );
    assert!(
        coalesced.admitted.is_empty() && coalesced.deferred.is_empty(),
        "a coalesced pass reports no rows of its own: {coalesced:?}"
    );

    pause.resume();
    let scanned = scanning_drive
        .await
        .expect("scanning drive task joins")
        .expect("the scanning drive returns its own admission");
    assert_eq!(
        scanned.intake,
        ProcessAdmissionIntake::Scanned,
        "the call that read the page owns the intake"
    );
    assert_eq!(scanned.admitted, vec![coalesced_intake_row]);
    wait_for_terminal_count(&registry, 1, "coalesced intake row").await;
}

/// The re-entrant leg of the same problem: a nested drive (trigger-delivery
/// reconcile calls the work driver) admits rows the outer scan then sees as
/// already scheduled. Folding the nested report in first is what keeps the
/// outer call from reporting its own admission as somebody else's `Busy`.
#[test]
fn an_absorbed_nested_report_is_never_re_reported_as_busy() {
    let mut outer = ProcessAdmissionReport {
        intake: ProcessAdmissionIntake::Coalesced,
        ..ProcessAdmissionReport::default()
    };
    outer.absorb(ProcessAdmissionReport {
        intake: ProcessAdmissionIntake::Scanned,
        admitted: vec![crate::ProcessId::fixture("nested-row")],
        deferred: Vec::new(),
    });
    assert_eq!(
        outer.intake,
        ProcessAdmissionIntake::Scanned,
        "the nested pass's intake belongs to this call"
    );

    // The outer scan sees the nested pass's row already scheduled and the
    // page's own untouched row.
    outer.absorb(ProcessAdmissionReport {
        intake: ProcessAdmissionIntake::Scanned,
        admitted: vec![crate::ProcessId::fixture("outer-row")],
        deferred: vec![
            ProcessAdmissionDeferred {
                process_id: crate::ProcessId::fixture("nested-row"),
                disposition: ProcessRecoveryAttemptOutcome::Busy,
            },
            ProcessAdmissionDeferred {
                process_id: crate::ProcessId::fixture("peer-row"),
                disposition: ProcessRecoveryAttemptOutcome::Busy,
            },
        ],
    });
    assert_eq!(
        outer.admitted,
        vec![
            crate::ProcessId::fixture("nested-row"),
            crate::ProcessId::fixture("outer-row")
        ],
        "both legs' admissions belong to the one call"
    );
    assert_eq!(
        outer.deferred,
        vec![ProcessAdmissionDeferred {
            process_id: crate::ProcessId::fixture("peer-row"),
            disposition: ProcessRecoveryAttemptOutcome::Busy,
        }],
        "another owner's contention survives; this call's own admission does not become it"
    );
}

/// FIG-2964: the native tier's failed start poke is only advisory because the
/// idle dispatcher rescans the worklist on the worker-sweep cadence.
///
/// The second row below is registered straight into the store with no poke at
/// all — the state a failed `drive_pending_processes` nudge leaves behind. It
/// must still *run*, not merely stay non-terminal: without the rescan the row
/// sits pending until some unrelated poke happens by, which on an idle host is
/// never. The first row is what gets the dispatcher running in the first place,
/// so the pickup under test is the rescan and nothing else.
#[tokio::test]
async fn an_idle_dispatcher_rescans_and_runs_a_row_no_poke_announced() {
    let native_substrate = crate::NativeSubstrateConfig {
        worker_sweep: crate::WorkerSweepPolicy {
            rescan_interval: Duration::from_millis(5),
            ..crate::WorkerSweepPolicy::default()
        },
        ..crate::NativeSubstrateConfig::default()
    };
    let started = Arc::new(AtomicUsize::new(0));
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (worker, registry, _run_handle, env_ref, test_registry) =
        worker_with_engine_registry_timings_supplier_and_sink(
            2,
            Arc::new(GatedSuccessEngine {
                started: Arc::clone(&started),
                started_changed: Arc::new(tokio::sync::Notify::new()),
                release: Arc::new(tokio::sync::Semaphore::new(16)),
            }),
            run_handle,
            None,
            None,
            None,
            native_substrate,
        )
        .await;

    let poked_row = test_registry
        .register_process(engine_registration(
            "gated-success",
            env_ref.clone(),
            serde_json::json!({}),
        ))
        .await
        .expect("register the row that starts the dispatcher")
        .id;
    let report = worker
        .drive_pending_processes()
        .await
        .expect("the poked row is admitted");
    assert_eq!(report.admitted, vec![poked_row]);
    wait_for_terminal_count(&registry, 1, "the poked row to run").await;

    // Nothing announces this row: no poke, no drive call, no notification.
    test_registry
        .register_process(engine_registration(
            "gated-success",
            env_ref,
            serde_json::json!({}),
        ))
        .await
        .expect("register the row whose poke failed");

    wait_for_terminal_count(&registry, 2, "the rescan to find the unannounced row").await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        2,
        "the rescan must actually run the row, not merely leave it non-terminal"
    );
}
