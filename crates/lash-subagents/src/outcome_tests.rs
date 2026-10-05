use super::*;

#[test]
fn spawn_rejects_child_depth_past_limit() {
    let registry = CapabilityRegistry::new().with(Arc::new(crate::StaticCapability::new(
        "child",
        SessionSpec::inherit(),
    )));
    let parent = SubagentSessionContext {
        capability: "child".into(),
        depth: 5,
    };
    let snapshot = lash_core::runtime::RuntimeSessionState::new(lash_core::SessionPolicy::new(
        lash_core::TurnBudget::Unbounded,
        lash_core::MaxToolCalls::new(1024),
    ))
    .to_snapshot();
    let result = build_spawn_create_request(SpawnCreateRequestInput {
        fleet_format: lash_core::FleetFormat::current(),
        registry: &registry,
        parent_session_id: &lash_core::SessionId::from("parent"),
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
