use std::collections::BTreeMap;
use std::sync::Arc;

use super::materialization::select_value;
use super::*;

fn registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        crate::RecoveryDisposition::ExternallyOwned,
        ProcessProvenance::host(),
    )
}

#[test]
fn process_wake_input_from_event_payload_prefers_text_field() {
    let payload = serde_json::json!({
        "text": "ready",
        "value": "ignored"
    });

    assert_eq!(process_wake_input_from_event_payload(&payload), "ready");
}

#[test]
fn process_wake_input_from_event_payload_falls_back_to_value_field() {
    let payload = serde_json::json!({
        "value": { "status": "ready" }
    });

    assert_eq!(
        process_wake_input_from_event_payload(&payload),
        r#"{"status":"ready"}"#
    );
}

#[test]
fn process_wake_input_from_event_payload_renders_malformed_payload_as_json() {
    let payload = serde_json::json!({
        "unexpected": true
    });

    assert_eq!(
        process_wake_input_from_event_payload(&payload),
        r#"{"unexpected":true}"#
    );
}

#[test]
fn process_wake_input_from_event_payload_renders_plain_scalar_payload_as_json() {
    let payload = serde_json::json!(42);

    assert_eq!(process_wake_input_from_event_payload(&payload), "42");
}

#[test]
fn process_wake_turn_text_frames_process_id_sequence_and_input() {
    let wake = wake_delivery("process.ready", None);

    assert_eq!(
        process_wake_turn_text(&wake),
        "Background process wake\nProcess: process-1\nEvent: process.ready #7\nWake input:\nline one\nline two"
    );
}

#[test]
fn process_wake_turn_cause_preserves_process_origin() {
    let process_caused_by = crate::CausalRef::SessionNode {
        session_id: "target".to_string(),
        node_id: "trigger:button".to_string(),
    };
    let wake = wake_delivery("process.ready", Some(process_caused_by.clone()));

    let cause = process_wake_turn_cause(&wake);

    assert_eq!(cause.id, "wake:abc");
    assert_eq!(cause.event_type, "process.ready");
    assert_eq!(
        cause.text,
        "Background process wake\nProcess: process-1\nEvent: process.ready #7\nWake input:\nline one\nline two"
    );
    assert!(matches!(
        cause.origin,
        crate::MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            caused_by,
        } if process_id == "process-1"
            && event_type == "process.ready"
            && sequence == 7
            && wake_id.as_deref() == Some("wake:abc")
            && caused_by == Some(process_caused_by)
    ));
}

#[test]
fn process_wake_delivery_carries_event_invocation_and_process_cause() {
    let process_caused_by = crate::CausalRef::SessionNode {
        session_id: "target".to_string(),
        node_id: "trigger:button".to_string(),
    };
    let wake = wake_delivery("process.ready", Some(process_caused_by.clone()));

    assert_eq!(wake.event_type, "process.ready");
    assert_eq!(wake.process_caused_by, Some(process_caused_by));
    assert!(matches!(
        wake.event_invocation.subject,
        crate::RuntimeSubject::ProcessEvent {
            process_id,
            sequence: 7,
            event_type,
        } if process_id == "process-1" && event_type == "process.ready"
    ));
}

fn wake_delivery(
    event_type: impl Into<String>,
    process_caused_by: Option<crate::CausalRef>,
) -> ProcessWakeDelivery {
    let event_type = event_type.into();
    ProcessWakeDelivery {
        wake_id: "wake:abc".to_string(),
        target_session_id: "target".to_string(),
        process_id: "process-1".to_string(),
        sequence: 7,
        event_type: event_type.clone(),
        event_invocation: crate::RuntimeInvocation {
            scope: crate::RuntimeScope::new("target"),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: "process-1".to_string(),
                sequence: 7,
                event_type,
            },
            caused_by: Some(crate::CausalRef::Process {
                process_id: "process-1".to_string(),
            }),
            replay: None,
        },
        process_caused_by,
        authority: crate::QueuedWorkAuthority::default(),
        input: "line one\nline two".to_string(),
        created_at_ms: 123,
    }
}

