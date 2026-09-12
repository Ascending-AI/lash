use super::*;
use lash_core::{ProcessAwaitOutput, TestLocalProcessRegistry};
use lash_core::{ProcessLifecycle as _, ProcessRegistrar as _, ProcessRetention as _};
use lash_sansio::ProcessId;
use serde_json::json;

fn successful_turn_output(turn: lash_core::facade_support::AssembledTurn) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(json!({
        "turn": turn,
    })))
}

async fn registry_result(
    output: ProcessAwaitOutput,
    prune: bool,
    output_schema: Option<&Value>,
) -> Result<Value, String> {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let process_id = "subagent-outcome";
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        ))
        .await
        .unwrap();
    let terminal = registry
        .complete_process(
            &ProcessId::from(process_id),
            output,
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .unwrap();
    if prune {
        registry
            .prune_terminal_processes(
                terminal.updated_at_ms.saturating_add(1),
                None,
                lash_core::ProjectionWatermark::NoProjector,
            )
            .await
            .unwrap();
    }
    let output = lash_core::NativeProcessWork::for_registry(registry)
        .await_terminal(&ProcessId::from(process_id))
        .await
        .unwrap();
    child_task_result(output, output_schema)
}

#[tokio::test]
async fn failed_child_preserves_failure_reason() {
    let output = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(
        lash_core::ToolFailure::tool(
            lash_core::ToolFailureClass::Execution,
            "child_failure",
            "child failed precisely",
        ),
    ));
    assert_eq!(
        registry_result(output, false, None).await,
        Err("child failed precisely".into())
    );
}

#[tokio::test]
async fn cancelled_child_preserves_cancellation_reason() {
    let output = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
        lash_core::ToolCancellation::runtime("child cancelled precisely"),
    ));
    assert_eq!(
        registry_result(output, false, None).await,
        Err("child cancelled precisely".into())
    );
}

#[tokio::test]
async fn abandoned_child_reports_missing_outcome() {
    let output = ProcessAwaitOutput::Abandoned {
        evidence: Box::new(lash_core::AbandonEvidence {
            writer: lash_core::AbandonWriter::ReconciledRequest,
            owner: None,
            epoch_ms: 1,
        }),
        control: None,
    };
    assert_eq!(
        registry_result(output, false, None).await,
        Err("subagent process was abandoned before recording an outcome".into())
    );
}

#[tokio::test]
async fn pruned_child_reports_no_longer_retained() {
    let output =
        ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(Value::Null));
    assert_eq!(
        registry_result(output, true, None).await,
        Err("subagent process outcome is no longer retained".into())
    );
}

#[tokio::test]
async fn frame_switching_child_is_not_a_successful_task_result() {
    let mut turn = lash_core::testing::mock_assembled_turn(
        &lash_core::SessionId::from("frame-switching-child"),
        "unused",
    );
    turn.outcome = lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
        frame_key: lash_core::FrameKey::from_caller_material("next-frame").unwrap(),
        task: "continue elsewhere".to_string(),
        initial_nodes: Vec::new(),
    };

    assert_eq!(
        registry_result(successful_turn_output(turn), false, None).await,
        Err("subagent switched agent frames instead of producing a final task result".into())
    );
}

#[tokio::test]
async fn frame_switch_is_rejected_even_when_metadata_matches_output_schema() {
    let mut turn = lash_core::testing::mock_assembled_turn(
        &lash_core::SessionId::from("typed-frame-switching-child"),
        "unused",
    );
    turn.outcome = lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
        frame_key: lash_core::FrameKey::from_caller_material("next-frame").unwrap(),
        task: "continue elsewhere".to_string(),
        initial_nodes: Vec::new(),
    };
    let permissive_handoff_schema = json!({
        "type": "object",
        "properties": {
            "frame_key": { "type": "string" },
            "task": { "type": "string" }
        },
        "required": ["frame_key", "task"]
    });

    assert_eq!(
        registry_result(
            successful_turn_output(turn),
            false,
            Some(&permissive_handoff_schema)
        )
        .await,
        Err("subagent switched agent frames instead of producing a final task result".into())
    );
}

