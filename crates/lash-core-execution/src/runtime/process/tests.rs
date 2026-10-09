use super::*;

fn registration(_id: &str) -> ProcessRegistration {
    crate::testing::held_engine_registration(
        serde_json::Value::Null,
        ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
}

/// Plan `request` as a store does: the canonical preparation, then the plan.
fn plan_append(
    record: &ProcessRecord,
    request: ProcessEventAppendRequest,
    sequence: u64,
    last_event_sequence: Option<u64>,
    replay_lookup: Option<ProcessEvent>,
    occurred_at_ms: u64,
) -> Result<ProcessEventAppendPlan, crate::PluginError> {
    prepare_process_event_append(
        record,
        request.canonical(record, crate::FleetFormat::current())?,
        sequence,
        last_event_sequence,
        replay_lookup,
        occurred_at_ms,
    )
}

#[test]
fn process_event_old_system_time_json_is_rejected() {
    let record = ProcessRecord::from_registration(
        registration("process-old-time-shape"),
        crate::process_id_for_test("process-old-time-shape"),
    );
    let plan = plan_append(
        &record,
        ProcessEventAppendRequest::observer_added(
            &record.id,
            &crate::SessionId::from("observer"),
            &ProcessObserverBy::host("old-time-shape"),
        ),
        1,
        None,
        None,
        1_700_000_000_000,
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
        ProcessEvent::decode(&json.to_string(), crate::FleetFormat::current()).is_err(),
        "the pre-cutover SystemTime shape must not decode as an epoch-ms process event"
    );
}

#[test]
fn replayed_waiting_non_tail_does_not_repair_terminal_projection() {
    let record = ProcessRecord::from_registration(
        registration("process-repair-waiting"),
        crate::process_id_for_test("process-repair-waiting"),
    );
    let wait = WaitState {
        kind: WaitKind::Call {
            call_id: crate::ToolCallId::fixture("wait-call"),
            tool_id: crate::ToolId::from("wait-tool"),
        },
        since_ms: 42,
        site: None,
    };
    let waiting_request = ProcessEventAppendRequest::wait_entered(
        &crate::process_id_for_test("process-repair-waiting"),
        &wait,
    );
    let waiting = plan_append(&record, waiting_request.clone(), 1, None, None, 42)
        .expect("prepare waiting event");
    let ProcessEventAppendPlan::Insert {
        event: waiting_event,
        projected_record: waiting_record,
        ..
    } = waiting
    else {
        panic!("waiting event should insert");
    };

    let terminal = plan_append(
        &waiting_record,
        terminal_append_request(
            &waiting_record.id,
            &ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"ok": true}),
            )),
            None,
        ),
        2,
        Some(1),
        None,
        43,
    )
    .expect("prepare terminal event");
    let ProcessEventAppendPlan::Insert {
        projected_record: terminal_record,
        ..
    } = terminal
    else {
        panic!("terminal event should insert");
    };

    let replay = plan_append(
        &terminal_record,
        waiting_request,
        99,
        Some(2),
        Some(waiting_event),
        100,
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

/// A lifecycle fact that moves no status: an observer joining.
fn observed(process: &ProcessId, session: &str) -> ProcessEventAppendRequest {
    ProcessEventAppendRequest::observer_added(
        process,
        &crate::SessionId::fixture(session),
        &ProcessObserverBy::host("tests"),
    )
}

#[test]
fn replayed_generic_tail_repairs_projection_across_sender_floor_gap() {
    let mut stale_record = ProcessRecord::from_registration(
        registration("process-generic-repair"),
        crate::process_id_for_test("process"),
    );
    stale_record.updated_at_ms = 0;
    let request = observed(&stale_record.id, "progress");
    let first = plan_append(&stale_record, request.clone(), 7, None, None, 42)
        .expect("prepare generic event at a sender-floor boundary");
    let ProcessEventAppendPlan::Insert { event, .. } = first else {
        panic!("first generic event should insert")
    };

    let replay = plan_append(
        &stale_record,
        request,
        100,
        Some(event.sequence),
        Some(event),
        100,
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
    let record = ProcessRecord::from_registration(
        registration("process-generic-stale-replay"),
        crate::process_id_for_test("process"),
    );
    let first_request = observed(&record.id, "first");
    let first = plan_append(&record, first_request.clone(), 1, None, None, 42)
        .expect("prepare first generic event");
    let ProcessEventAppendPlan::Insert {
        event: first_event,
        projected_record: first_record,
        ..
    } = first
    else {
        panic!("first generic event should insert")
    };

    let second = plan_append(
        &first_record,
        observed(&record.id, "second"),
        2,
        Some(1),
        None,
        100,
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

    let replay = plan_append(
        &current_record,
        first_request,
        3,
        Some(2),
        Some(first_event),
        200,
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

/// An ended referrer is a typed refusal, not a prose sentinel (ADR 0113
/// §2.7): the classifier answers the fenced referrer from the error's typed
/// cause, across the controller-error conversion and the journal's serde, so
/// a session error that merely quotes the message is not a fence.
#[test]
fn an_ended_referrer_classifies_by_its_typed_cause_not_its_message() {
    let referrer = crate::ArtifactReferrer::ProcessRecord(crate::ProcessId::fixture("ended"));
    let ended: crate::PluginError = crate::ArtifactStoreError::ReferrerEnded {
        referrer: referrer.clone(),
    }
    .into();
    assert_eq!(artifact_referrer_ended(&ended), Some(&referrer));
    assert!(!ended.is_retryable());
    // A permanent fence is terminal: a redrive meets it again.
    assert!(ended.is_terminal());

    let controller: crate::RuntimeEffectControllerError =
        crate::StoreError::ArtifactReferrerEnded {
            referrer: referrer.clone(),
        }
        .into();
    assert_eq!(
        controller.code,
        crate::RuntimeErrorCode::ArtifactReferrerEnded
    );
    let journaled: crate::RuntimeEffectControllerError =
        serde_json::from_value(serde_json::to_value(&controller).expect("encode")).expect("decode");
    assert_eq!(
        artifact_referrer_ended(&crate::PluginError::RuntimeEffectController(journaled)),
        Some(&referrer)
    );

    let quoted = crate::PluginError::Session(format!("artifact referrer `{referrer}` has ended"));
    assert_eq!(
        artifact_referrer_ended(&quoted),
        None,
        "prose that quotes the refusal is not a typed fence"
    );
    let missing: crate::PluginError = crate::ArtifactStoreError::ArtifactMissing {
        artifact_ref: "env".to_string(),
    }
    .into();
    assert_eq!(
        artifact_referrer_ended(&missing),
        None,
        "missing bytes are not a fence"
    );
}