#[test]
fn selector_extracts_payload_pointer_const_template_and_present() {
    let payload = serde_json::json!({
        "line": "done",
        "wake_input": "wake me"
    });

    assert_eq!(
        select_value(&payload, &ProcessValueSelector::Payload).unwrap(),
        payload
    );
    assert_eq!(
        select_value(
            &payload,
            &ProcessValueSelector::Pointer("/line".to_string())
        )
        .unwrap(),
        serde_json::json!("done")
    );
    assert_eq!(
        select_value(
            &payload,
            &ProcessValueSelector::Const(serde_json::json!({"ok": true}))
        )
        .unwrap(),
        serde_json::json!({"ok": true})
    );
    assert_eq!(
        select_value(
            &payload,
            &ProcessValueSelector::Template {
                template: "event: {line}".to_string(),
                fields: BTreeMap::from([(
                    "line".to_string(),
                    ProcessValueSelector::Pointer("/line".to_string())
                )]),
            },
        )
        .unwrap(),
        serde_json::json!("event: done")
    );
    assert_eq!(
        select_value(
            &payload,
            &ProcessValueSelector::Present("/wake_input".to_string())
        )
        .unwrap(),
        serde_json::json!(true)
    );
}

#[test]
fn replayed_terminal_event_repairs_non_terminal_status_projection() {
    let record = ProcessRecord::from_registration(registration("process-repair"));
    let request = ProcessEventAppendRequest::new(
        "process.completed",
        serde_json::json!({
            "await_output": ProcessAwaitOutput::Success {
                value: serde_json::json!({"ok": true}),
                control: None,
            },
        }),
    )
    .with_replay_key("process-repair-terminal");
    let first = prepare_process_event_append(&record, request.clone(), 1, None, None, 42, None)
        .expect("prepare first terminal event");
    let ProcessEventAppendPlan::Insert {
        event: first_event, ..
    } = first
    else {
        panic!("first terminal event should insert");
    };

    let replayed =
        prepare_process_event_append(&record, request, 99, Some(1), Some(first_event), 100, None)
            .expect("prepare replayed terminal event");

    let ProcessEventAppendPlan::Replay {
        event,
        repair_record,
        occurred_at_ms,
        ..
    } = replayed
    else {
        panic!("terminal event replay should replay");
    };
    assert_eq!(event.sequence, 1);
    assert_eq!(occurred_at_ms, 42);
    assert!(matches!(
        repair_record.as_ref().map(|record| record.status),
        Some(ProcessStatus::Completed)
    ));
    assert!(matches!(
        repair_record.and_then(|record| record.outcome),
        Some(ProcessAwaitOutput::Success { .. })
    ));
}

#[test]
fn replayed_generic_tail_repairs_projection_across_sender_floor_gap() {
    let registration =
        registration("process-generic-repair").with_extra_event_types([ProcessEventType {
            name: "producer.progress".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: ProcessEventSemanticsSpec::default(),
        }]);
    let mut stale_record = ProcessRecord::from_registration(registration);
    stale_record.updated_at_ms = 0;
    let request =
        ProcessEventAppendRequest::new("producer.progress", serde_json::json!({"value": 1}))
            .with_replay_key("process-generic-repair:progress");
    let first =
        prepare_process_event_append(&stale_record, request.clone(), 7, None, None, 42, None)
            .expect("prepare generic event at a sender-floor boundary");
    let ProcessEventAppendPlan::Insert { event, .. } = first else {
        panic!("first generic event should insert")
    };

    let replay = prepare_process_event_append(
        &stale_record,
        request,
        100,
        Some(event.sequence),
        Some(event),
        100,
        None,
    )
    .expect("replay generic tail across a sender-floor gap");
    let ProcessEventAppendPlan::Replay { repair_record, .. } = replay else {
        panic!("generic keyed append should replay")
    };
    assert_eq!(
        repair_record
            .expect("generic tail replay must repair the stale projection")
            .updated_at_ms,
        42
    );
}

// Contract invariants (registration idempotency, event/wake materialization,
// ack suppression, terminal/await, observer edges, session deletion) live in the
// backend-agnostic conformance suite so the in-memory and Sqlite registries are
// held to one spec. See `crate::testing::conformance`.
#[tokio::test]
async fn test_local_process_registry_satisfies_conformance() {
    crate::testing::conformance::process_registry(|| {
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>
    })
    .await;
}

#[tokio::test]
async fn test_local_process_registry_pagination_satisfies_conformance() {
    crate::testing::conformance::process_registry_pagination(Arc::new(
        TestLocalProcessRegistry::default(),
    ))
    .await;
}

#[tokio::test]
async fn test_local_process_prune_scopes_to_the_retention_filter() {
    crate::testing::conformance::process_prune_scoped_by_originator(Arc::new(
        TestLocalProcessRegistry::default(),
    ))
    .await;
}

