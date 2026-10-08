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

/// The vocabulary is closed: a kind lash does not name is refused when it
/// is read, before any append is planned.
#[test]
fn effect_summary_refuses_unknown_runtime_kind() {
    let error = crate::ProcessEventAppendRequest::from_stored(
        "process.effect_future",
        serde_json::json!({}),
        "future-effect",
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
        let event_type = request.kind().as_str().to_string();
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
}

#[test]
fn a_resume_cannot_return_an_ended_process_to_running() {
    let mut record = crate::ProcessRecord::from_registration(
        fixture_registration("resume-after-terminal"),
        crate::process_id_for_test("resume-after-terminal"),
    );
    let wait = WaitState {
        since_ms: 1,
        kind: crate::WaitKind::Call {
            call_id: crate::ToolCallId::fixture("resume-call"),
            tool_id: crate::ToolId::from("resume-tool"),
        },
    };
    let process_id = record.id.clone();
    let event = |sequence, request: crate::ProcessEventAppendRequest| crate::ProcessEvent {
        process_id: process_id.clone(),
        sequence,
        fact: request.fact,
        invocation: crate::runtime::causal::process_event_invocation(
            &process_id,
            sequence,
            "fixture",
            request.replay,
        ),
        trace_cause: request.trace_cause,
        occurred_at: sequence,
    };
    let outcome = crate::ProcessTerminal::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!(1),
    ));
    let waiting = event(
        1,
        crate::ProcessEventAppendRequest::wait_entered(&process_id, &wait),
    );
    let completed = event(
        2,
        crate::terminal_append_request(&process_id, &outcome.clone().into(), None),
    );
    let resumed = event(
        3,
        crate::ProcessEventAppendRequest::wait_cleared(&process_id, &wait),
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
fn a_persisted_record_accepts_every_runtime_lifecycle_fact() {
    let registration = prepare_process_registration(fixture_registration("pre-upgrade-record"))
        .expect("prepare pre-upgrade fixture");
    let encoded = serde_json::to_vec(&ProcessRecord::from_prepared_registration(
        registration,
        crate::process_id_for_test("pre-upgrade-record"),
        1,
    ))
    .expect("encode pre-upgrade row");
    let mut record: ProcessRecord =
        serde_json::from_slice(&encoded).expect("decode pre-upgrade row");
    let wait = WaitState {
        kind: WaitKind::Call {
            call_id: crate::ToolCallId::fixture("pre-upgrade-call"),
            tool_id: crate::ToolId::from("pre-upgrade-tool"),
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
            sequence + 10,
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
