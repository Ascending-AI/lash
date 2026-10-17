use super::*;

fn recording_factory(
    backend: &lash_execution_contracts::backend::Backend,
) -> crate::testing::runtime_helpers::RecordingDeploymentStore {
    crate::testing::runtime_helpers::RecordingDeploymentStore::over(backend.session_store_factory())
}

/// `backend` with its session catalog recorded by `factory`.
fn recording_backend(
    backend: lash_execution_contracts::backend::Backend,
    factory: &crate::testing::runtime_helpers::RecordingDeploymentStore,
) -> lash_execution_contracts::backend::Backend {
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
        lash_protocol_types::session_model::TurnStop::Incomplete,
        lash_protocol_types::session_model::TurnStop::InvalidInput,
        lash_protocol_types::session_model::TurnStop::MaxTurns,
        lash_protocol_types::session_model::TurnStop::ToolFailure,
        lash_protocol_types::session_model::TurnStop::ProviderError {
            cause: lash_protocol_types::TurnErrorCause::new(
                lash_protocol_types::TurnErrorCode::ProviderRejected,
            ),
        },
        lash_protocol_types::session_model::TurnStop::ContextOverflow,
        lash_protocol_types::session_model::TurnStop::PluginAbort,
        lash_protocol_types::session_model::TurnStop::RuntimeError {
            cause: lash_protocol_types::TurnErrorCause::new(
                lash_protocol_types::TurnErrorCode::RuntimeRefused,
            ),
        },
        lash_protocol_types::session_model::TurnStop::AgentFrameSwitchLimit,
        lash_protocol_types::session_model::TurnStop::SubmittedError {
            value: serde_json::json!({ "reason": "missing shard amber" }),
        },
        lash_protocol_types::session_model::TurnStop::ToolError {
            tool_name: "submit_error".to_string(),
            value: serde_json::json!({
                "class": "execution",
                "code": "child_submit_error",
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
        assert_eq!(
            failure.source,
            lash_protocol_types::tool_output::ToolFailureSource::Tool
        );
    }
    assert_eq!(seen.len(), 11);
}

/// The child's own terminal words reach the parent verbatim: a parent
/// model reads the child's reason, not a summary of it.
#[test]
fn a_child_authored_stop_reason_reaches_the_parent_verbatim() {
    let submitted = failed_child_failure(
        lash_protocol_types::session_model::TurnStop::SubmittedError {
            value: serde_json::json!({ "reason": "missing shard amber" }),
        },
    );
    assert_eq!(submitted.message, "missing shard amber");
    assert_eq!(submitted.code, "process_session_turn_submitted_error");

    let tool_error =
        failed_child_failure(lash_protocol_types::session_model::TurnStop::ToolError {
            tool_name: "submit_error".to_string(),
            value: serde_json::json!({
                "class": "execution",
                "code": "child_submit_error",
                "message": "missing shard amber",
            }),
        });
    assert_eq!(tool_error.message, "missing shard amber");
    assert_eq!(tool_error.code, "process_session_turn_tool_error");
    assert!(
        tool_error
            .raw
            .as_ref()
            .map(lash_protocol_types::tool_output::ToolValue::to_json_value)
            .is_some_and(|raw| raw["stop"]
                .as_str()
                .is_some_and(|stop| stop.contains("child_submit_error"))),
        "the projected stop rides the bounded `raw` channel: {tool_error:?}"
    );
}

/// Run one stopped child turn through the runner's own projection, so the
/// assertions cover the failure a parent, a process record and a terminal
/// process event all read.
fn failed_child_failure(
    stop: lash_protocol_types::session_model::TurnStop,
) -> lash_protocol_types::tool_output::ToolFailure {
    let turn = lash_protocol_types::session_model::TurnOutcome::Stopped(stop);
    lash_execution_contracts::testing::held_engine_registration(
        serde_json::Value::Null,
        lash_execution_contracts::process::ProcessProvenance::host(),
        lash_execution_contracts::process::Lifetime::Detached,
    );
    let state = process_terminal_state_for_turn(&turn);
    assert_eq!(
        state,
        lash_execution_contracts::process::ProcessStatus::Failed,
        "precondition: the stop must fold to a failed process"
    );
    let output = output_from_process_turn(
        &lash_protocol_types::ProcessId::fixture("failing-child-process"),
        &SessionId::from("failing-child"),
        &lash_protocol_types::TurnId::fixture("projected-child-turn"),
        turn,
        &lash_execution_contracts::process::SessionTurnOutcome::Turn,
        lash_core_store::execution_limits::DiagnosticLimits::standard(),
        lash_protocol_types::session_model::RuntimeOutputCuts::standard(),
    );
    let lash_protocol_types::tool_output::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("a failed child turn must project a tool failure");
    };
    failure
}
use crate::runtime::tests::helpers::{
    EmptyTools, mock_provider, runtime_with_plugins_and_tools_and_host,
};
use std::sync::Arc;

/// Durable cutover (FIG-3378): `SessionCreateRequest` rides inside
/// durable `ProcessInput::SessionTurn` rows, and rows recorded before
/// the `snapshot` start was removed carry
/// `{"kind":"snapshot","snapshot":{...}}`. The payload still decodes —
/// `SessionStartPoint` keeps the variant for exactly that — and the run
/// port refuses it with a terminal failure instead of a record-decode
/// error or an endlessly recoverable infra error, before any turn is mailed.
#[tokio::test]
async fn predecessor_snapshot_start_decodes_and_is_refused_terminally() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let child_session_id = SessionId::from("snapshot-start-child");
    let process_id = lash_protocol_types::ProcessId::fixture("process:child:snapshot-start-child");
    let factory = recording_factory(&backend);
    let backend = recording_backend(backend, &factory);
    let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::new(
        backend.clone(),
        lash_core_store::store::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
        lash_protocol_types::ExecutionBudgets::recommended(),
        crate::runtime::DeltaCoalescing::recommended(),
        crate::DataRetentionConfig::standard(),
    ));
    // The refusal lands before any turn runs: no provider call is made.
    let transport = mock_provider(Vec::new());
    let runtime =
        runtime_with_plugins_and_tools_and_host(Vec::new(), Arc::new(EmptyTools), transport, host)
            .await;
    let services = runtime
        .runtime_session_services()
        .expect("runtime session services");
    let create_request =
        lash_execution_contracts::session_types::SessionCreateRequest::child_session(
            runtime.session_id(),
            lash_core_store::session_identity::SessionStartPoint::Empty,
            lash_core_store::execution_state::PluginOptions::default(),
        )
        .with_session_id(&child_session_id);

    // Rewrite the recorded request's `start` to the predecessor durable
    // spelling — what a pre-FIG-3378 `ProcessInput::SessionTurn` row
    // stored.
    let mut recorded = serde_json::to_value(&create_request).expect("serialize recorded request");
    recorded["start"] = serde_json::json!({
        "kind": "snapshot",
        "snapshot": serde_json::to_value(lash_core_store::session_identity::SessionSnapshot::new(SessionId::from("session"),
            runtime.state.policy().clone(),
        ))
        .expect("serialize predecessor snapshot"),
    });
    let predecessor_request: lash_execution_contracts::session_types::SessionCreateRequest =
        serde_json::from_value(recorded).expect("predecessor snapshot payload decodes");
    assert!(matches!(
        predecessor_request.start,
        lash_core_store::session_identity::SessionStartPoint::Snapshot { .. }
    ));

    let mailed = services
        .mail_process_session_turn(
            &process_id,
            predecessor_request,
            lash_core_store::turn_input_vocabulary::TurnInput::text("run"),
            None,
        )
        .await
        .expect("the refusal is a typed terminal, not an infra error");
    let lash_core_execution::runtime::actor::process::SessionTurnMail::Refused(output) = mailed
    else {
        panic!("the snapshot start must terminalize, got {mailed:?}");
    };
    let failure = match output.into_tool_output().outcome {
        lash_protocol_types::tool_output::ToolCallOutcome::Failure(failure) => failure,
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
    let child_session_id = lash_protocol_types::SessionId::from("child-turn-cancellation-evidence");
    let registration = lash_execution_contracts::testing::held_engine_registration(
        serde_json::json!({"fixture": "child-turn-cancellation-evidence"}),
        lash_execution_contracts::process::ProcessProvenance::host(),
        lash_execution_contracts::process::Lifetime::Detached,
    );
    let evidence = lash_protocol_types::session_model::TurnCancellationEvidence {
        request_id: "child-request-17".to_string(),
        origin: Some("opaque-host-origin".to_string()),
        reason: Some("child turn stopped by its host".to_string()),
        undelivered: lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Defer,
        mode: lash_protocol_types::session_model::TurnCancelMode::Immediate,
        honoured_after_step: None,
    };
    let registry = crate::testing::sqlite_recording_backend()
        .await
        .process_registry();
    let process_id = registry
        .register_process(registration)
        .await
        .expect("register child-turn process")
        .id;
    let turn = lash_protocol_types::session_model::TurnOutcome::Stopped(
        lash_protocol_types::session_model::TurnStop::Cancelled {
            evidence: evidence.clone(),
        },
    );

    let runner_output = output_from_process_turn(
        &process_id,
        &child_session_id,
        &lash_protocol_types::TurnId::fixture("projected-child-turn"),
        turn,
        &lash_execution_contracts::process::SessionTurnOutcome::FinalValue { schema: None },
        lash_core_store::execution_limits::DiagnosticLimits::standard(),
        lash_protocol_types::session_model::RuntimeOutputCuts::standard(),
    );
    assert_child_turn_cancellation(&runner_output, &evidence);

    let completion = registry
        .complete_process(
            &process_id,
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(runner_output),
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                &process_id,
            ),
        )
        .await
        .expect("persist child-turn cancellation");
    let recorded = completion
        .stored()
        .outcome()
        .as_ref()
        .expect("terminal process outcome")
        .clone()
        .into_tool_output();
    assert_child_turn_cancellation(&recorded, &evidence);
    let parent_result = completion
        .stored()
        .outcome()
        .clone()
        .expect("parent await result")
        .into_tool_output();
    assert_child_turn_cancellation(&parent_result, &evidence);
}

