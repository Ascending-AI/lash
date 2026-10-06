use super::{
    ProcessEventAppendPlan, prepare_process_event_append, prepare_process_registration,
    validate_process_registration,
};
use crate::runtime::process::{
    ProcessEffectOccurrence, ProcessEffectOmissions, ProcessEffectOmittedCounts,
    ProcessEffectOutcomeClass, validate_generic_process_event_append,
};
use crate::{
    ProcessEventAppendRequest, ProcessExternalRef, ProcessProvenance, ProcessRecord,
    ProcessRegistration, ProcessStarted, WaitKind, WaitState,
};

fn fixture_registration(_label: &str) -> ProcessRegistration {
    crate::testing::held_engine_registration(
        serde_json::Value::Null,
        ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
}

fn effect_summary_request() -> crate::ProcessEventAppendRequest {
    ProcessEffectOccurrence::new(
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
fn effect_summary_refuses_unknown_runtime_kind() {
    let registration =
        fixture_registration("effect-summary-unknown-kind").with_extra_event_types([
            crate::ProcessEventType {
                name: "process.effect_future".to_string(),
                payload_schema: crate::JsonSchema::any(),
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
fn terminal_semantics_cannot_name_a_non_terminal_status() {
    for status in crate::ProcessStatus::ALL {
        let spec = serde_json::json!({
            "status": status.label(),
            "await_output": "payload",
        });
        let decoded = serde_json::from_value::<crate::ProcessTerminalSpec>(spec);
        assert_eq!(
            decoded
                .ok()
                .map(|spec| crate::ProcessStatus::from(spec.status)),
            status.terminal().map(crate::ProcessStatus::from),
            "a terminal event declares `{}` exactly when it is terminal",
            status.label()
        );
    }
}

/// A record holds one lifecycle state, on the stored row and the wire alike:
/// the status is read from it, and a wait, a park, a status or an outcome
/// that contradicts it has no encoding to arrive in.
#[test]
fn a_process_record_decodes_one_lifecycle_state() {
    let record = crate::ProcessRecord::from_registration(
        fixture_registration("one-lifecycle-state"),
        crate::process_id_for_test("one-lifecycle-state"),
    );
    let encoded = serde_json::to_value(&record).expect("encode record");
    for flat in ["status", "wait", "park", "outcome"] {
        assert!(
            encoded.get(flat).is_none(),
            "a record carries no `{flat}` beside its lifecycle"
        );
    }
    assert_eq!(
        encoded["lifecycle"],
        serde_json::json!({"state": "running"})
    );
    let with_lifecycle = |lifecycle: serde_json::Value| {
        let mut encoded = encoded.clone();
        encoded["lifecycle"] = lifecycle;
        serde_json::from_value::<crate::ProcessRecord>(encoded)
    };
    let outcome = serde_json::to_value(crate::ProcessTerminal::from_tool_output(
        crate::ToolCallOutput::success(serde_json::json!(1)),
    ))
    .expect("encode outcome");
    let wait = serde_json::to_value(crate::ProcessLifecycleState::fixture(
        crate::ProcessStatus::Waiting,
    ))
    .expect("encode a waiting state")["wait"]
        .clone();
    assert!(wait.is_object(), "a waiting state carries its wait");

    for status in crate::ProcessStatus::ALL {
        let state = crate::ProcessLifecycleState::fixture(*status);
        assert_eq!(state.status(), *status, "a state derives its status");
        let decoded = with_lifecycle(serde_json::to_value(&state).expect("encode state"))
            .expect("every lifecycle state decodes");
        assert_eq!(decoded.status(), *status);
        assert_eq!(decoded.is_terminal(), decoded.outcome().is_some());
        assert_eq!(
            decoded
                .outcome()
                .and_then(|outcome| outcome.terminal_status()),
            status.terminal()
        );
    }

    for (case, lifecycle) in [
        (
            "a terminal state without an outcome",
            serde_json::json!({"state": "terminal"}),
        ),
        (
            "a running state with an outcome",
            serde_json::json!({"state": "running", "outcome": outcome}),
        ),
        (
            "a terminal state with a wait",
            serde_json::json!({"state": "terminal", "outcome": outcome, "wait": wait}),
        ),
        (
            "a waiting state without a wait",
            serde_json::json!({"state": "waiting"}),
        ),
        (
            "a pruned answer as an outcome",
            serde_json::json!({"state": "terminal", "outcome": {
                "type": "no_longer_retained",
                "terminal_label": "completed",
                "pruned_at_ms": 1,
            }}),
        ),
        (
            "a status beside the state",
            serde_json::json!({"state": "running", "status": "completed"}),
        ),
        (
            "a status in place of a state",
            serde_json::json!({"state": "completed"}),
        ),
    ] {
        assert!(with_lifecycle(lifecycle).is_err(), "{case} must not decode");
    }

    let terminal = serde_json::json!({"outcome": outcome});
    serde_json::from_value::<crate::ProcessTerminalSemantics>(terminal)
        .expect("a stored terminal event carries its outcome");
    assert!(
        serde_json::from_value::<crate::ProcessTerminalSemantics>(
            serde_json::json!({"status": "completed", "outcome": outcome})
        )
        .is_err(),
        "a stored terminal event carries no status beside its outcome"
    );
}

#[test]
fn a_resume_cannot_return_an_ended_process_to_running() {
    let mut record = crate::ProcessRecord::from_registration(
        fixture_registration("resume-after-terminal"),
        crate::process_id_for_test("resume-after-terminal"),
    );
    let wait = WaitState {
        since_ms: 1,
        kind: crate::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: crate::runtime::process_signal_wait_key(&record.id, "ready", 1),
            ordinal: 1,
        },
    };
    let process_id = record.id.clone();
    let event =
        |sequence, request: crate::ProcessEventAppendRequest, terminal| crate::ProcessEvent {
            process_id: process_id.clone(),
            sequence,
            event_type: request.event_type,
            payload: request.payload,
            invocation: crate::runtime::causal::process_event_invocation(
                &process_id,
                sequence,
                "fixture",
                request.replay,
            ),
            semantics: crate::ProcessEventSemantics {
                terminal,
                ..crate::ProcessEventSemantics::default()
            },
            occurred_at: sequence,
        };
    let outcome = crate::ProcessTerminal::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!(1),
    ));
    let waiting = event(
        1,
        crate::ProcessEventAppendRequest::wait_entered(&process_id, &wait),
        None,
    );
    let completed = event(
        2,
        crate::terminal_append_request(&process_id, &outcome.clone().into(), None),
        Some(crate::ProcessTerminalSemantics {
            outcome: outcome.clone(),
        }),
    );
    let resumed = event(
        3,
        crate::ProcessEventAppendRequest::wait_cleared(&process_id, &wait),
        None,
    );
    super::apply_process_event_projection(&mut record, &waiting).expect("enter the wait");
    assert_eq!(record.wait(), Some(&wait));
    super::apply_process_event_projection(&mut record, &completed).expect("end the process");
    let ended = record.clone();
    assert_eq!(ended.status(), crate::ProcessStatus::Completed);
    assert_eq!(ended.wait(), None, "the outcome takes the wait with it");

    let error = super::apply_process_event_projection(&mut record, &resumed)
        .expect_err("a resume cannot take an outcome back");
    assert!(
        matches!(
            error,
            crate::PluginError::ProcessAlreadyTerminal {
                status: crate::ProcessStatus::Completed,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(record, ended, "the refused resume leaves the record ended");
    assert_eq!(record.terminal(), Some(&outcome));
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
                attempt: 1,
                started_at_ms: 2,
                generation: None,
                plugins: None,
            },
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
    ];
    for (index, request) in requests.into_iter().enumerate() {
        let sequence = index as u64 + 1;
        let plan = prepare_process_event_append(
            &record,
            request,
            sequence,
            (sequence > 1).then_some(sequence - 1),
            None,
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
    assert!(record.wait().is_none());
    assert!(record.external_ref.is_some());
}

#[test]
fn typed_signal_id_with_fold_validation_suffix_does_not_panic() {
    let registration = fixture_registration("host-signal-fold-validation-key")
        .with_extra_event_types([crate::ProcessEventType {
            name: "signal.ready".to_string(),
            payload_schema: crate::JsonSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        }]);
    let record =
        ProcessRecord::from_registration(registration, crate::process_id_for_test("record"));
    let request = crate::ProcessSignal::new(
        crate::ProcessSignalIdentity::new(
            record.id.clone(),
            "ready",
            "host-supplied:fold-validation",
        )
        .expect("valid signal identity"),
        serde_json::json!({"value": "ready"}),
    )
    .append_request();

    let plan = prepare_process_event_append(
        &record,
        request,
        1,
        None,
        None,
        Some(0),
        42,
        None,
        crate::FleetFormat::current(),
    )
    .expect("typed signal id with the fold-validation suffix should append");
    assert!(matches!(plan, ProcessEventAppendPlan::Insert { .. }));
}

/// A signal append selects the wait it resolves (FIG-4298): the declared
/// ordinal of the wait the process is parked on for the name, else the
/// signal's position among the events of its type. A store that supplies no
/// count for an unparked signal is refused, never guessed for.
#[test]
fn a_signal_append_selects_its_declared_wait_or_its_position() {
    let registration = fixture_registration("signal-wait-selection").with_extra_event_types([
        crate::ProcessEventType {
            name: "signal.ready".to_string(),
            payload_schema: crate::JsonSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        },
    ]);
    let mut record =
        ProcessRecord::from_registration(registration, crate::process_id_for_test("record"));
    let selected = |record: &ProcessRecord, before: Option<u64>| {
        prepare_process_event_append(
            record,
            crate::ProcessSignal::new(
                crate::ProcessSignalIdentity::new(
                    record.id.clone(),
                    "ready",
                    "signal-wait-selection",
                )
                .expect("valid signal identity"),
                serde_json::json!(1),
            )
            .append_request(),
            4,
            Some(3),
            None,
            before,
            42,
            None,
            crate::FleetFormat::current(),
        )
        .map(|plan| match plan {
            ProcessEventAppendPlan::Insert { event, .. } => event.semantics.signal_wait,
            ProcessEventAppendPlan::Replay { .. } => panic!("a fresh signal inserts"),
        })
    };
    let binding = |ordinal| Some(crate::ProcessSignalWaitBinding { ordinal });

    assert_eq!(selected(&record, Some(2)).expect("unparked"), binding(3));
    assert!(
        selected(&record, None).is_err(),
        "an unparked signal needs the store's count"
    );

    record.lifecycle = crate::ProcessLifecycleState::Waiting {
        wait: WaitState {
            since_ms: 1,
            kind: crate::WaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: crate::runtime::process_signal_wait_key(&record.id, "ready", 7),
                ordinal: 7,
            },
        },
        park: None,
    };
    assert_eq!(selected(&record, Some(2)).expect("parked"), binding(7));
    assert_eq!(selected(&record, None).expect("parked"), binding(7));

    record.lifecycle = crate::ProcessLifecycleState::Waiting {
        wait: WaitState {
            since_ms: 1,
            kind: crate::WaitKind::Signal {
                name: "other".to_string(),
                event_type: "signal.other".to_string(),
                key: crate::runtime::process_signal_wait_key(&record.id, "other", 7),
                ordinal: 7,
            },
        },
        park: None,
    };
    assert_eq!(
        selected(&record, Some(2)).expect("parked elsewhere"),
        binding(3),
        "a wait for another name does not bind this signal"
    );
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
