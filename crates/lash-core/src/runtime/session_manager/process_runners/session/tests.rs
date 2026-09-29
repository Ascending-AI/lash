use super::*;

fn recording_factory(
    backend: &crate::Backend,
) -> crate::testing::runtime_helpers::RecordingDeploymentStore {
    crate::testing::runtime_helpers::RecordingDeploymentStore::over(backend.session_store_factory())
}

/// `backend` with its session catalog recorded by `factory`.
fn recording_backend(
    backend: crate::Backend,
    factory: &crate::testing::runtime_helpers::RecordingDeploymentStore,
) -> crate::Backend {
    let factory = factory.clone();
    crate::testing::runtime_helpers::LayeredBackend::over(backend)
        .map_session_store_factory(move |_| Arc::new(factory))
        .into_backend()
}

/// FIG-2975: every non-cancelled child stop reaches the parent as its own
/// failure. The pre-fix runner answered all ten with one code and one
/// sentence, so a parent model could not tell a retryable provider error
/// from a deliberate refusal.
#[test]
fn every_non_cancelled_child_stop_is_distinguishable_to_the_parent() {
    let stops = [
        crate::TurnStop::Incomplete,
        crate::TurnStop::InvalidInput,
        crate::TurnStop::MaxTurns,
        crate::TurnStop::ToolFailure,
        crate::TurnStop::ProviderError,
        crate::TurnStop::ContextOverflow,
        crate::TurnStop::PluginAbort,
        crate::TurnStop::RuntimeError,
        crate::TurnStop::SubmittedError {
            value: serde_json::json!({ "reason": "missing shard amber" }),
        },
        crate::TurnStop::ToolError {
            tool_name: "submit_error".to_string(),
            value: serde_json::json!({
                "class": "execution",
                "code": "subagent_submit_error",
                "message": "missing shard amber",
            }),
        },
    ];

    let mut seen = std::collections::BTreeSet::new();
    for stop in stops {
        let failure = failed_child_failure(stop.clone());
        assert!(
            seen.insert(failure.code.clone()),
            "two stops share the code `{}`: {stop:?}",
            failure.code
        );
        assert_ne!(
            failure.message, "background session turn failed",
            "{stop:?} still collapses onto the shared generic message"
        );
        assert_eq!(failure.source, crate::ToolFailureSource::Tool);
    }
    assert_eq!(seen.len(), 10);
}

/// The child's own terminal words reach the parent verbatim: a parent
/// model reads the child's reason, not a summary of it.
#[test]
fn a_child_authored_stop_reason_reaches_the_parent_verbatim() {
    let submitted = failed_child_failure(crate::TurnStop::SubmittedError {
        value: serde_json::json!({ "reason": "missing shard amber" }),
    });
    assert_eq!(submitted.message, "missing shard amber");
    assert_eq!(submitted.code, "process_session_turn_submitted_error");

    let tool_error = failed_child_failure(crate::TurnStop::ToolError {
        tool_name: "submit_error".to_string(),
        value: serde_json::json!({
            "class": "execution",
            "code": "subagent_submit_error",
            "message": "missing shard amber",
        }),
    });
    assert_eq!(tool_error.message, "missing shard amber");
    assert_eq!(tool_error.code, "process_session_turn_tool_error");
    assert!(
        tool_error
            .raw
            .as_ref()
            .map(crate::ToolValue::to_json_value)
            .is_some_and(|raw| raw["stop"]
                .as_str()
                .is_some_and(|stop| stop.contains("subagent_submit_error"))),
        "the projected stop rides the bounded `raw` channel: {tool_error:?}"
    );
}

/// A stop the child did not author text for carries the assembler's own
/// blocking issue — including its typed kind and code — rather than a
/// sentence with nothing behind it.
#[test]
fn a_category_stop_carries_the_child_turn_blocking_issue() {
    let mut turn = crate::testing::mock_assembled_turn(&SessionId::from("failing-child"), "unused");
    turn.outcome = crate::TurnOutcome::Stopped(crate::TurnStop::ProviderError);
    turn.errors = vec![crate::TurnIssue {
        severity: crate::TurnIssueSeverity::Blocking,
        kind: crate::TurnFailureKind::LlmProvider,
        code: Some(crate::TurnFailureCode::ContextOverflow.into()),
        terminal_reason: None,
        message: "the request exceeded the model's context window".to_string(),
        raw: None,
        retryable: Some(false),
        provider_failure_kind: None,
    }];

    let failure = failure_from_process_turn(&turn);
    assert_eq!(failure.code, "process_session_turn_provider_error");
    assert_eq!(failure.class, crate::ToolFailureClass::External);
    assert_eq!(
        failure.message,
        "background session turn stopped on a provider error: the request exceeded the model's context window"
    );
    let raw = failure
        .raw
        .as_ref()
        .map(crate::ToolValue::to_json_value)
        .expect("typed diagnostics ride `raw`");
    assert_eq!(raw["kind"], serde_json::json!("llm_provider"));
    assert_eq!(raw["code"], serde_json::json!("lash:context_overflow"));
    assert_eq!(raw["retryable"], serde_json::json!(false));
}