fn assert_child_turn_cancellation(
    output: &lash_protocol_types::tool_output::ToolCallOutput,
    evidence: &lash_protocol_types::session_model::TurnCancellationEvidence,
) {
    let lash_protocol_types::tool_output::ToolCallOutcome::Cancelled(cancellation) =
        &output.outcome
    else {
        panic!("expected child-turn cancellation, got {:?}", output.outcome);
    };
    assert_eq!(
        cancellation.origin,
        Some(lash_protocol_types::tool_output::CancelOrigin::TurnStopped)
    );
    assert_eq!(cancellation.message, evidence.reason.as_deref().unwrap());
    assert_eq!(
        cancellation
            .raw
            .as_ref()
            .map(lash_protocol_types::tool_output::ToolValue::to_json_value),
        Some(serde_json::to_value(evidence).expect("encode turn cancellation evidence"))
    );
}

/// Project one ended child turn through the runner under `result`.
fn project_turn(
    turn: lash_protocol_types::session_model::TurnOutcome,
    result: &lash_execution_contracts::process::SessionTurnOutcome,
) -> lash_protocol_types::tool_output::ToolCallOutput {
    output_from_process_turn(
        &lash_protocol_types::ProcessId::fixture("projected-child-process"),
        &SessionId::from("projected-child"),
        &lash_protocol_types::TurnId::fixture("projected-child-turn"),
        turn,
        result,
        lash_core_store::execution_limits::DiagnosticLimits::standard(),
        lash_protocol_types::session_model::RuntimeOutputCuts::standard(),
    )
}

