use super::*;

fn assert_recovery_lease_lost_event(
    capture: &EventCapture,
    process_id: &str,
    operation: &str,
    error: &str,
) {
    let event = capture.exactly_one("process_recovery.lease_lost");
    assert_eq!(event.level, "WARN");
    assert_eq!(event.target, "lash_core::process_recovery");
    let expected = [
        (
            "event",
            "process_recovery.lease_lost",
            CapturedFieldKind::Str,
        ),
        ("decision_basis", "lease_superseded", CapturedFieldKind::Str),
        ("process_id", process_id, CapturedFieldKind::Str),
        ("operation", operation, CapturedFieldKind::Str),
        ("outcome", "deferred_to_new_owner", CapturedFieldKind::Str),
        ("error", error, CapturedFieldKind::Str),
        (
            "message",
            "process recovery lease was superseded; deferring to the new owner",
            CapturedFieldKind::Debug,
        ),
    ];
    assert_eq!(event.field_count(), expected.len());
    for (field, value, kind) in expected {
        assert_eq!(event.field_kind(field), kind, "field kind for {field}");
        assert_eq!(event.field(field), value, "field value for {field}");
    }
}

fn peer_completed_record(mut record: ProcessRecord) -> ProcessRecord {
    record.status = ProcessStatus::Completed;
    record.outcome = Some(ProcessAwaitOutput::from_tool_output(
        crate::ToolCallOutput::success(serde_json::json!({"peer": "won"})),
    ));
    record
}

async fn seed_started_owner_bound(
    registry: &Arc<TestLocalProcessRegistry>,
    process_id: &str,
    owner: &LeaseOwnerIdentity,
) -> ProcessRecord {
    registry
        .register_process(registration_with_disposition(
            process_id,
            RecoveryContract::OwnerBound,
        ))
        .await
        .expect("register owner-bound row");
    registry
        .record_first_started(
            process_id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record first start");
    registry
        .get_process(process_id)
        .await
        .expect("read seeded process")
        .expect("seeded process exists")
}

#[tokio::test]
async fn drain_reports_superseded_terminal_as_peer_settled() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-superseded", "host-a", "start-a");
    let process_id = "owner-bound-superseded";
    let peer = peer_completed_record(seed_started_owner_bound(&registry, process_id, &owner).await);
    registry
        .set_process_terminal_write_outcome(crate::ProcessCompletionOutcome::Superseded {
            stored: peer,
        })
        .await;

    let report = inline_worker(registry.clone(), owner)
        .drain_owner_bound_work()
        .await
        .expect("owner drain");

    assert!(
        report.abandoned.is_empty(),
        "a superseded write is not this drain's acknowledged abandonment"
    );
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptOutcome::SettledByPeer {
                terminal_status: ProcessStatus::Completed,
            },
        }]
    );
    assert!(
        registry
            .get_process_lease(process_id)
            .await
            .expect("read lease")
            .is_none(),
        "peer-settled completion releases this pass's stale drain lease"
    );
}

#[tokio::test]
async fn drain_does_not_claim_an_already_applied_terminal_as_this_pass() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-already-applied", "host-a", "start-a");
    let process_id = "owner-bound-already-applied";
    let stored = seed_started_owner_bound(&registry, process_id, &owner).await;
    registry
        .set_process_terminal_write_outcome(crate::ProcessCompletionOutcome::AlreadyApplied {
            stored: ProcessRecord {
                status: ProcessStatus::Abandoned,
                ..stored
            },
        })
        .await;

    let report = inline_worker(registry.clone(), owner)
        .drain_owner_bound_work()
        .await
        .expect("owner drain");

    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptOutcome::AlreadyApplied {
                terminal_status: ProcessStatus::Abandoned,
            },
        }]
    );
    assert!(
        registry
            .get_process_lease(process_id)
            .await
            .expect("read lease")
            .is_none(),
        "already-applied completion releases this pass's stale drain lease"
    );
}

#[tokio::test]
async fn drain_reports_already_terminal_completed_as_peer_settled() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-already-terminal", "host-a", "start-a");
    let process_id = "owner-bound-already-terminal";
    let peer = peer_completed_record(seed_started_owner_bound(&registry, process_id, &owner).await);
    registry.set_process_read_override(peer).await;

    let report = inline_worker(registry, owner)
        .drain_owner_bound_work()
        .await
        .expect("owner drain");

    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptOutcome::SettledByPeer {
                terminal_status: ProcessStatus::Completed,
            },
        }]
    );
}

