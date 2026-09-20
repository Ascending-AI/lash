use super::*;
use crate::{
    ProcessEventLog as _, ProcessLeases as _, ProcessLifecycle as _, ProcessObserverRegistry as _,
    ProcessQuery as _, ProcessRegistrar as _,
};

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
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new("test-env"))),
        )
        .await
        .expect("register");
    let owner = crate::LeaseOwnerIdentity::opaque("worker-a", "incarnation-a");
    let lease = registry
        .claim_process_lease(&ProcessId::from(PROCESS_ID), &owner, 60_000)
        .await
        .expect("claim")
        .acquired()
        .expect("lease");
    registry
        .record_first_started_with_authority(
            &ProcessId::from(PROCESS_ID),
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
                &ProcessId::from(PROCESS_ID),
                ProcessEventAppendRequest::cancel_requested(
                    &writer_registry
                        .resolve_process_ref(&ProcessId::from(PROCESS_ID))
                        .await
                        .expect("retained cancellation target"),
                    &crate::CancelRequest::new(
                        crate::CancelOrigin::OperatorRequested,
                        "actor:fixture:claim_cannot_interleave_between_authority_validation_and_append",
                        11,
                    ),
                ),
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
                &ProcessId::from(PROCESS_ID),
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

/// A failed multi-process observer transfer must commit nothing: the first
/// process's observer move, its journaled events, the wake deliveries, and the
/// sender floor all stage on the cloned state and disappear with it.
#[tokio::test]
async fn a_failed_observer_transfer_leaves_no_partial_mutation() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let from_session = SessionId::from("observer-transfer-from");
    let to_session = SessionId::from("observer-transfer-to");
    let first = ProcessId::from("observer-transfer-first");
    let second = ProcessId::from("observer-transfer-second");
    let registration = |id: &str, wake: Option<SessionId>| {
        ProcessRegistration::new(
            id,
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryContract::ExternallyOwned,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
        .with_wake_session_id(wake)
        .with_extra_event_types([crate::ProcessEventType {
            name: "producer.wake".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec {
                wake: Some(crate::ProcessWakeSpec {
                    when: Some(crate::ProcessValueSelector::Present(
                        "/wake_input".to_string(),
                    )),
                    input: crate::ProcessValueSelector::Pointer("/wake_input".to_string()),
                }),
                ..crate::ProcessEventSemanticsSpec::default()
            },
        }])
    };
    registry
        .register_process(registration(first.as_str(), Some(from_session.clone())))
        .await
        .expect("register first process");
    // Seed a real wake delivery so the snapshot has a non-empty outbox row to
    // compare against after the failed transfer.
    registry
        .append_event(
            &first,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "seed"}),
            )
            .with_replay_key("observer-transfer:seed"),
        )
        .await
        .expect("seed wake delivery");
    registry
        .register_process(registration(second.as_str(), None))
        .await
        .expect("register second process");
    registry
        .add_observer(
            &from_session,
            &first,
            ProcessObserverBy::host("observer-transfer"),
        )
        .await
        .expect("observe first process");

    async fn raw_snapshot(registry: &TestLocalProcessRegistry) -> serde_json::Value {
        let raw = registry.raw_state_for_testing().await;
        serde_json::json!({
            "records": raw.records,
            "events": raw.events,
            "observers": raw.observers,
            "leases": raw.leases,
            "wake_deliveries": raw.wake_deliveries,
            "wake_allocation_floors": raw.wake_allocation_floors,
            "tombstones": raw.tombstones,
        })
    }
    let before = raw_snapshot(&registry).await;

    // The second process id is not observed by `from_session`, so the transfer
    // fails after the first process's staged mutation.
    let outcome = registry
        .transfer_observers(
            &from_session,
            &to_session,
            &[first.clone(), second.clone()],
            ProcessObserverBy::host("observer-transfer"),
        )
        .await;
    assert!(
        outcome.is_err(),
        "the second process id must fail the transfer"
    );

    let after = raw_snapshot(&registry).await;
    assert_eq!(
        after, before,
        "raw records, events, observers, deliveries, and floors must equal the pre-call snapshot"
    );
}
