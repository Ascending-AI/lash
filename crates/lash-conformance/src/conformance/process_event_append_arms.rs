//! Cross-backend contract for the two arms of a process-event append.

use super::*;
use crate::ProcessEventLogTestSupport as _;
use lash_sansio::ProcessId;
use pretty_assertions::assert_eq;

/// The two arms of a process-event append leave different durable footprints,
/// and every entry point into the append sequence must produce the same one.
///
/// The insert arm writes exactly one event row. The replay arm writes none:
/// it persists nothing, so nothing a later read sees may move.
///
/// Each backend spells the sequence once and reaches it from two entry points
/// — a runner's authority-fenced append and workflow-key completion. Both are exercised
/// here. The completion path settles its repeat call on the already-terminal
/// row rather than the replay arm proper; the observable contract is the same
/// either way, and asserting it per entry point is what catches an event row
/// escaping onto a path that persisted nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_event_append_arms_are_ordered(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    // Entry point 1: a runner's append, which reaches the replay arm proper
    // through a repeated replay key.
    let host_id = registry
        .register_process(registration("append-arm-host"))
        .await
        .expect("register host-append arm process")
        .id;
    let runner = start_runner(registry.as_ref(), &host_id).await;
    let host_request = || {
        call_wait_event(
            &host_id,
            "lifecycle.wait",
            "lifecycle.wait",
            serde_json::json!({"call_label": "append arm ordering"}),
        )
        .with_replay_key("append-arm-host:wait:1")
    };
    let baseline = append_arm_footprint(&registry, &host_id).await;
    assert_eq!(baseline, 1, "a started process holds its start event");
    let inserted = registry
        .append_event_with_authority(&host_id, host_request(), &runner)
        .await
        .expect("the runner's append takes the insert arm");
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
        append_arm_footprint(&registry, &host_id).await,
        baseline + 1,
        "the insert arm writes one event row"
    );
    let later = registry
        .append_event_with_authority(
            &host_id,
            call_wait_event(
                &host_id,
                "lifecycle.wait",
                "lifecycle.wait",
                serde_json::json!({"call_label": "later append"}),
            )
            .with_replay_key("append-arm-host:wait:2"),
            &runner,
        )
        .await
        .expect("a later append takes the insert arm");
    let replayed = registry
        .append_event_with_authority(&host_id, host_request(), &runner)
        .await
        .expect("the runner's append takes the replay arm");
    assert_eq!(replayed.event.sequence, inserted.event.sequence);
    assert_eq!(
        replayed.last_event_sequence, later.event.sequence,
        "a replay receipt reports the process fold position, not the older replayed event"
    );
    assert_eq!(
        append_arm_footprint(&registry, &host_id).await,
        baseline + 2,
        "the replay arm writes no event row"
    );

    // Entry point 2: workflow-key completion.
    let workflow_id = registry
        .register_process(executed_registration("append-arm-workflow-key-completion"))
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
    let workflow_footprint = append_arm_footprint(&registry, &workflow_id).await;
    assert_eq!(
        workflow_footprint, 1,
        "workflow-key completion writes exactly one terminal event row"
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
        append_arm_footprint(&registry, &workflow_id).await,
        workflow_footprint,
        "a repeated workflow-key completion writes no event row"
    );

    durable_effect_outcome_event_crash_windows(registry).await;
}

/// The durable footprint of a process's appends: how many event rows exist.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn append_arm_footprint(
    registry: &Arc<dyn crate::ConformanceProcessRegistry>,
    process_id: &ProcessId,
) -> usize {
    registry
        .full_event_window(process_id, 0)
        .await
        .expect("read append-arm event rows")
        .len()
}

/// The runtime's effect-summary appends go through execution authority.
/// A lost acknowledgement recovers the
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
    let recorded = lash_core::ProcessEffectOccurrence::new(
        lash_sansio::effect_identity_fixture("node:tool", 1),
        "tool:fixture",
        lash_core::ProcessEffectOutcomeClass::Failure,
        Some(lash_sansio::FailureCode::from_foreign_wire(
            "fixture:refused",
        )),
        "lash_vm:recorded-effect:1",
        lash_core::FleetFormat::current(),
    );

    let inserted = registry
        .append_event_with_authority(&process_id, recorded.append_request(), &authority)
        .await
        .expect("incorporate the recorded failure");
    assert_eq!(inserted.event.sequence, 2);
    assert_eq!(
        inserted.event.fact.event_type(),
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE
    );

    // The acknowledgement is lost; the redrive recovers the original append.
    let replayed = registry
        .append_event_with_authority(&process_id, recorded.append_request(), &authority)
        .await
        .expect("recover the append after a lost acknowledgement");
    assert_eq!(replayed.event.sequence, inserted.event.sequence);
    assert_eq!(replayed.event.fact.payload(), inserted.event.fact.payload());

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
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref()))
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
    assert_eq!(
        terminal_replay.event.fact.payload(),
        invoked.event.fact.payload()
    );
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
            .filter(|event| event.fact.event_type() == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
            .count(),
        1,
        "exactly one effect outcome survives every redrive: {events:?}"
    );
}
