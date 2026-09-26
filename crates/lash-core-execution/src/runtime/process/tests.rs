use std::collections::BTreeMap;

use super::materialization::select_value;
use super::*;

fn registration(_id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        crate::RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

#[test]
fn process_event_old_system_time_json_is_rejected() {
    let record = ProcessRecord::from_registration(
        registration("process-old-time-shape"),
        crate::process_id_for_test("process-old-time-shape"),
    );
    let plan = prepare_process_event_append(
        &record,
        ProcessEventAppendRequest::new("process.caller_departed", serde_json::Value::Null)
            .with_replay_key("process-old-time-shape:caller-departed"),
        1,
        None,
        None,
        1_700_000_000_000,
        None,
        crate::FleetFormat::current(),
    )
    .expect("prepare process event");
    let ProcessEventAppendPlan::Insert { event, .. } = plan else {
        panic!("new process event should insert");
    };
    let mut json = serde_json::to_value(event).expect("encode process event");
    json["occurred_at"] = serde_json::json!({
        "secs_since_epoch": 1_700_000_000,
        "nanos_since_epoch": 0,
    });

    assert!(
        serde_json::from_value::<ProcessEvent>(json).is_err(),
        "the pre-cutover SystemTime shape must not decode as an epoch-ms process event"
    );
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
        "Background process wake\nProcess: p_c5546c16360677e5a56c42a3a5c9e20c\nEvent: process.ready #7\nWake input:\nline one\nline two"
    );
}

#[test]
fn process_wake_turn_cause_preserves_process_origin() {
    let process_caused_by = crate::CausalRef::SessionNode {
        session_id: SessionId::from("target"),
        node_id: "trigger:button".to_string(),
    };
    let wake = wake_delivery("process.ready", Some(process_caused_by.clone()));

    let cause = process_wake_turn_cause(&wake);

    assert_eq!(cause.id, "wake:abc");
    assert_eq!(cause.event_type, "process.ready");
    assert_eq!(
        cause.text,
        "Background process wake\nProcess: p_c5546c16360677e5a56c42a3a5c9e20c\nEvent: process.ready #7\nWake input:\nline one\nline two"
    );
    assert!(matches!(
        cause.origin,
        crate::MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            caused_by,
        } if process_id == crate::process_id_for_test("process-1")
            && event_type == "process.ready"
            && sequence == 7
            && wake_id.as_deref() == Some("wake:abc")
            && caused_by == Some(process_caused_by)
    ));
}

#[test]
fn process_wake_delivery_carries_event_invocation_and_process_cause() {
    let process_caused_by = crate::CausalRef::SessionNode {
        session_id: SessionId::from("target"),
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
        } if process_id == crate::process_id_for_test("process-1") && event_type == "process.ready"
    ));
}

