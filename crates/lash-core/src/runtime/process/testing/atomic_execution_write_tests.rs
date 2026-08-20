use super::*;

#[tokio::test]
async fn claim_cannot_interleave_between_authority_validation_and_append() {
    const PROCESS_ID: &str = "atomic-authority-append";
    let registry = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(
            ProcessRegistration::new(
                PROCESS_ID,
                crate::ProcessInput::Engine {
                    kind: "test".to_string(),
                    payload: serde_json::Value::Null,
                },
                crate::RecoveryContract::Rerunnable,
                crate::ProcessProvenance::host(),
            )
            .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new("test-env"))),
        )
        .await
        .expect("register");
    let owner = crate::LeaseOwnerIdentity::opaque("worker-a", "incarnation-a");
    let lease = registry
        .claim_process_lease(PROCESS_ID, &owner, 60_000)
        .await
        .expect("claim")
        .acquired()
        .expect("lease");
    registry
        .record_first_started_with_authority(
            PROCESS_ID,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: lease.fencing_token,
                attempt: 1,
                started_at_ms: 1,
            },
            &ProcessExecutionWriteAuthority::lease(lease.clone()),
        )
        .await
        .expect("start");

    let pause = registry.pause_next_execution_write_after_validation();
    let writer_registry = Arc::clone(&registry);
    let writer_lease = lease.clone();
    let writer = crate::task::spawn(async move {
        writer_registry
            .append_event_with_authority(
                PROCESS_ID,
                ProcessEventAppendRequest::cancel_requested(PROCESS_ID, Some("race".into())),
                &ProcessExecutionWriteAuthority::lease(writer_lease),
            )
            .await
    });
    pause.wait_until_validated().await;

    let claimant_registry = Arc::clone(&registry);
    let claimant_lease = lease.clone();
    let claimant = crate::task::spawn(async move {
        claimant_registry
            .complete_process_lease(&ProcessLeaseCompletion::from_lease(&claimant_lease))
            .await?;
        claimant_registry
            .claim_process_lease(
                PROCESS_ID,
                &crate::LeaseOwnerIdentity::opaque("worker-b", "incarnation-b"),
                60_000,
            )
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), async {
            while !claimant.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "claim must remain blocked while the validated append holds the lease lock"
    );

    pause.resume();
    writer
        .await
        .expect("writer joins")
        .expect("validated writer appends");
    let claimed = claimant
        .await
        .expect("claimant joins")
        .expect("claim after append")
        .acquired()
        .expect("new lease");
    assert!(claimed.fencing_token > lease.fencing_token);
}
