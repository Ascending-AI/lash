//! The in-memory registry's own lock discipline: pause points inside its
//! writes prove a concurrent prune, registration or cleanup serializes behind
//! the write it would otherwise interleave with. The SQL registries get the
//! same serialization from their transactions and expose no such pause, so
//! these tests belong to the in-memory registry and go with it (ADR 0102).

use std::sync::Arc;

use super::*;
use crate::{
    ProcessEventLog as _, ProcessLeases as _, ProcessLifecycle as _, ProcessObserverRegistry as _,
    ProcessQuery as _, ProcessRegistrar as _, ProcessRegistryTestSupport as _,
    ProcessRetention as _, ProcessWakeOutbox as _,
};

fn registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        crate::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        crate::RecoveryContract::ExternallyOwned,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

fn wake_registration(id: &str, target_session_id: &SessionId) -> ProcessRegistration {
    registration(id)
        .with_wake_session_id(Some(SessionId::from(target_session_id.to_string())))
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
}

#[tokio::test]
async fn prune_serializes_same_id_reregistration_and_fresh_wake_cleanup() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let process_id = "prune-reregister-race";
    let target_session_id = "prune-reregister-target";
    registry
        .register_process(wake_registration(
            process_id,
            &SessionId::from(target_session_id),
        ))
        .await
        .expect("register old process incarnation");
    registry
        .append_event(
            &ProcessId::from(process_id),
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "old"}),
            )
            .with_replay_key("prune-reregister:old"),
        )
        .await
        .expect("append old wake");
    let claimed = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("claim old wake delivery")
        .pop()
        .expect("old wake delivery");
    registry
        .mark_wake_enqueued(
            &claimed.delivery_id,
            claimed.claim_token().expect("wake claim token"),
        )
        .await
        .expect("settle old wake delivery");
    registry
        .complete_process(
            &ProcessId::from(process_id),
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("old done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete old process incarnation");

    let pause = registry.pause_next_prune_after_managed_removal();
    let prune_registry = Arc::clone(&registry);
    let prune = crate::task::spawn(async move {
        prune_registry
            .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
            .await
    });
    pause.wait_until_validated().await;

    let registration_registry = Arc::clone(&registry);
    let fresh = crate::task::spawn(async move {
        registration_registry
            .register_process(wake_registration(
                process_id,
                &SessionId::from(target_session_id),
            ))
            .await
            .expect("re-register process");
        registration_registry
            .append_event(
                &ProcessId::from(process_id),
                ProcessEventAppendRequest::new(
                    "producer.wake",
                    serde_json::json!({"wake_input": "fresh"}),
                )
                .with_replay_key("prune-reregister:fresh"),
            )
            .await
            .expect("append fresh wake")
            .wake_delivery
            .expect("fresh wake delivery")
    });

    if registry.transaction_is_locked_for_testing() {
        pause.resume();
        prune
            .await
            .expect("join serialized prune")
            .expect("serialized prune");
    } else {
        let fresh_wake = fresh.await.expect("join racing fresh wake");
        pause.resume();
        prune
            .await
            .expect("join racing prune")
            .expect("racing prune");
        assert!(
            registry
                .list_wake_deliveries(None)
                .await
                .expect("list fresh deliveries after racing prune")
                .iter()
                .any(|delivery| delivery.wake.wake_id == fresh_wake.wake_id),
            "prune deleted the concurrently registered incarnation's fresh wake"
        );
        return;
    }

    let fresh_wake = fresh.await.expect("join serialized fresh wake");
    assert!(
        registry
            .list_wake_deliveries(None)
            .await
            .expect("list fresh deliveries after serialized prune")
            .iter()
            .any(|delivery| delivery.wake.wake_id == fresh_wake.wake_id),
        "fresh wake must survive the old incarnation's complete prune"
    );
}

#[tokio::test]
async fn lifecycle_append_serializes_target_cleanup_and_cannot_recreate_sender_floor() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let process_id = "lifecycle-target-cleanup-race";
    let target_session_id = "lifecycle-target-cleanup-session";
    registry
        .register_process(wake_registration(
            process_id,
            &SessionId::from(target_session_id),
        ))
        .await
        .expect("register lifecycle process");
    registry
        .append_event(
            &ProcessId::from(process_id),
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "seed floor"}),
            )
            .with_replay_key("lifecycle-target-cleanup:seed"),
        )
        .await
        .expect("seed sender floor");

    let pause = registry.pause_next_append_after_target_snapshot();
    let append_registry = Arc::clone(&registry);
    let append = crate::task::spawn(async move {
        append_registry
            .set_external_ref(
                &ProcessId::from(process_id),
                ProcessExternalRef {
                    backend: "test".to_string(),
                    id: "external".to_string(),
                    metadata: None,
                    segment_ordinal: None,
                },
            )
            .await
    });
    pause.wait_until_validated().await;

    let cleanup_registry = Arc::clone(&registry);
    let cleanup = crate::task::spawn(async move {
        cleanup_registry
            .delete_session_process_state(&SessionId::from(target_session_id))
            .await
    });
    if registry.transaction_is_locked_for_testing() {
        pause.resume();
        append
            .await
            .expect("join serialized lifecycle append")
            .expect("serialized lifecycle append");
        cleanup
            .await
            .expect("join serialized target cleanup")
            .expect("serialized target cleanup");
    } else {
        cleanup
            .await
            .expect("join racing target cleanup")
            .expect("racing target cleanup");
        pause.resume();
        append
            .await
            .expect("join racing lifecycle append")
            .expect("racing lifecycle append");
    }

    assert_eq!(
        registry
            .wake_allocation_floor_for_testing(
                &SessionId::from(target_session_id),
                &ProcessId::from(process_id)
            )
            .await
            .expect("read sender floor after target cleanup"),
        None,
        "a lifecycle append must not recreate sender state after target cleanup"
    );
}

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
