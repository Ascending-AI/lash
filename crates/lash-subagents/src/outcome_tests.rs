use super::*;
use lash_core::{ProcessAwaitOutput, TestLocalProcessRegistry};
use lash_core::{ProcessLifecycle as _, ProcessRegistrar as _, ProcessRetention as _};
use lash_sansio::ProcessId;
use serde_json::json;

async fn registry_result(output: ProcessAwaitOutput, prune: bool) -> Result<Value, String> {
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
    child_task_result(output)
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
        registry_result(output, false).await,
        Err("child failed precisely".into())
    );
}

#[tokio::test]
async fn cancelled_child_preserves_cancellation_reason() {
    let output = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
        lash_core::ToolCancellation::runtime("child cancelled precisely"),
    ));
    assert_eq!(
        registry_result(output, false).await,
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
        registry_result(output, false).await,
        Err("subagent process was abandoned before recording an outcome".into())
    );
}

#[tokio::test]
async fn pruned_child_reports_no_longer_retained() {
    let output =
        ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(Value::Null));
    assert_eq!(
        registry_result(output, true).await,
        Err("subagent process outcome is no longer retained".into())
    );
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
