//! Cross-backend contract for the two arms of a process-event append.

use super::*;
use crate::ProcessEventLogTestSupport as _;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

/// The two arms of a process-event append leave different durable footprints,
/// and every entry point into the append sequence must produce the same one.
///
/// The insert arm writes exactly one event row and advances the wake allocation
/// floor to that event's sequence. The replay arm writes no event row and must
/// leave the floor alone: re-advancing it there would push a later incarnation's
/// sequences past a wake that was already allocated and delivered, for a call
/// that persisted nothing.
///
/// Each backend spells the sequence once and reaches it from three entry points
/// — the unfenced host append, external-owner completion, and workflow-key
/// completion. All three are exercised here. The completion paths
/// settle their repeat call on the already-terminal row rather than the replay
/// arm proper; the observable contract is the same either way, and asserting it
/// per entry point is what catches a floor advance or an event row escaping
/// onto a path that persisted nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_event_append_arms_are_ordered(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    let target_session_id = SessionId::from("append-arm-ordering-target");

    // Entry point 1: the unfenced host append, which reaches the replay arm
    // proper through a repeated replay key.
    let host_id = registry
        .register_process(
            registration("append-arm-host")
                .with_extra_event_types([wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.clone())),
        )
        .await
        .expect("register host-append arm process")
        .id;
    let host_request = || {
        ProcessEventAppendRequest::new(
            "producer.wake",
            serde_json::json!({"wake_input": "append arm ordering"}),
        )
        .with_replay_key("append-arm-host:wake:1")
    };
    let baseline = append_arm_footprint(&registry, &host_id, &target_session_id).await;
    assert_eq!(
        baseline,
        (0, None),
        "a registered process has no events and no sender floor yet"
    );
    let inserted = registry
        .append_event(&host_id, host_request())
        .await
        .expect("host append takes the insert arm");
    assert_eq!(inserted.last_event_sequence, inserted.event.sequence);
    assert_eq!(
        registry
            .get_process(&host_id)
            .await
            .expect("read inserted projection")
            .expect("inserted process")
            .last_event_sequence,
        inserted.event.sequence,
        "the record fold and append receipt must carry the inserted event sequence"
    );
    assert_eq!(
        append_arm_footprint(&registry, &host_id, &target_session_id).await,
        (1, Some(inserted.event.sequence)),
        "the insert arm writes one event row and advances the floor to it"
    );
    let later = registry
        .append_event(
            &host_id,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "later append"}),
            )
            .with_replay_key("append-arm-host:wake:2"),
        )
        .await
        .expect("a later host append takes the insert arm");
    let replayed = registry
        .append_event(&host_id, host_request())
        .await
        .expect("host append takes the replay arm");
    assert_eq!(replayed.event.sequence, inserted.event.sequence);
    assert_eq!(
        replayed.last_event_sequence, later.event.sequence,
        "a replay receipt reports the process fold position, not the older replayed event"
    );
    assert_eq!(
        append_arm_footprint(&registry, &host_id, &target_session_id).await,
        (2, Some(later.event.sequence)),
        "the replay arm writes no event row and leaves the floor where the latest insert put it"
    );

    // Entry point 2: terminal completion under an explicit authority.
    let authority_id = registry
        .register_process(
            registration("append-arm-authority-completion")
                .with_wake_session_id(Some(target_session_id.clone())),
        )
        .await
        .expect("register authority-completion arm process")
        .id;
    let authority_output = ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!({"append_arm": "authority"}),
    ));
    assert!(matches!(
        registry
            .complete_process(
                &authority_id,
                authority_output.clone(),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("authority completion takes the insert arm"),
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    let authority_footprint =
        append_arm_footprint(&registry, &authority_id, &target_session_id).await;
    assert_eq!(
        authority_footprint.0, 1,
        "authority completion writes exactly one terminal event row"
    );
    assert_eq!(
        authority_footprint.1,
        Some(terminal_sequence(&registry, &authority_id).await),
        "authority completion advances the floor to its terminal event"
    );
    assert!(matches!(
        registry
            .complete_process(
                &authority_id,
                authority_output,
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("authority completion is idempotent"),
        crate::ProcessCompletionOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(
        append_arm_footprint(&registry, &authority_id, &target_session_id).await,
        authority_footprint,
        "a repeated authority completion writes no event row and does not move the floor"
    );

    // Entry point 3: workflow-key completion.
    let workflow_id = registry
        .register_process(
            executed_registration("append-arm-workflow-key-completion")
                .with_wake_session_id(Some(target_session_id.clone())),
        )
        .await
        .expect("register workflow-key completion arm process")
        .id;
    let workflow_output = ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!({"append_arm": "workflow_key"}),
    ));
    assert!(matches!(
        registry
            .complete_process(
                &workflow_id,
                workflow_output.clone(),
                ProcessCompletionAuthority::workflow_key(workflow_id.to_string()),
            )
            .await
            .expect("workflow-key completion takes the insert arm"),
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    let workflow_footprint =
        append_arm_footprint(&registry, &workflow_id, &target_session_id).await;
    assert_eq!(
        workflow_footprint.0, 1,
        "workflow-key completion writes exactly one terminal event row"
    );
    assert_eq!(
        workflow_footprint.1,
        Some(terminal_sequence(&registry, &workflow_id).await),
        "workflow-key completion advances the floor to its terminal event"
    );
    assert!(matches!(
        registry
            .complete_process(
                &workflow_id,
                workflow_output,
                ProcessCompletionAuthority::workflow_key(workflow_id.to_string()),
            )
            .await
            .expect("workflow-key completion is idempotent"),
        crate::ProcessCompletionOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(
        append_arm_footprint(&registry, &workflow_id, &target_session_id).await,
        workflow_footprint,
        "a repeated workflow-key completion writes no event row and does not move the floor"
    );

    durable_effect_outcome_event_crash_windows(registry).await;
}

/// The durable footprint of a process's appends: how many event rows exist, and
/// where the sender floor for `target_session_id` stands.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn append_arm_footprint(
    registry: &Arc<dyn crate::ConformanceProcessRegistry>,
    process_id: &ProcessId,
    target_session_id: &SessionId,
) -> (usize, Option<u64>) {
    let events = registry
        .full_event_window(process_id, 0)
        .await
        .expect("read append-arm event rows")
        .len();
    let floor = registry
        .wake_allocation_floor_for_testing(target_session_id, process_id)
        .await
        .expect("read append-arm sender floor");
    (events, floor)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn terminal_sequence(
    registry: &Arc<dyn crate::ConformanceProcessRegistry>,
    process_id: &ProcessId,
) -> u64 {
    registry
        .full_event_window(process_id, 0)
        .await
        .expect("read append-arm event rows")
        .last()
        .expect("a completed process has a terminal event")
        .sequence
}

/// The runtime's effect-summary appends go through execution authority only.
/// A host append of the kind is refused; a lost acknowledgement recovers the
/// original event; a changed payload under the same effect key is refused;
/// and a redrive that reaches the append after the run terminalised the
/// process recovers it too.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn durable_effect_outcome_event_crash_windows(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    let process_id = registry
        .register_process(super::process_registry::executed_registration(
            "durable-effect-outcome-crash-windows",
        ))
        .await
        .expect("register effect-summary process")
        .id;
    let authority =
        crate::ProcessExecutionWriteAuthority::invocation(process_id.clone(), "effect-worker:1")
            .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &process_id,
            authority
                .invocation_started()
                .expect("a bound invocation names its execution"),
            &authority,
        )
        .await
        .expect("record the invocation's execution start");
    let recorded = lash_core::ProcessEffectSummaryOccurrence::new(
        "node:tool",
        1,
        "tool:fixture",
        lash_core::ProcessEffectOutcomeClass::Failure,
        Some(lash_sansio::FailureCode::from_foreign_wire(
            "fixture:refused",
        )),
        "lashlang:recorded-effect:1",
        lash_core::FleetFormat::current(),
    );

    assert!(
        matches!(
            registry
                .append_event(&process_id, recorded.append_request())
                .await,
            Err(crate::PluginError::ReservedProcessEvent { .. })
        ),
        "a host append of the runtime-owned kind is refused"
    );

    let inserted = registry
        .append_event_with_authority(&process_id, recorded.append_request(), &authority)
        .await
        .expect("incorporate the recorded failure");
    assert_eq!(inserted.event.sequence, 2);
    assert_eq!(
        inserted.event.event_type,
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE
    );

    // The acknowledgement is lost; the redrive recovers the original append.
    let replayed = registry
        .append_event_with_authority(&process_id, recorded.append_request(), &authority)
        .await
        .expect("recover the append after a lost acknowledgement");
    assert_eq!(replayed.event.sequence, inserted.event.sequence);
    assert_eq!(replayed.event.payload, inserted.event.payload);

    let mut changed = recorded.clone();
    changed.code = Some(lash_sansio::FailureCode::from_foreign_wire(
        "fixture:changed",
    ));
    let error = registry
        .append_event_with_authority(&process_id, changed.append_request(), &authority)
        .await
        .expect_err("a changed outcome under the same effect key is refused");
    assert!(
        error
            .to_string()
            .contains("conflicts with an existing event"),
        "{error}"
    );
    assert_eq!(
        registry
            .full_event_window(&process_id, 0)
            .await
            .expect("read events")
            .len(),
        2
    );

    // A durable substrate may replay the invocation that already completed
    // the process, reaching the append again after terminalisation.
    let invoked_id = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "conformance-effect-engine".to_string(),
                    payload: serde_json::Value::Null,
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new(
                "conformance-effect-env",
            )))
            .with_admitted_identity(crate::AdmittedProcessIdentity::for_testing(
                ProcessIdentity::for_definition(
                    crate::ProcessDefinitionRef::unclaimed(
                        "conformance-effect-engine",
                        serde_json::Value::Null,
                    ),
                    Some("durable-effect-outcome-terminal-redrive"),
                ),
            )),
        )
        .await
        .expect("register invocation-owned effect-summary process")
        .id;
    let invocation = crate::ProcessExecutionWriteAuthority::invocation(
        invoked_id.clone(),
        "effect-summary-invocation",
    )
    .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &invoked_id,
            invocation
                .invocation_started()
                .expect("a bound invocation names its execution"),
            &invocation,
        )
        .await
        .expect("record the invocation's execution start");
    let invoked = registry
        .append_event_with_authority(&invoked_id, recorded.append_request(), &invocation)
        .await
        .expect("incorporate under invocation authority");
    registry
        .complete_process(
            &invoked_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key(invoked_id.as_str()),
        )
        .await
        .expect("terminalise the process");
    let terminal_replay = registry
        .append_event_with_authority(&invoked_id, recorded.append_request(), &invocation)
        .await
        .expect("a redrive after terminalisation recovers the append");
    assert_eq!(terminal_replay.event.sequence, invoked.event.sequence);
    assert_eq!(terminal_replay.event.payload, invoked.event.payload);
    registry
        .append_event_with_authority(&invoked_id, changed.append_request(), &invocation)
        .await
        .expect_err("a changed outcome after terminalisation is refused");
    let events = registry
        .full_event_window(&invoked_id, 0)
        .await
        .expect("read events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
            .count(),
        1,
        "exactly one effect outcome survives every redrive: {events:?}"
    );
}
