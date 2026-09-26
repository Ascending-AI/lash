//! A drive of pending processes admits rows and returns; these tests pin the
//! consequence: a backend fault on an admitted row can never present as a
//! completed clean drive. Each injects one registry fault at a different
//! operation (claim, read, terminal write, lease release) and asserts the typed
//! fault arrives on the unconditional [`ProcessEventSink`] surface while the row
//! is honestly reported as *admitted*, never as driven terminal.

use super::*;

struct ImmediateSuccessEngine;

#[async_trait::async_trait]
impl crate::ProcessEngine for ImmediateSuccessEngine {
    fn kind(&self) -> &'static str {
        "immediate-success"
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"process_id": context.process_id()}),
            ))
            .into(),
        )
    }
}

async fn drive_one_faulted_row(
    inject: impl AsyncFnOnce(&Arc<crate::testing::ProcessRegistryFaults>),
) -> (ProcessAdmissionReport, ProcessWorkerFault, usize, ProcessId) {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (_worker, registry, run_handle, env_ref, test_registry, sink) =
        worker_with_engine_and_fault_sink(1, Arc::new(ImmediateSuccessEngine), run_handle).await;
    let process_id = registry
        .register_process(engine_registration(
            "immediate-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register faulted-drive fixture")
        .id;
    inject(&test_registry).await;

    let report = run_handle
        .enable_and_drive()
        .await
        .expect("admission itself succeeds; the fault is on the admitted row");
    let fault = sink
        .await_first_fault("the admitted row's backend fault")
        .await;
    (report, fault, terminal_count(&registry).await, process_id)
}

fn assert_backend_fault(
    fault: &ProcessWorkerFault,
    expected_process_id: &ProcessId,
    expected_operation: ProcessRecoveryOperation,
) {
    match fault {
        ProcessWorkerFault::RecoveryBackendError {
            process_id,
            operation,
            error,
        } => {
            assert_eq!(process_id, expected_process_id);
            assert_eq!(*operation, expected_operation);
            assert!(
                error.contains("injected"),
                "the typed fault carries the backend error: {error}"
            );
        }
        other => panic!("expected a recovery backend fault, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failed_claim_on_an_admitted_row_reaches_the_fault_surface() {
    let (report, fault, terminal, process_id) = drive_one_faulted_row(async |registry| {
        registry.set_process_lease_claim_error(Some(PluginError::Session(
            "injected claim failure".to_string(),
        )));
    })
    .await;

    assert_eq!(report.admitted, vec![process_id.clone()]);
    assert!(report.deferred.is_empty());
    assert_backend_fault(&fault, &process_id, ProcessRecoveryOperation::ClaimLease);
    assert_eq!(terminal, 0, "a failed claim never terminalizes the row");
}

#[tokio::test]
async fn a_failed_read_on_an_admitted_row_reaches_the_fault_surface() {
    let (report, fault, terminal, process_id) = drive_one_faulted_row(async |registry| {
        registry.set_process_read_error(Some(PluginError::Session(
            "injected read failure".to_string(),
        )));
    })
    .await;

    assert_eq!(report.admitted, vec![process_id.clone()]);
    assert_backend_fault(&fault, &process_id, ProcessRecoveryOperation::ReadProcess);
    assert_eq!(terminal, 0, "a failed read never terminalizes the row");
}

#[tokio::test]
async fn non_session_cancel_watcher_read_failure_preserves_prior_terminalization() {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (_worker, registry, run_handle, env_ref, test_registry) =
        worker_with_engine_and_registry(1, Arc::new(ImmediateSuccessEngine), run_handle).await;
    let _process_id = "engine-cancel-watch-read-failure";
    let immediate_success_record = registry
        .register_process(engine_registration(
            "immediate-success",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register non-session fixture");
    let process_id = immediate_success_record.id.clone();
    test_registry.set_process_events_read_error(PluginError::Session(
        "injected non-session cancel watcher read failure".to_string(),
    ));

    let report = run_handle
        .enable_and_drive()
        .await
        .expect("admit non-session fixture");
    assert_eq!(report.admitted, vec![process_id.to_string()]);
    await_terminal(&registry, &process_id).await;
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read non-session fixture")
        .expect("non-session fixture remains retained");
    assert_eq!(
        record.status,
        ProcessStatus::Completed,
        "a cancel-watcher read failure must not widen SessionTurn retry semantics to engine processes"
    );
}

#[tokio::test]
async fn a_failed_terminal_write_on_an_admitted_row_reaches_the_fault_surface() {
    let (report, fault, terminal, process_id) = drive_one_faulted_row(async |registry| {
        registry.set_process_terminal_write_error(Some(PluginError::Session(
            "injected terminal write failure".to_string(),
        )));
    })
    .await;

    assert_eq!(report.admitted, vec![process_id.clone()]);
    assert_backend_fault(&fault, &process_id, ProcessRecoveryOperation::WriteTerminal);
    assert_eq!(
        terminal, 0,
        "a failed terminal write leaves the row non-terminal"
    );
}

#[tokio::test]
async fn a_failed_lease_release_on_an_admitted_row_reaches_the_fault_surface() {
    // The row disappears after admission, so the drive's only remaining act is
    // releasing its claim — and that release fails.
    let (report, fault, _terminal, process_id) = drive_one_faulted_row(async |registry| {
        registry.set_process_read_absent(true);
        registry.set_process_lease_release_error(Some(PluginError::Session(
            "injected release failure".to_string(),
        )));
    })
    .await;

    assert_eq!(report.admitted, vec![process_id.clone()]);
    assert_backend_fault(&fault, &process_id, ProcessRecoveryOperation::ReleaseLease);
}

#[tokio::test]
async fn a_row_this_worker_is_already_running_is_deferred_busy_not_admitted_twice() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    struct HeldEngine {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Semaphore>,
    }
    #[async_trait::async_trait]
    impl crate::ProcessEngine for HeldEngine {
        fn kind(&self) -> &'static str {
            "held"
        }

        async fn run(
            &self,
            context: crate::ProcessEngineRunContext<'_>,
            _payload: serde_json::Value,
        ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
            self.started.notify_one();
            let _permit = self.release.acquire().await.expect("release permit");
            Ok(
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!({"process_id": context.process_id()}),
                ))
                .into(),
            )
        }
    }

    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (_worker, registry, run_handle, env_ref, _test_registry, sink) =
        worker_with_engine_and_fault_sink(
            1,
            Arc::new(HeldEngine {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            }),
            run_handle,
        )
        .await;
    let held_row_record = registry
        .register_process(engine_registration(
            "held",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register held row");

    let first = run_handle
        .enable_and_drive()
        .await
        .expect("first admission");
    assert_eq!(first.admitted, vec![held_row_record.id.clone()]);
    started.notified().await;

    let second = run_handle.enable_and_drive().await.expect("second pass");
    assert!(
        second.admitted.is_empty(),
        "the running row is not admitted a second time"
    );
    assert_eq!(
        second.deferred,
        vec![ProcessAdmissionDeferred {
            process_id: held_row_record.id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::Busy,
        }],
        "busy stays distinct from absent in the admission report"
    );

    release.add_permits(2);
    wait_for_terminal_count(&registry, 1, "held row after release").await;
    assert!(
        sink.faults().is_empty(),
        "an ordinary coalesced pass is not a fault"
    );
}

/// The floor for a host that wired no sink: the typed fault still reaches the
/// `tracing` seam every host has. Before faults were typed, `WorklistScanIncomplete`
/// surfaced as an `Err` to the next caller — a sinkless host must not come out of
/// this change blinder than it went in.
#[tokio::test]
async fn a_sinkless_worker_reports_faults_on_the_tracing_seam() {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (worker, _registry, _run_handle, _env_ref, _test_registry) =
        worker_with_engine_and_registry(1, Arc::new(ImmediateSuccessEngine), run_handle).await;
    assert!(
        worker.config.process_event_sink.is_none(),
        "this fixture must model the sinkless host"
    );

    let ((), capture) = capturing(|| {
        worker.emit_worker_fault(ProcessWorkerFault::RecoveryRunFailed {
            process_id: crate::ProcessId::fixture("sinkless-row"),
            error: "engine rebuild failed".to_string(),
        })
    })
    .await;
    let logged = capture.exactly_one("process_worker.fault");
    assert_eq!(logged.level, "ERROR");
    assert_eq!(logged.field("fault"), "recovery_run_failed");
    assert_eq!(
        logged.field("process_id"),
        crate::ProcessId::fixture("sinkless-row").as_str()
    );
    assert_eq!(logged.field("error"), "engine rebuild failed");
}