fn finished_turn(
    finish: lash_protocol_types::session_model::TurnFinish,
) -> lash_protocol_types::session_model::TurnOutcome {
    lash_protocol_types::session_model::TurnOutcome::Finished(finish)
}

fn projected_failure(
    output: lash_protocol_types::tool_output::ToolCallOutput,
) -> lash_protocol_types::tool_output::ToolFailure {
    let lash_protocol_types::tool_output::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("expected a projected failure, got {:?}", output.outcome);
    };
    failure
}

/// ADR 0033: the child's assistant value is its exact outcome text.
#[test]
fn child_final_value_preserves_assistant_text() {
    for text in [" \tuseful prose  \r\nnext\t\n", "   ", ""] {
        let output = project_turn(
            finished_turn(
                lash_protocol_types::session_model::TurnFinish::AssistantMessage {
                    text: text.to_string(),
                },
            ),
            &lash_execution_contracts::process::SessionTurnOutcome::FinalValue { schema: None },
        );
        assert_eq!(output.value_for_projection(), serde_json::json!(text));
    }
}

/// ADR 0116 §3.7: typed final and tool values retain their shape, and
/// the declared schema accepts or refuses that value.
#[test]
fn child_final_value_keeps_typed_values_and_checks_the_schema() {
    let value = serde_json::json!({ "answer": 42 });
    let typed = lash_execution_contracts::process::SessionTurnOutcome::FinalValue {
        schema: Some(
            lash_protocol_types::JsonSchema::admit(serde_json::json!({
                "type": "object",
                "properties": { "answer": { "type": "integer" } },
                "required": ["answer"],
                "additionalProperties": false
            }))
            .expect("declared result schema"),
        ),
    };
    for finish in [lash_protocol_types::session_model::TurnFinish::Finished {
        tool_name: "finish".to_string(),
        value: value.clone(),
    }] {
        for result in [
            &lash_execution_contracts::process::SessionTurnOutcome::FinalValue { schema: None },
            &typed,
        ] {
            assert_eq!(
                project_turn(finished_turn(finish.clone()), result).value_for_projection(),
                value
            );
        }
    }
    for value in [
        serde_json::json!(["tool", "value"]),
        serde_json::json!(null),
        serde_json::json!(42),
    ] {
        assert_eq!(
            project_turn(
                finished_turn(lash_protocol_types::session_model::TurnFinish::Finished {
                    tool_name: "finish".to_string(),
                    value: value.clone()
                }),
                &lash_execution_contracts::process::SessionTurnOutcome::FinalValue { schema: None }
            )
            .value_for_projection(),
            value
        );
    }
    let failure = projected_failure(project_turn(
        finished_turn(lash_protocol_types::session_model::TurnFinish::Finished {
            tool_name: "finish".to_string(),
            value: serde_json::json!({ "answer": "forty-two" }),
        }),
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
}

/// ADR 0116 §3.7: a frame switch or stopped turn cannot supply a final value.
#[test]
fn child_final_value_refuses_a_frame_switch_or_stopped_turn() {
    let switched = lash_protocol_types::session_model::TurnOutcome::AgentFrameSwitch {
        frame_key: lash_protocol_types::FrameKey::from_caller_material("next-frame")
            .expect("frame key"),
        task: "continue elsewhere".to_string(),
        initial_nodes: Vec::new(),
        next_run: lash_core_store::turn_input_vocabulary::RunSpec::default()
            .next_run()
            .expect("the default spec encodes"),
    };
    let failure = projected_failure(project_turn(
        switched.clone(),
        &lash_execution_contracts::process::SessionTurnOutcome::FinalValue { schema: None },
    ));
    assert_eq!(failure.code, "process_session_turn_frame_switch");
    let turn = project_turn(
        switched.clone(),
        &lash_execution_contracts::process::SessionTurnOutcome::Turn,
    );
    assert_eq!(
        turn.value_for_projection()["outcome"],
        serde_json::to_value(&switched).expect("typed outcome")
    );

    let stopped = lash_protocol_types::session_model::TurnOutcome::Stopped(
        lash_protocol_types::session_model::TurnStop::Incomplete,
    );
    let failure = final_value_of_turn(&stopped).expect_err("a stopped child has no final value");
    assert_eq!(failure.code, "process_session_turn_stopped");
}

/// FIG-5615: plugins read the same observed state as hosts, through their
/// runtime-provided read service rather than a store row.
#[tokio::test]
async fn plugin_process_reads_are_canonical_observations() {
    let backend = crate::testing::sqlite_memory_store_backend().await;
    let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::new(
        backend.clone(),
        lash_core_store::store::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
        lash_protocol_types::ExecutionBudgets::recommended(),
        crate::runtime::DeltaCoalescing::recommended(),
        crate::DataRetentionConfig::standard(),
    ));
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let registry = backend.process_registry();
    let record = registry
        .register_process(lash_execution_contracts::testing::held_engine_registration(
            serde_json::json!({"probe": true}),
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect("register");
    registry
        .add_observer(
            runtime.session_id(),
            &record.id,
            lash_execution_contracts::process::ProcessObserverBy::host("law"),
        )
        .await
        .expect("visible");
    let services = runtime.runtime_session_services().expect("services");
    let rows: Vec<lash_execution_contracts::process::ObservedProcess> = services
        .process_read_service()
        .list_visible(
            runtime.session_id(),
            lash_execution_contracts::process::ProcessListMode::All,
            crate::ProcessOpScope::new(crate::ActorContext::detached(backend.clone())),
        )
        .await
        .expect("plugin read");
    let observer = lash_execution_contracts::process::ProcessWorkObserver::new(registry)
        .with_actor_parks(Arc::clone(backend.durable()));
    assert_eq!(
        rows,
        vec![
            observer
                .process(&record.id)
                .await
                .expect("host read")
                .expect("retained")
        ]
    );
}
