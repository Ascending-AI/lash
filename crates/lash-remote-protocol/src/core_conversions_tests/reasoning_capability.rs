use super::*;

#[test]
fn recorded_remote_reasoning_capabilities_of_the_removed_shape_are_refused() {
    for removed in [
        serde_json::json!({ "efforts": ["low"], "retired_default": "low" }),
        serde_json::json!({ "efforts": ["low"], "aliases": { "minimal": "low" } }),
        serde_json::json!({ "efforts": ["low"], "disable": "toggle_false" }),
        serde_json::json!({ "efforts": ["low"], "disable": { "budget": 0 } }),
    ] {
        assert!(
            serde_json::from_value::<RemoteReasoningCapability>(removed.clone()).is_err(),
            "{removed}"
        );
    }
    let current: RemoteReasoningCapability =
        serde_json::from_value(serde_json::json!({ "efforts": ["low"], "disable": true }))
            .expect("current shape");
    let core = core_llm::ReasoningCapability::from(current.clone());
    assert!(core.disable);
    assert_eq!(RemoteReasoningCapability::from(core), current);
}