#[tokio::test]
async fn drain_reports_renewal_supersession_as_lease_lost() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-renew-superseded", "host-a", "start-a");
    let process_id = "owner-bound-renew-superseded";
    seed_started_owner_bound(&registry, process_id, &owner).await;
    registry
        .set_process_lease_renew_error(Some(PluginError::ProcessLeaseSuperseded {
            process_id: process_id.to_string(),
        }))
        .await;

    let worker = inline_worker(registry, owner);
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");

    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptOutcome::LeaseLost {
                operation: ProcessRecoveryOperation::RenewLease,
            },
        }]
    );
    assert_recovery_lease_lost_event(
        &capture,
        process_id,
        "renew_lease",
        "process lease for `owner-bound-renew-superseded` is missing or expired (superseded)",
    );
    assert!(capture.named("process_recovery.backend_error").is_empty());
}

#[tokio::test]
async fn drain_reports_terminal_write_supersession_as_lease_lost() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-write-superseded", "host-a", "start-a");
    let process_id = "owner-bound-write-superseded";
    seed_started_owner_bound(&registry, process_id, &owner).await;
    registry
        .set_process_terminal_write_error(Some(PluginError::ProcessLeaseSuperseded {
            process_id: process_id.to_string(),
        }))
        .await;

    let worker = inline_worker(registry, owner);
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");

    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptOutcome::LeaseLost {
                operation: ProcessRecoveryOperation::WriteTerminal,
            },
        }]
    );
    assert_recovery_lease_lost_event(
        &capture,
        process_id,
        "write_terminal",
        "process lease for `owner-bound-write-superseded` is missing or expired (superseded)",
    );
    assert!(capture.named("process_recovery.backend_error").is_empty());
}

#[tokio::test]
async fn drain_release_failure_overrides_absent_disposition() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-release-failure", "host-a", "start-a");
    let process_id = "owner-bound-release-failure";
    seed_started_owner_bound(&registry, process_id, &owner).await;
    registry.set_process_read_absent(true).await;
    registry
        .set_process_lease_release_error(Some(PluginError::Session(
            "injected lease-release failure".to_string(),
        )))
        .await;

    let worker = inline_worker(registry, owner);
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");

    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                operation: ProcessRecoveryOperation::ReleaseLease,
                error: "plugin session error: injected lease-release failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        process_id,
        "release_lease",
        "plugin session error: injected lease-release failure",
    );
}

#[tokio::test]
async fn recovered_nested_registry_read_uses_backend_error_telemetry() {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let fail = Arc::new(tokio::sync::Notify::new());
    let (worker, registry, _, env_ref, test_registry) = worker_with_engine_and_registry(
        1,
        Arc::new(PausedInfraEngine { started, fail }),
        run_handle,
    )
    .await;
    let process_id = "nested-recovery-read-failure";
    registry
        .register_process(engine_registration(
            process_id,
            "paused-infra",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register process");
    let record = registry
        .get_process(process_id)
        .await
        .expect("read process")
        .expect("process exists");
    test_registry
        .set_process_read_error_after(
            1,
            PluginError::Session("injected nested registry read failure".to_string()),
        )
        .await;

    let (_outcome, capture) = capturing(|| worker.recover_process(record)).await;

    assert_recovery_backend_error_event(
        &capture,
        process_id,
        "read_process",
        "plugin session error: injected nested registry read failure",
    );
}

#[tokio::test]
async fn recovered_live_renewal_uses_backend_error_telemetry() {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let fail = Arc::new(tokio::sync::Notify::new());
    let timings = crate::LeaseTimings::new(Duration::from_millis(30), Duration::from_millis(10))
        .expect("valid short lease timings");
    let (worker, registry, _, env_ref, test_registry) = worker_with_engine_registry_and_timings(
        1,
        Arc::new(PausedInfraEngine {
            started: Arc::clone(&started),
            fail,
        }),
        run_handle,
        Some(timings),
    )
    .await;
    let process_id = "live-recovery-renewal-failure";
    registry
        .register_process(engine_registration(
            process_id,
            "paused-infra",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register process");
    let record = registry
        .get_process(process_id)
        .await
        .expect("read process")
        .expect("process exists");
    test_registry
        .set_process_lease_renew_error(Some(PluginError::Session(
            "injected live lease-renewal failure".to_string(),
        )))
        .await;

    let (_outcome, capture) = capturing(|| worker.recover_process(record)).await;

    assert_recovery_backend_error_event(
        &capture,
        process_id,
        "renew_lease",
        "plugin session error: injected live lease-renewal failure",
    );
    assert!(
        registry
            .get_process_lease(process_id)
            .await
            .expect("read lease")
            .is_none(),
        "token-fenced release makes the backend-error row immediately claimable"
    );
}