fn wake_delivery(
    event_type: impl Into<String>,
    process_caused_by: Option<crate::CausalRef>,
) -> ProcessWakeDelivery {
    let event_type = event_type.into();
    ProcessWakeDelivery {
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: "wake:abc".to_string(),
        target_session_id: SessionId::from("target"),
        process_id: crate::process_id_for_test("process-1"),
        sequence: 7,
        event_type: event_type.clone(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::RuntimeAttribution::for_session("target"),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: crate::process_id_for_test("process-1"),
                sequence: 7,
                event_type,
            },
            caused_by: Some(crate::CausalRef::Process {
                process_id: crate::process_id_for_test("process-1"),
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
fn replayed_waiting_non_tail_does_not_repair_terminal_projection() {
    let record = ProcessRecord::from_registration(
        registration("process-repair-waiting"),
        crate::process_id_for_test("process-repair-waiting"),
    );
    let wait = WaitState {
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "wait-key".to_string(),
            ordinal: 1,
        },
        since_ms: 42,
    };
    let waiting_request = ProcessEventAppendRequest::wait_entered(
        &crate::process_id_for_test("process-repair-waiting"),
        &wait,
    );
    let waiting = prepare_process_event_append(
        &record,
        waiting_request.clone(),
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect("prepare waiting event");
    let ProcessEventAppendPlan::Insert {
        event: waiting_event,
        projected_record: waiting_record,
        ..
    } = waiting
    else {
        panic!("waiting event should insert");
    };

    let terminal = prepare_process_event_append(
        &waiting_record,
        ProcessEventAppendRequest::new(
            "process.completed",
            serde_json::json!({
                "await_output": ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::success(serde_json::json!({"ok": true})),
                ),
            }),
        )
        .with_replay_key("process-repair-waiting-terminal"),
        2,
        Some(1),
        None,
        43,
        None,
        crate::FleetFormat::current(),
    )
    .expect("prepare terminal event");
    let ProcessEventAppendPlan::Insert {
        projected_record: terminal_record,
        ..
    } = terminal
    else {
        panic!("terminal event should insert");
    };

    let replay = prepare_process_event_append(
        &terminal_record,
        waiting_request,
        99,
        Some(2),
        Some(waiting_event),
        100,
        None,
        crate::FleetFormat::current(),
    )
    .expect("stale waiting event should replay without repair");
    let ProcessEventAppendPlan::Replay {
        event,
        repair_record,
        ..
    } = replay
    else {
        panic!("stale waiting keyed append should replay")
    };
    assert_eq!(event.sequence, 1);
    assert!(
        repair_record.is_none(),
        "stale waiting replay must not repair a terminal projection"
    );
}

#[test]
fn replayed_terminal_event_repairs_non_terminal_status_projection() {
    let record = ProcessRecord::from_registration(
        registration("process-repair"),
        crate::process_id_for_test("process-repair"),
    );
    let request = ProcessEventAppendRequest::new(
        "process.completed",
        serde_json::json!({
            "await_output": ProcessAwaitOutput::from_tool_output(
                crate::ToolCallOutput::success(serde_json::json!({"ok": true})),
            ),
        }),
    )
    .with_replay_key("process-repair-terminal");
    let first = prepare_process_event_append(
        &record,
        request.clone(),
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect("prepare first terminal event");
    let ProcessEventAppendPlan::Insert {
        event: first_event, ..
    } = first
    else {
        panic!("first terminal event should insert");
    };

    let replayed = prepare_process_event_append(
        &record,
        request,
        99,
        Some(1),
        Some(first_event),
        100,
        None,
        crate::FleetFormat::current(),
    )
    .expect("prepare replayed terminal event");

    let ProcessEventAppendPlan::Replay {
        event,
        repair_record,
        ..
    } = replayed
    else {
        panic!("terminal event replay should replay");
    };
    assert_eq!(event.sequence, 1);
    assert_eq!(event.occurred_at, 42);
    assert!(matches!(
        repair_record.as_ref().map(|record| record.status),
        Some(ProcessStatus::Completed)
    ));
    assert!(matches!(
        repair_record.and_then(|record| record.outcome),
        Some(ProcessAwaitOutput::Settled { .. })
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
    let mut stale_record =
        ProcessRecord::from_registration(registration, crate::process_id_for_test("process"));
    stale_record.updated_at_ms = 0;
    let request =
        ProcessEventAppendRequest::new("producer.progress", serde_json::json!({"value": 1}))
            .with_replay_key("process-generic-repair:progress");
    let first = prepare_process_event_append(
        &stale_record,
        request.clone(),
        7,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
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
        crate::FleetFormat::current(),
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

#[test]
fn replayed_generic_non_tail_does_not_rewind_projection_timestamp() {
    let registration =
        registration("process-generic-stale-replay").with_extra_event_types([ProcessEventType {
            name: "producer.progress".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: ProcessEventSemanticsSpec::default(),
        }]);
    let record =
        ProcessRecord::from_registration(registration, crate::process_id_for_test("process"));
    let first_request =
        ProcessEventAppendRequest::new("producer.progress", serde_json::json!({"value": 1}))
            .with_replay_key("process-generic-stale-replay:1");
    let first = prepare_process_event_append(
        &record,
        first_request.clone(),
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect("prepare first generic event");
    let ProcessEventAppendPlan::Insert {
        event: first_event,
        projected_record: first_record,
        ..
    } = first
    else {
        panic!("first generic event should insert")
    };

    let second = prepare_process_event_append(
        &first_record,
        ProcessEventAppendRequest::new("producer.progress", serde_json::json!({"value": 2}))
            .with_replay_key("process-generic-stale-replay:2"),
        2,
        Some(1),
        None,
        100,
        None,
        crate::FleetFormat::current(),
    )
    .expect("prepare second generic event");
    let ProcessEventAppendPlan::Insert {
        projected_record: current_record,
        ..
    } = second
    else {
        panic!("second generic event should insert")
    };
    assert_eq!(current_record.updated_at_ms, 100);

    let replay = prepare_process_event_append(
        &current_record,
        first_request,
        3,
        Some(2),
        Some(first_event),
        200,
        None,
        crate::FleetFormat::current(),
    )
    .expect("stale generic event should replay without repair");
    let ProcessEventAppendPlan::Replay { repair_record, .. } = replay else {
        panic!("stale generic keyed append should replay")
    };
    assert_eq!(current_record.updated_at_ms, 100);
    assert!(
        repair_record.is_none(),
        "stale non-tail replay must not propose a projection repair"
    );
}

/// Artifact-owner retirement is a typed refusal, not a prose sentinel: the
/// classifiers answer from the runtime error code, so a session error that
/// merely quotes the message is not a retirement.
#[test]
fn artifact_owner_retirement_classifies_by_code_not_message() {
    let retired = artifact_owner_retired_error();
    assert!(artifact_owner_is_permanently_retired(&retired));
    assert!(!retired.is_retryable());
    // A permanent retirement fence is terminal: a redrive meets it again.
    assert!(retired.is_terminal());

    // The destination form of the same fence is the same classification.
    let destination = artifact_destination_owner_retired_error();
    assert!(artifact_owner_is_permanently_retired(&destination));

    let quoted = crate::PluginError::Session(
        "backend refused: artifact owner has been permanently retired".to_string(),
    );
    assert!(
        !artifact_owner_is_permanently_retired(&quoted),
        "prose that quotes the sentinel is not a typed retirement"
    );

    let missing = artifact_staging_edge_missing_error("process execution environment `env`");
    assert!(artifact_staging_owner_edge_is_missing(&missing));
    assert!(
        !artifact_owner_is_permanently_retired(&missing),
        "a missing edge is not a retirement"
    );
    let quoted_edge = crate::PluginError::Session(
        "outer failure: inner is not retained by the staging owner".to_string(),
    );
    assert!(
        !artifact_staging_owner_edge_is_missing(&quoted_edge),
        "prose that quotes the fragment is not a typed staging-edge miss"
    );
}
