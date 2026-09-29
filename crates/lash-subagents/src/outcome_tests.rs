use super::*;
use serde_json::json;

#[tokio::test]
async fn submit_error_emits_failure_control_with_reason() {
    let provider = RlmSubagentToolsProvider {
        registry: Arc::new(CapabilityRegistry::new()),
        session_spec: SessionSpec::inherit(),
        tool_access: SessionToolAccess::default(),
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        parent_subagent: None,
        include_submit_error: true,
        lifetime: Arc::new(lash_core::lifetime::starter),
        timeout: None,
    }
    .into_provider();
    let args = json!({"reason": "child cannot finish"});
    let lash_core::ToolAttemptOutcome::Done { result, intents } =
        lash_core::testing::run_tool(&provider, "submit_error", &args).await
    else {
        panic!("submit_error must finish inline");
    };
    assert!(intents.is_empty());
    let output = result.into_output();
    let Some(lash_core::ToolControl::Fail { failure }) = output.control else {
        panic!("submit_error must carry Fail control");
    };
    assert_eq!(failure.code, "subagent_submit_error");
    // FIG-2975: the reason is the message a parent reads verbatim; the encoded
    // call survives in `raw`.
    assert_eq!(failure.message, "child cannot finish");
    assert_eq!(
        failure
            .raw
            .as_ref()
            .map(lash_core::ToolValue::to_json_value),
        Some(args)
    );
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
