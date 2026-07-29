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
        target_scope_id: SessionScope::new("target").id(),
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
    let first = prepare_process_event_append(&record, request.clone(), 1, None, 42, None)
        .expect("prepare first terminal event");
    let ProcessEventAppendPlan::Insert {
        event: first_event,
        payload_hash: first_payload_hash,
        ..
    } = first
    else {
        panic!("first terminal event should insert");
    };

    let replayed = prepare_process_event_append(
        &record,
        request,
        99,
        Some((first_payload_hash, first_event)),
        100,
        None,
    )
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