#[tokio::test]
async fn valid_untyped_child_result_remains_successful() {
    let turn = lash_core::testing::mock_assembled_turn(
        &lash_core::SessionId::from("untyped-child"),
        "  useful prose  ",
    );

    assert_eq!(
        registry_result(successful_turn_output(turn), false, None).await,
        Ok(json!("useful prose"))
    );
}

#[tokio::test]
async fn valid_typed_child_result_remains_successful() {
    let value = json!({ "answer": 42 });
    let mut turn = lash_core::testing::mock_assembled_turn(
        &lash_core::SessionId::from("typed-child"),
        "unused",
    );
    turn.outcome = lash_core::facade_support::TurnOutcome::Finished(
        lash_core::facade_support::TurnFinish::FinalValue {
            value: value.clone(),
        },
    );
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"],
        "additionalProperties": false
    });

    assert_eq!(
        registry_result(successful_turn_output(turn), false, Some(&schema)).await,
        Ok(value)
    );
}

#[tokio::test]
async fn invalid_typed_terminal_result_reaches_parent_failure_channel() {
    let mut turn = lash_core::testing::mock_assembled_turn(
        &lash_core::SessionId::from("invalid-typed-child"),
        "unused",
    );
    turn.outcome = lash_core::facade_support::TurnOutcome::Finished(
        lash_core::facade_support::TurnFinish::FinalValue {
            value: json!({ "answer": "forty-two" }),
        },
    );
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"],
        "additionalProperties": false
    });

    let result = registry_result(successful_turn_output(turn), false, Some(&schema)).await;
    let error = result.expect_err("the terminal boundary must reject a schema mismatch");
    assert!(
        error.starts_with("subagent task result did not match the declared output schema:"),
        "unexpected boundary error: {error}"
    );
    let ToolOutcome::Done(output) = finalise_tool_result(Err(error.clone())) else {
        panic!("terminal result rejection must finish through the parent tool channel");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("terminal result rejection must be a parent-visible failure");
    };
    assert_eq!(failure.message, error);
}

#[tokio::test]
async fn submit_error_emits_failure_control_with_reason() {
    let provider = RlmSubagentToolsProvider {
        registry: Arc::new(CapabilityRegistry::new()),
        session_spec: SessionSpec::inherit(),
        tool_access: SessionToolAccess::default(),
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        parent_subagent: None,
        include_submit_error: true,
    }
    .into_leaf_provider();
    let args = json!({"reason": "child cannot finish"});
    let ToolOutcome::Done(output) =
        lash_core::testing::run_tool(&provider, "submit_error", &args).await
    else {
        panic!("submit_error must finish inline");
    };
    let Some(lash_core::ToolControl::Fail { failure }) = output.control else {
        panic!("submit_error must carry Fail control");
    };
    assert_eq!(failure.code, "subagent_submit_error");
    assert_eq!(failure.message, args.to_string());
}

#[test]
fn spawn_rejects_child_depth_past_limit() {
    let registry = CapabilityRegistry::new().with(Arc::new(crate::StaticCapability::new(
        "child",
        SessionSpec::inherit(),
    )));
    let parent = SubagentSessionContext {
        parent_session_id: "root".into(),
        capability: "child".into(),
        depth: 5,
        max_depth: 5,
    };
    let snapshot = lash_core::runtime::RuntimeSessionState::new(lash_core::SessionPolicy::new(
        lash_core::TurnBudget::Unbounded,
    ))
    .to_snapshot();
    let result = build_spawn_create_request(SpawnCreateRequestInput {
        registry: &registry,
        parent_session_id: &SessionId::from("parent"),
        current_snapshot: snapshot,
        session_spec: &SessionSpec::inherit(),
        tool_access: &SessionToolAccess::default(),
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        capability_name: "child",
        output_schema: None,
        seed: Default::default(),
        parent_subagent: Some(&parent),
        caused_by: None,
    });
    assert_eq!(
        result.unwrap_err(),
        "subagent recursion depth exceeded: max depth is 5"
    );
}
