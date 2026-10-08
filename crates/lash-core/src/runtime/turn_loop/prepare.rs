//! What a turn's preparation shares: the resident head's refresh, input
//! normalization and the turn's trace metadata.

use super::*;

impl LashRuntime {
    pub async fn normalize_input_items(
        &self,
        items: &[InputItem],
    ) -> Result<Vec<NormalizedItem>, String> {
        normalize_input_items(items, self.host.core.durability.attachment_store.as_ref()).await
    }
}

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
