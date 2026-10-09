use super::{
    ProcessEventAppendPlan, prepare_process_event_append, prepare_process_registration,
    validate_process_registration,
};
use crate::runtime::process::{
    ProcessEffectOccurrence, ProcessEffectOmissions, ProcessEffectOmittedCounts,
    ProcessEffectOutcomeClass,
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

/// Plan `request` as a store does: the canonical preparation, then the plan.
fn plan_append(
    record: &ProcessRecord,
    request: ProcessEventAppendRequest,
    sequence: u64,
    last_event_sequence: Option<u64>,
    replay_lookup: Option<crate::ProcessEvent>,
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

fn effect_summary_request() -> crate::ProcessEventAppendRequest {
    ProcessEffectOccurrence::new(
        "resource_operation:node",
        1,
        "fixture.operation",
        ProcessEffectOutcomeClass::Failure,
        Some(crate::FailureCode::from_foreign_wire("fixture:failed")),
        "lash_vm:scope:resource:17:fixture.operation:23:resource_operation:node:1",
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
        crate::FleetFormat::current(),
    )
    .expect_err("an unknown runtime-owned effect kind must be refused");
    assert!(matches!(
        error,
        crate::PluginError::UnknownProcessEventKind { .. }
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

    let error = plan_append(&record, request, 1, None, None, 42)
        .expect_err("the payload may not claim a different effect");
    assert!(
        error
            .to_string()
            .contains("payload replay_key must equal the append replay key"),
        "{error}"
    );
}

/// A stored effect fact is read through its versioned reader: a payload
/// outside the fleet's read window is refused when the event is read, not
/// admitted as whatever its fields happen to parse to (FIG-5509).
#[test]
fn a_stored_effect_fact_is_read_inside_the_fleet_read_window() {
    let fleet = crate::FleetFormat::current();
    let mut counts = ProcessEffectOmittedCounts::default();
    counts.record(ProcessEffectOutcomeClass::Success);
    for request in [
        effect_summary_request(),
        ProcessEffectOmissions::new(
            std::collections::BTreeMap::from([("node".to_string(), counts)]),
            fleet,
        )
        .append_request("omissions"),
    ] {
        let event_type = request.fact.event_type();
        let stored = request.fact.payload();
        assert_eq!(
            crate::ProcessLifecycleFact::decode(event_type, stored.clone(), fleet)
                .expect("a payload this fleet writes is read back"),
            request.fact
        );
        let mut future = stored;
        future["vocabulary_version"] = serde_json::json!(u32::MAX);
        let error = crate::ProcessLifecycleFact::decode(event_type, future, fleet)
            .expect_err("a payload outside the read window is refused");
        assert!(
            error.to_string().contains("vocabulary version"),
            "`{event_type}`: {error}"
        );
    }
}

/// The effect vocabulary's version stamp names an encoding, not the fact: the
/// occurrence a newer reader lifted is the one the older writer appended, and
/// any other difference is another fact (FIG-5509).
#[test]
fn an_effect_fact_replays_across_its_vocabulary_stamp() {
    let request = effect_summary_request();
    let crate::ProcessLifecycleFact::EffectOutcome(written) = &request.fact else {
        panic!("the fixture appends an effect outcome");
    };
    let mut lifted = written.clone();
    lifted.vocabulary_version += 1;
    assert!(
        request
            .fact
            .same_fact(&crate::ProcessLifecycleFact::EffectOutcome(lifted))
    );
    let mut changed = written.clone();
    changed.code = Some(crate::FailureCode::from_foreign_wire("fixture:changed"));
    assert!(
        !request
            .fact
            .same_fact(&crate::ProcessLifecycleFact::EffectOutcome(changed))
    );
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
    for flat in ["status", "wait", "waits", "park", "outcome"] {
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
    let waits = serde_json::to_value(crate::ProcessLifecycleState::fixture(
        crate::ProcessStatus::Waiting,
    ))
    .expect("encode a waiting state")["waits"]
        .clone();
    assert!(
        waits.as_array().is_some_and(|waits| !waits.is_empty()),
        "a waiting state carries its waits"
    );

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
            "a terminal state with waits",
            serde_json::json!({"state": "terminal", "outcome": outcome, "waits": waits}),
        ),
        (
            "a waiting state without its waits",
            serde_json::json!({"state": "waiting"}),
        ),
        (
            "a waiting state that waits on nothing",
            serde_json::json!({"state": "waiting", "waits": []}),
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

/// A process blocked on several things lists each, and the end of one wait
/// leaves the others: it reads running only once none is left (FIG-5553).
#[test]
fn a_resume_ends_only_its_own_wait() {
    let mut record = crate::ProcessRecord::from_registration(
        fixture_registration("two-waits"),
        crate::process_id_for_test("two-waits"),
    );
    let process_id = record.id.clone();
    let sleep = WaitState {
        since_ms: 1,
        kind: WaitKind::Sleep { until_ms: 900 },
        site: Some(crate::StepEffectSite {
            node_id: "nap".to_owned(),
            occurrence: 1,
            context: Default::default(),
        }),
    };
    let child = WaitState {
        since_ms: 1,
        kind: WaitKind::Process {
            process_id: crate::process_id_for_test("awaited"),
        },
        site: None,
    };
    let mut sequence = 0;
    let mut apply = |record: &mut crate::ProcessRecord,
                     request: crate::ProcessEventAppendRequest| {
        sequence += 1;
        let event = crate::ProcessEvent {
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
        super::apply_process_event_projection(record, &event).expect("fold the wait fact");
    };
    apply(
        &mut record,
        crate::ProcessEventAppendRequest::wait_entered(&process_id, &sleep),
    );
    apply(
        &mut record,
        crate::ProcessEventAppendRequest::wait_entered(&process_id, &child),
    );
    assert_eq!(record.waits(), [sleep.clone(), child.clone()]);
    apply(
        &mut record,
        crate::ProcessEventAppendRequest::wait_cleared(&process_id, &sleep),
    );
    assert_eq!(record.status(), crate::ProcessStatus::Waiting);
    assert_eq!(record.waits(), std::slice::from_ref(&child));
    apply(
        &mut record,
        crate::ProcessEventAppendRequest::wait_cleared(&process_id, &child),
    );
    assert_eq!(record.status(), crate::ProcessStatus::Running);
    assert!(record.waits().is_empty());
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
        site: None,
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
    assert_eq!(record.waits(), std::slice::from_ref(&wait));
    super::apply_process_event_projection(&mut record, &completed).expect("end the process");
    let ended = record.clone();
    assert_eq!(ended.status(), crate::ProcessStatus::Completed);
    assert!(
        ended.waits().is_empty(),
        "the outcome takes the wait with it"
    );

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
        site: None,
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
        let plan = plan_append(
            &record,
            request,
            sequence,
            (sequence > 1).then_some(sequence - 1),
            None,
            sequence + 10,
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
    assert!(record.waits().is_empty());
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

/// FIG-5644: one typed blocker has one entry even on stored decode.
#[test]
fn a_waiting_state_refuses_duplicate_blockers() {
    let wait = WaitState {
        kind: WaitKind::Sleep { until_ms: 900 },
        since_ms: 1,
        site: None,
    };
    let encoded = serde_json::json!({
        "state": "waiting",
        "waits": [wait.clone(), WaitState { since_ms: 2, ..wait }],
    });
    assert!(serde_json::from_value::<crate::ProcessLifecycleState>(encoded).is_err());
}

/// FIG-5644: construction and folding keep one entry per typed wait kind.
#[test]
fn waiting_construction_and_fold_keep_one_entry_per_kind() {
    let first = WaitState {
        kind: WaitKind::Sleep { until_ms: -1 },
        since_ms: -2,
        site: None,
    };
    let replacement = WaitState {
        since_ms: -1,
        ..first.clone()
    };
    assert!(crate::ProcessWaits::try_from(Vec::new()).is_err());
    let waiting = crate::ProcessLifecycleState::Waiting {
        waits: crate::ProcessWaits::new(first),
    }
    .entering(&replacement);
    assert_eq!(waiting.waits(), std::slice::from_ref(&replacement));
    let decoded: crate::ProcessLifecycleState =
        serde_json::from_value(serde_json::to_value(&waiting).expect("encode waiting state"))
            .expect("signed store milliseconds decode for both since and until");
    assert_eq!(decoded, waiting);
    assert_eq!(
        waiting.leaving(&replacement),
        crate::ProcessLifecycleState::running()
    );
}
