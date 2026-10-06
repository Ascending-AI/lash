use super::*;

pub(super) fn delivered_with_payload(
    sequence: usize,
    boundary_id: &str,
    actor_alias: &str,
    kind: BoundaryKind,
    payload: serde_json::Value,
    observed: serde_json::Value,
) -> DeliveredBoundary {
    DeliveredBoundary {
        schema: crate::scheduler::BOUNDARY_EVENT_SCHEMA.to_string(),
        sequence,
        scheduler: SchedulerDeliveryEvidence {
            scheduler_controlled: true,
            delivered_at: sequence as u64,
            ..SchedulerDeliveryEvidence::default()
        },
        boundary_id: boundary_id.to_string(),
        actor_alias: actor_alias.to_string(),
        kind,
        at: sequence as u64,
        label: format!("{kind:?}"),
        payload,
        observed,
    }
}

pub(super) fn runtime_completion(
    family: RuntimeCompletionFamily,
    ready_at: u64,
) -> serde_json::Value {
    runtime_completion_registered_after(family, ready_at, "session-001:ingress")
}

pub(super) fn runtime_completion_registered_after(
    family: RuntimeCompletionFamily,
    ready_at: u64,
    registered_after: &str,
) -> serde_json::Value {
    let family_name = serde_json::to_value(family)
        .expect("completion family serializes")
        .as_str()
        .expect("completion family serializes to a string")
        .to_string();
    serde_json::to_value(PendingRuntimeBoundary {
        schema: PENDING_RUNTIME_BOUNDARY_SCHEMA.to_string(),
        pending_id: format!("pending:{family_name}:{ready_at}"),
        boundary_id: format!("session-001:{family_name}:{ready_at}"),
        actor_alias: "session-001".to_string(),
        kind: BoundaryKind::Provider,
        completion_family: family,
        original_scheduled_at: ready_at,
        ready_at,
        registered_after: registered_after.to_string(),
        registered_after_sequence: 0,
        completion_units: vec![RuntimeCompletionUnit::new(
            format!("runtime:{family_name}"),
            ready_at,
        )],
    })
    .expect("pending runtime boundary fixture serializes")
}

pub(super) fn provider_mutation_observed(mutation: &str) -> serde_json::Value {
    json!({
        "mutation": mutation,
        "provider_parser_matrix": {
            "matrix": {
                "real_provider_parser_execution": true,
                "provider_kinds": [
                    "anthropic",
                    "google_oauth",
                    "openai",
                    "openai-compatible"
                ]
            }
        }
    })
}

pub(super) fn provider_event(
    sequence: usize,
    actor: &str,
    turn_boundary_id: &str,
) -> DeliveredBoundary {
    delivered_with_payload(
        sequence,
        &format!("{turn_boundary_id}:event:{sequence}"),
        actor,
        BoundaryKind::ProviderEvent,
        json!({ "turn_boundary_id": turn_boundary_id }),
        json!({}),
    )
}

pub(super) fn provider_completion(
    sequence: usize,
    actor: &str,
    turn_boundary_id: &str,
) -> DeliveredBoundary {
    delivered_with_payload(
        sequence,
        turn_boundary_id,
        actor,
        BoundaryKind::Provider,
        json!({ "provider_kind": "openai" }),
        json!({ "provider_kind": "openai" }),
    )
}

#[test]
fn interleaving_oracle_passes_when_two_sessions_overlap() {
    // Turn A and turn B are both live (each has released a provider event)
    // before either completes.
    let events = vec![
        provider_event(0, "session-a", "turn-a"),
        provider_event(1, "session-b", "turn-b"),
        provider_completion(2, "session-a", "turn-a"),
        provider_completion(3, "session-b", "turn-b"),
    ];
    assert_eq!(peak_concurrent_live_turns(&events), 2);
    assert!(
        provider_turn_interleaving_depth(
            &events,
            &WorkloadExpectations::new(
                vec!["session-a".to_string(), "session-b".to_string()],
                2,
                0,
            )
        )
        .is_passed()
    );
}

#[test]
fn interleaving_oracle_fails_when_multi_session_turns_never_overlap() {
    // Two sessions each run a turn, but each turn completes before the next
    // one releases an event, so peak concurrency is 1.
    let events = vec![
        provider_event(0, "session-a", "turn-a"),
        provider_completion(1, "session-a", "turn-a"),
        provider_event(2, "session-b", "turn-b"),
        provider_completion(3, "session-b", "turn-b"),
    ];
    assert_eq!(peak_concurrent_live_turns(&events), 1);
    let verdict = provider_turn_interleaving_depth(
        &events,
        &WorkloadExpectations::new(vec!["session-a".to_string(), "session-b".to_string()], 2, 0),
    );
    assert!(!verdict.is_passed());
    assert_eq!(verdict.oracle_id, PROVIDER_TURN_INTERLEAVING_ORACLE);
}

#[test]
fn interleaving_oracle_is_exempt_only_for_a_declared_single_session() {
    let events = vec![
        provider_event(0, "session-a", "turn-a"),
        provider_completion(1, "session-a", "turn-a"),
    ];
    assert_eq!(peak_concurrent_live_turns(&events), 1);
    let verdict = provider_turn_interleaving_depth(
        &events,
        &WorkloadExpectations::new(vec!["session-a".to_string()], 1, 0),
    );
    assert!(verdict.is_passed(), "{}", verdict.message);
    assert!(
        verdict
            .message
            .contains("the workload declared 1 session(s)"),
        "the exemption must be proved from the declaration: {}",
        verdict.message
    );
}

pub(super) fn suspend_resume_event(
    suspended_before: bool,
    before: u64,
    after: u64,
) -> DeliveredBoundary {
    delivered_with_payload(
        0,
        "suspend-tool:suspend-resume:001",
        "suspend-tool",
        BoundaryKind::Tool,
        json!({ "suspend_resume": true, "tool": "await_tool", "output": {"ok": true} }),
        json!({
            "session": "suspend-tool",
            "tool_output": {"ok": true},
            "runtime_suspend": {
                "suspend_kind": "tool",
                "turn_suspended_before_completion": suspended_before,
                "scheduler_delivered_completion": true,
                "resolve_accepted": true,
                "resumed_after_completion": after > before,
                "completed_event_count_before_resolution": before,
                "completed_event_count_after_resolution": after,
                "final_assistant_message": "resumed",
            },
        }),
    )
}

#[test]
fn suspend_resume_oracle_passes_when_turn_parked_then_resumed() {
    let events = vec![suspend_resume_event(true, 0, 1)];
    assert!(generated_suspend_resume(&events).is_passed());
}

#[test]
fn suspend_resume_oracle_fails_when_turn_ran_synchronously() {
    // The tool completed before the scheduler delivered the completion
    // boundary: the turn never actually parked.
    let events = vec![suspend_resume_event(false, 1, 1)];
    let verdict = generated_suspend_resume(&events);
    assert!(!verdict.is_passed());
    assert_eq!(verdict.oracle_id, GENERATED_SUSPEND_RESUME_ORACLE);
}

#[test]
fn suspend_resume_oracle_fails_when_the_suspend_class_is_absent() {
    // Anti-vacuity: with no suspend-resume boundary present, the oracle must
    // FAIL rather than pass on an absent class.
    let events = vec![provider_completion(0, "session-a", "turn-a")];
    let verdict = generated_suspend_resume(&events);
    assert!(!verdict.is_passed());
    assert!(verdict.message.contains("class is absent"));
}