fn wake_registration(id: &str, target_session_id: &str) -> ProcessRegistration {
    registration(id)
        .with_wake_session_id(Some(target_session_id.to_string()))
        .with_extra_event_types([ProcessEventType {
            name: "producer.wake".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: ProcessEventSemanticsSpec {
                wake: Some(ProcessWakeSpec {
                    when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                    input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                }),
                ..ProcessEventSemanticsSpec::default()
            },
        }])
}

#[tokio::test]
async fn prune_serializes_same_id_reregistration_and_fresh_wake_cleanup() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let process_id = "prune-reregister-race";
    let target_session_id = "prune-reregister-target";
    registry
        .register_process(wake_registration(process_id, target_session_id))
        .await
        .expect("register old process incarnation");
    registry
        .append_event(
            process_id,
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
            claimed.claim_token.as_deref().expect("wake claim token"),
        )
        .await
        .expect("settle old wake delivery");
    registry
        .complete_process(
            process_id,
            ProcessAwaitOutput::Success {
                value: serde_json::json!("old done"),
                control: None,
            },
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
            .register_process(wake_registration(process_id, target_session_id))
            .await
            .expect("re-register process");
        registration_registry
            .append_event(
                process_id,
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
        .register_process(wake_registration(process_id, target_session_id))
        .await
        .expect("register lifecycle process");
    registry
        .append_event(
            process_id,
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
                process_id,
                ProcessExternalRef {
                    backend: "test".to_string(),
                    id: "external".to_string(),
                    metadata: None,
                },
            )
            .await
    });
    pause.wait_until_validated().await;

    let cleanup_registry = Arc::clone(&registry);
    let cleanup = crate::task::spawn(async move {
        cleanup_registry
            .delete_session_process_state(target_session_id)
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
            .wake_allocation_floor_for_testing(target_session_id, process_id)
            .await
            .expect("read sender floor after target cleanup"),
        None,
        "a lifecycle append must not recreate sender state after target cleanup"
    );
}

#[tokio::test]
async fn in_memory_leased_completion_replay_repairs_projection() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let registry_for_corruption = Arc::clone(&registry);
    crate::testing::conformance::leased_completion_replay_repairs_projection(
        registry as Arc<dyn ProcessRegistry>,
        move |stale| async move {
            registry_for_corruption
                .replace_process_projection_for_testing(stale)
                .await;
        },
    )
    .await;
}

#[tokio::test]
async fn delete_session_process_command_revokes_only_observer_edges() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let registry_dyn = Arc::clone(&registry) as Arc<dyn ProcessRegistry>;
    for process_id in ["sole", "shared"] {
        registry
            .register_process(registration(process_id))
            .await
            .expect("register");
        registry
            .add_observer(
                "deleted",
                process_id,
                ProcessObserverBy::host(format!("deleted:{process_id}")),
            )
            .await
            .expect("observe from deleted");
    }
    registry
        .add_observer(
            "remaining",
            "shared",
            ProcessObserverBy::host("remaining:shared"),
        )
        .await
        .expect("observe from remaining");
    let sole_events = serde_json::to_vec(
        &registry
            .events_after("sole", 0)
            .await
            .expect("sole events before delete"),
    )
    .expect("serialize sole events");
    let shared_events = serde_json::to_vec(
        &registry
            .events_after("shared", 0)
            .await
            .expect("shared events before delete"),
    )
    .expect("serialize shared events");
    let controller = crate::InlineRuntimeEffectController::default();
    let invocation = crate::RuntimeInvocation::effect(
        crate::RuntimeScope::new("deleted"),
        "process:delete-session:deleted",
        crate::RuntimeEffectKind::Process,
        "deleted:delete-session",
    );

    let outcome = crate::RuntimeEffectController::execute_effect(
        &controller,
        crate::RuntimeEffectEnvelope::new(
            invocation,
            crate::RuntimeEffectCommand::process(crate::ProcessCommand::DeleteSession {
                session_id: "deleted".to_string(),
            }),
        ),
        crate::RuntimeEffectLocalExecutor::processes(registry_dyn, None),
    )
    .await
    .expect("delete session process command");

    let crate::RuntimeEffectOutcome::Process {
        result: crate::ProcessEffectOutcome::DeleteSession { report },
    } = outcome
    else {
        panic!("unexpected delete session outcome: {outcome:?}");
    };
    assert_eq!(report.removed_observer_count, 2);
    assert_eq!(
        serde_json::to_vec(&registry.events_after("sole", 0).await.expect("sole events"))
            .expect("serialize sole events"),
        sole_events
    );
    assert_eq!(
        serde_json::to_vec(
            &registry
                .events_after("shared", 0)
                .await
                .expect("shared events")
        )
        .expect("serialize shared events"),
        shared_events
    );
}