/// Run one stopped child turn through the runner's own projection, so the
/// assertions cover the failure a parent, a process record and a terminal
/// process event all read.
fn failed_child_failure(stop: crate::TurnStop) -> crate::ToolFailure {
    let mut turn = crate::testing::mock_assembled_turn(&SessionId::from("failing-child"), "unused");
    turn.outcome = crate::TurnOutcome::Stopped(stop);
    crate::ProcessRegistration::new(
        crate::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        crate::ProcessProvenance::host(),
        crate::Lifetime::Detached,
    );
    let state = process_terminal_state_for_turn(&turn);
    assert_eq!(
        state,
        crate::ProcessStatus::Failed,
        "precondition: the stop must fold to a failed process"
    );
    let output = output_from_process_turn(
        &crate::ProcessId::fixture("failing-child-process"),
        &SessionId::from("failing-child"),
        turn,
        state,
        &crate::SessionTurnOutcome::Turn,
    );
    let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("a failed child turn must project a tool failure");
    };
    failure
}
use crate::runtime::tests::helpers::{
    EmptyTools, host_process_scope, mock_provider, runtime_with_plugins_and_tools_and_host,
};
use std::sync::Arc;

fn test_process_execution_write_authority(
    process_id: impl Into<crate::ProcessId>,
) -> crate::ProcessExecutionWriteAuthority {
    let process_id = process_id.into();
    crate::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        format!("test-process-execution:{process_id}"),
    )
    .bind_attempt(1)
}

