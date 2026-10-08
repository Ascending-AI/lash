//! What a turn's preparation shares: the resident head's refresh and the
//! turn's trace metadata.

pub(super) fn turn_trace_metadata(
    state: &crate::runtime::RuntimeSessionState,
    input_item_count: usize,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    std::collections::BTreeMap::from([
        (
            "input_item_count".into(),
            serde_json::json!(input_item_count),
        ),
        (
            "profile_key".into(),
            serde_json::json!(
                state
                    .policy
                    .model
                    .as_ref()
                    .map(|model| model.key().as_str())
            ),
        ),
        ("model".into(), serde_json::json!(state.policy.wire_model())),
        (
            "config_revision".into(),
            serde_json::json!(state.config_revision),
        ),
    ])
}
