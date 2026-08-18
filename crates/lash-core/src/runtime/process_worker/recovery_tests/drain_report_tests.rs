//! Drain-report fault surface: how `drain_owner_bound_work` reports rows it
//! could not terminalize, split out of `recovery_tests.rs` for file size.

use super::*;

#[tokio::test]
async fn drain_reports_claim_backend_error_and_retries() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-claim-failure", "host-a", "start-a");
    let process_id = "owner-bound-claim-failure";
    registry
        .register_process(registration_with_disposition(
            process_id,
            RecoveryDisposition::OwnerBound,
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
        .set_process_lease_claim_error(Some(PluginError::Session(
            "injected claim failure".to_string(),
        )))
        .await;

    let worker = inline_worker(registry.clone(), owner);
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");
    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptDisposition::BackendError {
                operation: ProcessRecoveryOperation::ClaimLease,
                error: "plugin session error: injected claim failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        process_id,
        "claim_lease",
        "plugin session error: injected claim failure",
    );

    registry.set_process_lease_claim_error(None).await;
    let retry = worker
        .drain_owner_bound_work()
        .await
        .expect("retry owner drain");
    assert_eq!(retry.abandoned, vec![process_id.to_string()]);
    assert!(retry.deferred.is_empty());
}

#[tokio::test]
async fn drain_reports_lease_renewal_backend_error_and_retries() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-renew-failure", "host-a", "start-a");
    let process_id = "owner-bound-renew-failure";
    registry
        .register_process(registration_with_disposition(
            process_id,
            RecoveryDisposition::OwnerBound,
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
        .set_process_lease_renew_error(Some(PluginError::Session(
            "injected lease-renewal failure".to_string(),
        )))
        .await;

    let worker = inline_worker(registry.clone(), owner);
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");
    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptDisposition::BackendError {
                operation: ProcessRecoveryOperation::RenewLease,
                error: "plugin session error: injected lease-renewal failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        process_id,
        "renew_lease",
        "plugin session error: injected lease-renewal failure",
    );

    registry.set_process_lease_renew_error(None).await;
    let retry = worker
        .drain_owner_bound_work()
        .await
        .expect("retry owner drain");
    assert_eq!(retry.abandoned, vec![process_id.to_string()]);
    assert!(retry.deferred.is_empty());
}

#[tokio::test]
async fn drain_reports_registry_read_error_instead_of_absent() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-read-failure", "host-a", "start-a");
    let process_id = "owner-bound-read-failure";
    registry
        .register_process(registration_with_disposition(
            process_id,
            RecoveryDisposition::OwnerBound,
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
        .set_process_read_error(Some(PluginError::Session(
            "injected registry read failure".to_string(),
        )))
        .await;

    let worker = inline_worker(registry.clone(), owner);
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");
    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.to_string(),
            disposition: ProcessRecoveryAttemptDisposition::BackendError {
                operation: ProcessRecoveryOperation::ReadProcess,
                error: "plugin session error: injected registry read failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        process_id,
        "read_process",
        "plugin session error: injected registry read failure",
    );

    registry.set_process_read_error(None).await;
    let retry = worker
        .drain_owner_bound_work()
        .await
        .expect("retry owner drain");
    assert_eq!(retry.abandoned, vec![process_id.to_string()]);
    assert!(retry.deferred.is_empty());
}

#[tokio::test]
async fn drain_distinguishes_busy_and_absent_rows() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-legitimate-deferrals", "host-a", "start-a");
    for process_id in ["owner-bound-busy", "owner-bound-absent"] {
        registry
            .register_process(registration_with_disposition(
                process_id,
                RecoveryDisposition::OwnerBound,
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
    }
    registry
        .claim_process_lease(
            "owner-bound-busy",
            &LeaseOwnerIdentity::opaque("live-peer", "live-peer-incarnation"),
            60_000,
        )
        .await
        .expect("claim live peer lease")
        .acquired()
        .expect("peer acquires lease");
    let worker = inline_worker(registry.clone(), owner);

    let busy = worker.drain_owner_bound_work().await.expect("busy drain");
    assert_eq!(
        busy.deferred,
        vec![ProcessDrainDeferred {
            process_id: "owner-bound-busy".to_string(),
            disposition: ProcessRecoveryAttemptDisposition::Busy,
        }]
    );
    assert_eq!(busy.abandoned, vec!["owner-bound-absent".to_string()]);

    let absent_id = "owner-bound-read-as-absent";
    registry
        .register_process(registration_with_disposition(
            absent_id,
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register read-as-absent row");
    registry
        .record_first_started(
            absent_id,
            ProcessStarted {
                owner: worker.config().lease_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record read-as-absent start");
    registry.set_process_read_absent(true).await;
    let absent = worker.drain_owner_bound_work().await.expect("absent drain");
    assert_eq!(
        absent.deferred,
        vec![
            ProcessDrainDeferred {
                process_id: "owner-bound-busy".to_string(),
                disposition: ProcessRecoveryAttemptDisposition::Busy,
            },
            ProcessDrainDeferred {
                process_id: absent_id.to_string(),
                disposition: ProcessRecoveryAttemptDisposition::Absent,
            },
        ]
    );
}