/// Durable cutover (FIG-3378): `SessionCreateRequest` rides inside
/// durable `ProcessInput::SessionTurn` rows, and rows recorded before
/// the `snapshot` start was removed carry
/// `{"kind":"snapshot","snapshot":{...}}`. The payload still decodes —
/// `SessionStartPoint` keeps the variant for exactly that — and the run
/// port refuses it with a terminal failure instead of a record-decode
/// error or an endlessly recoverable infra error.
#[tokio::test]
async fn predecessor_snapshot_start_decodes_and_is_refused_terminally() {
    let backend = crate::testing::memory_backend().await;
    let child_session_id = SessionId::from("snapshot-start-child");
    let process_id = crate::ProcessId::fixture("process:subagent:snapshot-start-child");
    let factory = recording_factory(&backend);
    let backend = recording_backend(backend, &factory);
    let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::new(
        backend.clone(),
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    // The refusal lands before any turn runs: no provider call is made.
    let transport = mock_provider(Vec::new());
    let runtime =
        runtime_with_plugins_and_tools_and_host(Vec::new(), Arc::new(EmptyTools), transport, host)
            .await;
    let services = runtime
        .runtime_session_services()
        .expect("runtime session services");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    let create_request = crate::SessionCreateRequest::child_session(
        runtime.session_id(),
        crate::SessionStartPoint::Empty,
        crate::PluginOptions::default(),
    )
    .with_session_id(&child_session_id)
    .with_plugin_source(crate::SessionPluginSource::ParentFork)
    .with_plugin_init(plugin_init);

    // Rewrite the recorded request's `start` to the predecessor durable
    // spelling — what a pre-FIG-3378 `ProcessInput::SessionTurn` row
    // stored.
    let mut recorded = serde_json::to_value(&create_request).expect("serialize recorded request");
    recorded["start"] = serde_json::json!({
        "kind": "snapshot",
        "snapshot": serde_json::to_value(crate::SessionSnapshot::new(
            runtime.state.policy().clone(),
        ))
        .expect("serialize predecessor snapshot"),
    });
    let predecessor_request: crate::SessionCreateRequest =
        serde_json::from_value(recorded).expect("predecessor snapshot payload decodes");
    assert!(matches!(
        predecessor_request.start,
        crate::SessionStartPoint::Snapshot { .. }
    ));

    let output = services
        .run_process_session_turn(
            process_id.clone(),
            test_lineage(&process_id, &predecessor_request),
            predecessor_request,
            crate::TurnInput::text("run"),
            crate::SessionTurnOutcome::Turn,
            test_process_execution_write_authority(process_id.clone()),
            host_process_scope(&runtime.host.core, &process_id),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("the refusal is a typed terminal, not an infra error");
    let failure = match output.into_tool_output().outcome {
        crate::ToolCallOutcome::Failure(failure) => failure,
        outcome => panic!("the snapshot start must terminalize, got {outcome:?}"),
    };
    assert_eq!(failure.code, "process_session_turn_refused");
    assert!(
        failure.message.contains("snapshot"),
        "the refusal names the removed start point: {}",
        failure.message
    );
    assert!(
        matches!(
            crate::store::SessionCatalogStore::lookup_session(&factory, &child_session_id)
                .await
                .expect("inspect refused child"),
            crate::store::SessionLookup::Absent
        ),
        "a refused request creates no session row"
    );
}

#[tokio::test]
async fn child_turn_cancellation_evidence_survives_runner_record_and_parent_result() {
    let child_session_id = crate::SessionId::from("child-turn-cancellation-evidence");
    let registration = crate::ProcessRegistration::new(
        crate::ProcessInput::External {
            metadata: serde_json::json!({"fixture": "child-turn-cancellation-evidence"}),
        },
        crate::ProcessProvenance::host(),
        crate::Lifetime::Detached,
    );
    let evidence = crate::TurnCancellationEvidence {
        request_id: "child-request-17".to_string(),
        origin: Some("opaque-host-origin".to_string()),
        reason: Some("child turn stopped by its host".to_string()),
        undelivered: crate::TurnCancelUndeliveredInputPolicy::Defer,
        mode: crate::TurnCancelMode::Immediate,
        honoured_after_step: None,
    };
    let registry = crate::testing::memory_backend().await.process_registry();
    let process_id = registry
        .register_process(registration)
        .await
        .expect("register child-turn process")
        .id;
    let mut turn = crate::testing::mock_assembled_turn(&child_session_id, "");
    turn.outcome = crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled {
        evidence: evidence.clone(),
    });

    let runner_output = output_from_process_turn(
        &process_id,
        &child_session_id,
        turn,
        crate::ProcessStatus::Cancelled,
        &crate::SessionTurnOutcome::FinalValue { schema: None },
    );
    assert_child_turn_cancellation(&runner_output, &evidence);

    let completion = registry
        .complete_process(
            &process_id,
            crate::ProcessAwaitOutput::from_tool_output(runner_output),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("persist child-turn cancellation");
    let recorded = completion
        .stored()
        .outcome
        .as_ref()
        .expect("terminal process outcome")
        .clone()
        .into_tool_output();
    assert_child_turn_cancellation(&recorded, &evidence);
    let parent_result = completion
        .stored()
        .outcome
        .clone()
        .expect("parent await result")
        .into_tool_output();
    assert_child_turn_cancellation(&parent_result, &evidence);
}

fn assert_child_turn_cancellation(
    output: &crate::ToolCallOutput,
    evidence: &crate::TurnCancellationEvidence,
) {
    let crate::ToolCallOutcome::Cancelled(cancellation) = &output.outcome else {
        panic!("expected child-turn cancellation, got {:?}", output.outcome);
    };
    assert_eq!(cancellation.origin, Some(crate::CancelOrigin::TurnStopped));
    assert_eq!(cancellation.message, evidence.reason.as_deref().unwrap());
    assert_eq!(
        cancellation
            .raw
            .as_ref()
            .map(crate::ToolValue::to_json_value),
        Some(serde_json::to_value(evidence).expect("encode turn cancellation evidence"))
    );
}

/// The lineage the runner derives for `process_id` from its registration: a
/// root process whose own session is the child session it runs.
fn test_lineage(
    process_id: &crate::ProcessId,
    create_request: &crate::SessionCreateRequest,
) -> crate::ProcessLineage {
    let own_session = create_request
        .session_id
        .clone()
        .unwrap_or_else(|| crate::runtime::process_child_session_id(process_id));
    crate::ProcessLineage::of_process(
        process_id,
        &crate::Ancestry::root(),
        None,
        Some(&own_session),
    )
}

/// Project one ended child turn through the runner under `result`.
fn project_turn(
    turn: crate::AssembledTurn,
    result: &crate::SessionTurnOutcome,
) -> crate::ToolCallOutput {
    let state = process_terminal_state_for_turn(&turn);
    output_from_process_turn(
        &crate::ProcessId::fixture("projected-child-process"),
        &SessionId::from("projected-child"),
        turn,
        state,
        result,
    )
}

fn finished_turn(finish: crate::TurnFinish, text: &str) -> crate::AssembledTurn {
    let mut turn = crate::testing::mock_assembled_turn(&SessionId::from("projected-child"), text);
    turn.outcome = crate::TurnOutcome::Finished(finish);
    turn
}

fn projected_failure(output: crate::ToolCallOutput) -> crate::ToolFailure {
    let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("expected a projected failure, got {:?}", output.outcome);
    };
    failure
}

/// ADR 0116 §3.7: under `FinalValue` the SessionTurn runner answers the
/// child's final value, checked against the declared schema, and keeps the
/// child's own failure and cancellation. This is the projection `spawn_agent`
/// answers its caller with, so the tool no longer re-derives it.
#[test]
fn spawn_agent_projects_final_value() {
    let untyped = crate::SessionTurnOutcome::FinalValue { schema: None };

    // Final value, tool value and trimmed assistant text are the value.
    let value = serde_json::json!({ "answer": 42 });
    let output = project_turn(
        finished_turn(
            crate::TurnFinish::FinalValue {
                value: value.clone(),
            },
            "unused",
        ),
        &untyped,
    );
    assert_eq!(output.value_for_projection(), value);
    let output = project_turn(
        finished_turn(
            crate::TurnFinish::ToolValue {
                tool_name: "finish".to_string(),
                value: serde_json::json!(["tool", "value"]),
            },
            "unused",
        ),
        &untyped,
    );
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!(["tool", "value"])
    );
    let output = project_turn(
        finished_turn(
            crate::TurnFinish::AssistantMessage {
                text: "  useful prose  ".to_string(),
            },
            "unused",
        ),
        &untyped,
    );
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!("useful prose")
    );
    let output = project_turn(
        finished_turn(
            crate::TurnFinish::AssistantMessage {
                text: "   ".to_string(),
            },
            "  from the safe text  ",
        ),
        &untyped,
    );
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!("from the safe text"),
        "an empty final message falls back to the assistant's own output"
    );

    // The declared schema is checked: a match passes, a mismatch fails.
    let typed = crate::SessionTurnOutcome::FinalValue {
        schema: Some(serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "integer" } },
            "required": ["answer"],
            "additionalProperties": false
        })),
    };
    let output = project_turn(
        finished_turn(
            crate::TurnFinish::FinalValue {
                value: value.clone(),
            },
            "unused",
        ),
        &typed,
    );
    assert_eq!(output.value_for_projection(), value);
    let failure = projected_failure(project_turn(
        finished_turn(
            crate::TurnFinish::FinalValue {
                value: serde_json::json!({ "answer": "forty-two" }),
            },
            "unused",
        ),
        &typed,
    ));
    assert_eq!(failure.code, "process_session_turn_result_schema_mismatch");
    assert!(
        failure
            .message
            .starts_with("the child's final value did not match the declared output schema:"),
        "{}",
        failure.message
    );

    // A child that ended its task with `submit_error` answers its own reason.
    let mut submitted =
        crate::testing::mock_assembled_turn(&SessionId::from("projected-child"), "unused");
    submitted.outcome = crate::TurnOutcome::Stopped(crate::TurnStop::ToolError {
        tool_name: "submit_error".to_string(),
        value: serde_json::json!({
            "class": "execution",
            "code": "subagent_submit_error",
            "message": "missing shard amber",
        }),
    });
    let failure = projected_failure(project_turn(submitted, &typed));
    assert_eq!(failure.message, "missing shard amber");

    // A frame switch has no final value, even when its metadata would match
    // the schema.
    let mut switched =
        crate::testing::mock_assembled_turn(&SessionId::from("projected-child"), "unused");
    switched.outcome = crate::TurnOutcome::AgentFrameSwitch {
        frame_key: crate::FrameKey::from_caller_material("next-frame").expect("frame key"),
        task: "continue elsewhere".to_string(),
        initial_nodes: Vec::new(),
    };
    let failure = projected_failure(project_turn(switched.clone(), &untyped));
    assert_eq!(failure.code, "process_session_turn_frame_switch");
    // Under `Turn` the same frame switch is the assembled turn, unchanged.
    let turn = project_turn(switched, &crate::SessionTurnOutcome::Turn);
    assert!(turn.value_for_projection().get("turn").is_some());

    // A stopped child never projects a value.
    let mut stopped =
        crate::testing::mock_assembled_turn(&SessionId::from("projected-child"), "unused");
    stopped.outcome = crate::TurnOutcome::Stopped(crate::TurnStop::Incomplete);
    let failure = final_value_of_turn(&stopped).expect_err("a stopped child has no final value");
    assert_eq!(failure.code, "process_session_turn_stopped");

    // Cancellation stays typed.
    let mut cancelled =
        crate::testing::mock_assembled_turn(&SessionId::from("projected-child"), "unused");
    cancelled.outcome = crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled {
        evidence: crate::TurnCancellationEvidence {
            request_id: "cancel-child".to_string(),
            origin: None,
            reason: Some("the parent went away".to_string()),
            undelivered: crate::TurnCancelUndeliveredInputPolicy::Defer,
            mode: crate::TurnCancelMode::Immediate,
            honoured_after_step: None,
        },
    });
    let output = project_turn(cancelled, &typed);
    let crate::ToolCallOutcome::Cancelled(cancellation) = output.outcome else {
        panic!("a cancelled child answers a typed cancellation");
    };
    assert_eq!(cancellation.message, "the parent went away");
    assert_eq!(cancellation.origin, Some(crate::CancelOrigin::TurnStopped));
}
