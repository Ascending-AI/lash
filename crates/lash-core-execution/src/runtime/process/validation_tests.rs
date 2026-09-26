use super::{
    ProcessEventAppendPlan, prepare_process_event_append, prepare_process_registration,
    validate_process_registration,
};
use crate::runtime::process::{
    PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EVENT_VOCABULARY_VERSION, ProcessEffectOmissions,
    ProcessEffectOmittedCounts, ProcessEffectOutcomeClass, ProcessEffectSummaryOccurrence,
    validate_generic_process_event_append,
};
use crate::{
    AbandonRequest, ProcessEventAppendRequest, ProcessExternalRef, ProcessInput, ProcessProvenance,
    ProcessRecord, ProcessRegistration, ProcessStarted, RecoveryContract, WaitKind, WaitState,
};

fn fixture_registration(_label: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
}

fn registration_for_input(input: ProcessInput) -> ProcessRegistration {
    ProcessRegistration::new(
        input,
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
}

#[test]
fn producer_cannot_override_runtime_lifecycle_event_types() {
    let mut collision =
        super::runtime_lifecycle_event_type("process.waiting").expect("reserved event type");
    collision.semantics.terminal = Some(crate::ProcessTerminalSpec {
        status: crate::ProcessStatus::Completed,
        await_output: None,
    });
    let registration = fixture_registration("reserved-collision").with_event_types([collision]);
    let error = prepare_process_registration(registration)
        .expect_err("reserved lifecycle collision must be rejected");
    assert!(
        error
            .to_string()
            .contains("reserved runtime lifecycle event type `process.waiting`")
    );
}

#[test]
fn process_event_vocabulary_version_is_pinned() {
    assert_eq!(PROCESS_EVENT_VOCABULARY_VERSION, 1);
}

fn effect_summary_request() -> crate::ProcessEventAppendRequest {
    ProcessEffectSummaryOccurrence::new(
        "resource_operation:node",
        1,
        "fixture.operation",
        ProcessEffectOutcomeClass::Failure,
        Some(crate::FailureCode::from_foreign_wire("fixture:failed")),
        "lashlang:scope:resource:17:fixture.operation:23:resource_operation:node:1",
        crate::FleetFormat::current(),
    )
    .append_request()
}

#[test]
fn effect_summary_refuses_predecessor_vocabulary() {
    let record = ProcessRecord::from_registration(
        fixture_registration("effect-summary-predecessor"),
        crate::process_id_for_test("record"),
    );
    let mut request = effect_summary_request();
    request.payload["vocabulary_version"] = serde_json::json!(0);
    request.payload["unknown_predecessor_field"] = serde_json::json!(true);

    let error = prepare_process_event_append(
        &record,
        request,
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect_err("a predecessor effect-summary payload must be refused");
    assert!(
        error
            .to_string()
            .contains("effect summary vocabulary version 0 is unsupported; expected 1"),
        "{error}"
    );
}

#[test]
fn effect_summary_refuses_unknown_field() {
    let record = ProcessRecord::from_registration(
        fixture_registration("effect-summary-unknown-field"),
        crate::process_id_for_test("record"),
    );
    let mut request = effect_summary_request();
    request.payload["unknown"] = serde_json::json!(true);

    let error = prepare_process_event_append(
        &record,
        request,
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect_err("an unknown effect-summary payload field must be refused");
    assert!(
        error.to_string().contains("unknown field `unknown`"),
        "{error}"
    );
}

#[test]
fn effect_summary_refuses_unknown_runtime_kind() {
    let registration =
        fixture_registration("effect-summary-unknown-kind").with_extra_event_types([
            crate::ProcessEventType {
                name: "process.effect_future".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            },
        ]);
    let record =
        ProcessRecord::from_registration(registration, crate::process_id_for_test("record"));
    let request =
        crate::ProcessEventAppendRequest::new("process.effect_future", serde_json::json!({}))
            .with_replay_key("future-effect");

    let error = prepare_process_event_append(
        &record,
        request,
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect_err("an unknown runtime-owned effect kind must be refused");
    assert!(matches!(
        error,
        crate::PluginError::ReservedProcessEvent { .. }
    ));
}

#[test]
fn effect_summary_refuses_payload_and_append_identity_drift() {
    let record = ProcessRecord::from_registration(
        fixture_registration("effect-summary-identity"),
        crate::process_id_for_test("record"),
    );
    let mut request = effect_summary_request();
    request
        .replay
        .as_mut()
        .expect("effect event has a replay key")
        .key = "different-effect".to_string();

    let error = prepare_process_event_append(
        &record,
        request,
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect_err("the payload may not claim a different effect");
    assert!(
        error
            .to_string()
            .contains("payload replay_key must equal the append replay key"),
        "{error}"
    );
}

#[test]
fn effect_summary_replay_is_a_noop_and_changed_payload_conflicts() {
    let record = ProcessRecord::from_registration(
        fixture_registration("effect-summary-replay"),
        crate::process_id_for_test("record"),
    );
    let request = effect_summary_request();
    let insert = prepare_process_event_append(
        &record,
        request.clone(),
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect("the first effect outcome inserts");
    let ProcessEventAppendPlan::Insert { event, .. } = insert else {
        panic!("the first effect outcome must insert")
    };
    let replay = prepare_process_event_append(
        &record,
        request.clone(),
        2,
        Some(1),
        Some(event.clone()),
        43,
        None,
        crate::FleetFormat::current(),
    )
    .expect("an identical effect outcome replays");
    assert!(matches!(replay, ProcessEventAppendPlan::Replay { .. }));

    let mut changed = request;
    changed.payload["outcome_class"] = serde_json::json!("success");
    changed
        .payload
        .as_object_mut()
        .expect("object")
        .remove("code");
    let error = prepare_process_event_append(
        &record,
        changed,
        2,
        Some(1),
        Some(event),
        43,
        None,
        crate::FleetFormat::current(),
    )
    .expect_err("a changed outcome under one replay key must conflict");
    assert!(
        error
            .to_string()
            .contains("conflicts with an existing event")
    );
}

#[test]
fn the_generic_append_refuses_runtime_owned_effect_summary_kinds() {
    let mut counts = ProcessEffectOmittedCounts::default();
    counts.record(ProcessEffectOutcomeClass::Success);
    for request in [
        effect_summary_request(),
        ProcessEffectOmissions::new(
            std::collections::BTreeMap::from([("node".to_string(), counts)]),
            crate::FleetFormat::current(),
        )
        .append_request("omissions"),
    ] {
        let event_type = request.event_type.clone();
        assert!(
            matches!(
                validate_generic_process_event_append(&request),
                Err(crate::PluginError::ReservedProcessEvent { event_type: refused })
                    if refused == event_type
            ),
            "a host append of `{event_type}` must be refused"
        );
    }
}

#[test]
fn effect_summary_refuses_occurrences_beyond_the_cap_and_malformed_omissions() {
    let record = ProcessRecord::from_registration(
        fixture_registration("effect-summary-cap"),
        crate::process_id_for_test("record"),
    );
    let beyond = ProcessEffectSummaryOccurrence::new(
        "node",
        PROCESS_EFFECT_OCCURRENCE_CAP + 1,
        "fixture.operation",
        ProcessEffectOutcomeClass::Success,
        None,
        "effect:beyond",
        crate::FleetFormat::current(),
    )
    .append_request();
    let error = prepare_process_event_append(
        &record,
        beyond,
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect_err("the writer never records an occurrence past the cap");
    assert!(
        error.to_string().contains("outside the recorded cap"),
        "{error}"
    );

    let empty = ProcessEffectOmissions::new(
        std::collections::BTreeMap::new(),
        crate::FleetFormat::current(),
    )
    .append_request("omissions");
    let error = prepare_process_event_append(
        &record,
        empty,
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect_err("an omission record must name an omission");
    assert!(
        error.to_string().contains("no omitted occurrence"),
        "{error}"
    );
}

#[test]
fn terminal_semantics_reject_non_terminal_status() {
    let registration = fixture_registration("invalid-terminal-status").with_extra_event_types([
        crate::ProcessEventType {
            name: "producer.invalid_terminal".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec {
                terminal: Some(crate::ProcessTerminalSpec {
                    status: crate::ProcessStatus::Running,
                    await_output: Some(crate::ProcessValueSelector::Payload),
                }),
                ..crate::ProcessEventSemanticsSpec::default()
            },
        },
    ]);
    let error = prepare_process_registration(registration)
        .expect_err("non-terminal status must be rejected at registration");
    assert!(
        error
            .to_string()
            .contains("must declare a terminal status, got `running`")
    );
}

#[test]
fn a_core_named_override_remains_a_valid_registration() {
    let with_core_events = prepare_process_registration(fixture_registration("second-lookup-id"))
        .expect("prepare exact core defaults");

    let mut overridden = with_core_events.clone();
    let completed = overridden
        .event_types
        .iter_mut()
        .find(|event_type| event_type.name == "process.completed")
        .expect("completed default");
    completed.semantics.terminal = Some(crate::ProcessTerminalSpec {
        status: crate::ProcessStatus::Completed,
        await_output: Some(crate::ProcessValueSelector::Pointer(
            "/hijacked".to_string(),
        )),
    });
    validate_process_registration(&overridden).expect("core-named override remains valid");
}

#[test]
fn tool_call_registration_refuses_empty_call_id_or_tool_name() {
    for (call_id, tool_name, expected) in [
        (
            "",
            "tool",
            "process `keyless start` tool call must carry a call id",
        ),
        (
            "  ",
            "tool",
            "process `keyless start` tool call must carry a call id",
        ),
        (
            "call",
            "",
            "process `keyless start` tool call must carry a tool name",
        ),
        (
            "call",
            "\t",
            "process `keyless start` tool call must carry a tool name",
        ),
    ] {
        let registration = registration_for_input(ProcessInput::ToolCall {
            call: crate::PreparedToolCall::from_parts(
                call_id,
                crate::ToolId::new("tool-id"),
                tool_name,
                serde_json::json!({}),
                None,
                serde_json::Value::Null,
            ),
        });
        match validate_process_registration(&registration) {
            Err(crate::PluginError::Session(message)) => assert_eq!(message, expected),
            Err(other) => panic!("expected session refusal `{expected}`, got {other:?}"),
            Ok(()) => panic!("expected session refusal `{expected}`, got success"),
        }
    }
}

#[test]
fn persisted_record_without_lifecycle_declarations_accepts_runtime_events() {
    let registration = prepare_process_registration(fixture_registration("pre-upgrade-record"))
        .expect("prepare pre-upgrade fixture");
    assert!(
        registration
            .event_types
            .iter()
            .all(|event_type| !super::is_runtime_lifecycle_event_type(&event_type.name)),
        "runtime lifecycle types must not be persisted as producer declarations"
    );
    let encoded = serde_json::to_vec(&ProcessRecord::from_prepared_registration(
        registration,
        crate::process_id_for_test("pre-upgrade-record"),
        1,
    ))
    .expect("encode pre-upgrade row");
    let mut record: ProcessRecord =
        serde_json::from_slice(&encoded).expect("decode pre-upgrade row");
    let wait = WaitState {
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:pre-upgrade-record:signal.ready:1".to_string(),
            ordinal: 1,
        },
        since_ms: 2,
    };
    let requests = [
        ProcessEventAppendRequest::first_started(
            &record.id,
            &ProcessStarted {
                owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 2,
                build_generation: None,
                generation: None,
            },
            false,
        ),
        ProcessEventAppendRequest::wait_entered(&record.id, &wait),
        ProcessEventAppendRequest::wait_cleared(&record.id, &wait),
        ProcessEventAppendRequest::external_ref_set(
            &record.id,
            &ProcessExternalRef {
                backend: "fixture".to_string(),
                id: "external".to_string(),
                metadata: None,
                segment_ordinal: None,
            },
        ),
        ProcessEventAppendRequest::abandon_requested(
            &record.id,
            &AbandonRequest {
                requested_by: "fixture".to_string(),
                requested_at_ms: 3,
                reason: None,
            },
        ),
    ];
    for (index, request) in requests.into_iter().enumerate() {
        let sequence = index as u64 + 1;
        let plan = prepare_process_event_append(
            &record,
            request,
            sequence,
            (sequence > 1).then_some(sequence - 1),
            None,
            sequence + 10,
            None,
            crate::FleetFormat::current(),
        )
        .expect("runtime-owned lifecycle append must validate");
        let ProcessEventAppendPlan::Insert {
            projected_record, ..
        } = plan
        else {
            panic!("unique lifecycle fixture must insert")
        };
        record = projected_record;
    }
    assert!(record.first_started.is_some());
    assert!(record.wait.is_none());
    assert!(record.external_ref.is_some());
    assert!(record.abandon_request.is_some());
}

#[test]
fn host_signal_replay_key_with_fold_validation_suffix_does_not_panic() {
    let registration = fixture_registration("host-signal-fold-validation-key")
        .with_extra_event_types([crate::ProcessEventType {
            name: "signal.ready".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        }]);
    let record =
        ProcessRecord::from_registration(registration, crate::process_id_for_test("record"));
    let request = ProcessEventAppendRequest::new(
        "signal.ready",
        serde_json::json!({
            "value": "ready",
        }),
    )
    .with_replay_key("host-supplied:fold-validation");

    let plan = prepare_process_event_append(
        &record,
        request,
        1,
        None,
        None,
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect("host-supplied signal replay key should retain the existing append contract");
    assert!(matches!(plan, ProcessEventAppendPlan::Insert { .. }));
}

#[test]
fn every_registration_refusal_rule_has_a_fixture_that_trips_exactly_it() {
    use crate::runtime::{
        ProcessRegistrationRefusal, accepted_process_registration, refused_process_registrations,
    };

    validate_process_registration(&accepted_process_registration())
        .expect("the base fixture must be accepted, or every refusal below proves nothing");

    for rule in ProcessRegistrationRefusal::ALL {
        let fixtures = refused_process_registrations(*rule);
        assert!(
            !fixtures.is_empty(),
            "rule {rule:?} contributes no fixture, so neither validator is proved against it"
        );
        for (index, registration) in fixtures.iter().enumerate() {
            match super::classify_process_registration(registration) {
                Err((refused_rule, error)) => assert_eq!(
                    refused_rule, *rule,
                    "fixture {index} for {rule:?} tripped {refused_rule:?} instead: {error}"
                ),
                Ok(()) => {
                    panic!("fixture {index} for {rule:?} is accepted by core validation")
                }
            }
        }
    }
}
