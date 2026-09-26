//! Drain-report fault surface: how `drain_owner_bound_work` reports rows it
//! could not terminalize, split out of `recovery_tests.rs` for file size.

use super::*;
use crate::ProcessLeases as _;

#[tokio::test]
async fn drain_reports_claim_backend_error_and_retries() {
    let (backend, registry) = faulted_memory_backend().await;
    let owner = local_owner("drain-claim-failure", "host-a", "start-a");
    let _process_id = "owner-bound-claim-failure";
    let owner_bound_claim_failure_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register owner-bound row");
    let process_id = owner_bound_claim_failure_record.id.clone();
    registry
        .record_first_started(
            &process_id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first start");
    registry.set_process_lease_claim_error(Some(PluginError::Session(
        "injected claim failure".to_string(),
    )));

    let worker = native_worker(&backend, owner).await;
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");
    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                operation: ProcessRecoveryOperation::ClaimLease,
                error: "plugin session error: injected claim failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        &process_id,
        "claim_lease",
        "plugin session error: injected claim failure",
    );

    registry.set_process_lease_claim_error(None);
    let retry = worker
        .drain_owner_bound_work()
        .await
        .expect("retry owner drain");
    assert_eq!(retry.abandoned, vec![process_id.to_string()]);
    assert!(retry.deferred.is_empty());
}

#[tokio::test]
async fn drain_reports_lease_renewal_backend_error_and_retries() {
    let (backend, registry) = faulted_memory_backend().await;
    let owner = local_owner("drain-renew-failure", "host-a", "start-a");
    let _process_id = "owner-bound-renew-failure";
    let owner_bound_renew_failure_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register owner-bound row");
    let process_id = owner_bound_renew_failure_record.id.clone();
    registry
        .record_first_started(
            &process_id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first start");
    registry.set_process_lease_renew_error(Some(PluginError::Session(
        "injected lease-renewal failure".to_string(),
    )));

    let worker = native_worker(&backend, owner).await;
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");
    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                operation: ProcessRecoveryOperation::RenewLease,
                error: "plugin session error: injected lease-renewal failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        &process_id,
        "renew_lease",
        "plugin session error: injected lease-renewal failure",
    );

    registry.set_process_lease_renew_error(None);
    let retry = worker
        .drain_owner_bound_work()
        .await
        .expect("retry owner drain");
    assert_eq!(retry.abandoned, vec![process_id.to_string()]);
    assert!(retry.deferred.is_empty());
}

#[tokio::test]
async fn drain_reports_registry_read_error_instead_of_absent() {
    let (backend, registry) = faulted_memory_backend().await;
    let owner = local_owner("drain-read-failure", "host-a", "start-a");
    let _process_id = "owner-bound-read-failure";
    let owner_bound_read_failure_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register owner-bound row");
    let process_id = owner_bound_read_failure_record.id.clone();
    registry
        .record_first_started(
            &process_id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first start");
    registry.set_process_read_error(Some(PluginError::Session(
        "injected registry read failure".to_string(),
    )));

    let worker = native_worker(&backend, owner).await;
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");
    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                operation: ProcessRecoveryOperation::ReadProcess,
                error: "plugin session error: injected registry read failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        &process_id,
        "read_process",
        "plugin session error: injected registry read failure",
    );

    registry.set_process_read_error(None);
    let retry = worker
        .drain_owner_bound_work()
        .await
        .expect("retry owner drain");
    assert_eq!(retry.abandoned, vec![process_id.to_string()]);
    assert!(retry.deferred.is_empty());
}

/// Explicit decision: when the drain claims a row that has already vanished,
/// its last act is releasing that claim — and if the release fails, the report
/// names the release fault, not `Absent`. The clean absent answer would leave
/// a failed release with no trace at all.
#[tokio::test]
async fn drain_reports_release_failure_over_absent() {
    let (backend, registry) = faulted_memory_backend().await;
    let owner = local_owner("drain-release-failure", "host-a", "start-a");
    let _process_id = "owner-bound-release-failure";
    let owner_bound_release_failure_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register owner-bound row");
    let process_id = owner_bound_release_failure_record.id.clone();
    registry
        .record_first_started(
            &process_id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first start");
    registry.set_process_read_absent(true);
    registry.set_process_lease_release_error(Some(PluginError::Session(
        "injected release failure".to_string(),
    )));

    let worker = native_worker(&backend, owner).await;
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");
    assert!(report.abandoned.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                operation: ProcessRecoveryOperation::ReleaseLease,
                error: "plugin session error: injected release failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        &process_id,
        "release_lease",
        "plugin session error: injected release failure",
    );
}

#[tokio::test]
async fn drain_distinguishes_busy_and_absent_rows() {
    let (backend, registry) = faulted_memory_backend().await;
    let owner = local_owner("drain-legitimate-deferrals", "host-a", "start-a");
    let mut ids = std::collections::BTreeMap::new();
    for process_id in ["owner-bound-busy", "owner-bound-absent"] {
        let registered = registry
            .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
            .await
            .expect("register owner-bound row");
        ids.insert(process_id, registered.id.clone());
        registry
            .record_first_started(
                &registered.id,
                ProcessStarted {
                    owner: owner.clone(),
                    fencing_token: 0,
                    attempt: 1,
                    started_at_ms: 1,
                    generation: None,
                },
            )
            .await
            .expect("record first start");
    }
    registry
        .claim_process_lease(
            &ids["owner-bound-busy"],
            &LeaseOwnerIdentity::opaque("live-peer", "live-peer-incarnation"),
            60_000,
        )
        .await
        .expect("claim live peer lease")
        .acquired()
        .expect("peer acquires lease");
    let worker = native_worker(&backend, owner).await;

    let busy = worker.drain_owner_bound_work().await.expect("busy drain");
    assert_eq!(
        busy.deferred,
        vec![ProcessDrainDeferred {
            process_id: ids["owner-bound-busy"].clone(),
            disposition: ProcessRecoveryAttemptOutcome::Busy,
        }]
    );
    assert_eq!(busy.abandoned, vec![ids["owner-bound-absent"].clone()]);

    let _absent_id = "owner-bound-read-as-absent";
    let owner_bound_read_as_absent_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register read-as-absent row");
    let absent_id = owner_bound_read_as_absent_record.id.clone();
    registry
        .record_first_started(
            &absent_id,
            ProcessStarted {
                owner: worker.config().lease_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record read-as-absent start");
    registry.set_process_read_absent(true);
    let absent = worker.drain_owner_bound_work().await.expect("absent drain");
    assert_eq!(
        absent.deferred,
        vec![
            ProcessDrainDeferred {
                process_id: ids["owner-bound-busy"].clone(),
                disposition: ProcessRecoveryAttemptOutcome::Busy,
            },
            ProcessDrainDeferred {
                process_id: absent_id.clone(),
                disposition: ProcessRecoveryAttemptOutcome::Absent,
            },
        ]
    );
}
