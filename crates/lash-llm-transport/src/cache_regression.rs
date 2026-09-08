//! Reusable prompt-cache regression assertions for provider and protocol tests.

use lash_core::LlmRequest;
use serde_json::Value;

/// One fully serialized provider request plus the provider-specific prompt
/// projection used for byte-stability comparisons.
#[derive(Clone, Debug)]
pub struct SerializedPromptRequest {
    pub body: Value,
    pub stable_prefix: Value,
}

/// Runs consecutive protocol requests through a provider serializer and
/// verifies that every prefix which was cacheable on call K is byte-identical
/// on call K+1.
pub fn assert_prefix_stability<F>(case_name: &str, iterations: &[LlmRequest], mut serialize: F)
where
    F: FnMut(&LlmRequest, usize) -> SerializedPromptRequest,
{
    assert!(
        iterations.len() >= 2,
        "{case_name}: prefix harness needs at least two protocol iterations"
    );

    for (call_index, pair) in iterations.windows(2).enumerate() {
        let stable_messages = stable_message_count(&pair[0]);
        let previous = serialize(&pair[0], stable_messages);
        let next = serialize(&pair[1], stable_messages);
        let previous_bytes = serde_json::to_vec(&previous.stable_prefix)
            .expect("provider stable prefix must serialize");
        let next_bytes =
            serde_json::to_vec(&next.stable_prefix).expect("provider stable prefix must serialize");
        assert_eq!(
            previous_bytes,
            next_bytes,
            "{case_name}: call {} changed bytes that call {} placed before its cache breakpoint\nprevious body: {}\nnext body: {}",
            call_index + 2,
            call_index + 1,
            previous.body,
            next.body,
        );
    }
}

/// Number of logical messages that belong to call K's stable prefix. An
/// explicit breakpoint wins; protocols without one treat the complete prior
/// call as stable because the next iteration may only append to it.
pub fn stable_message_count(request: &LlmRequest) -> usize {
    request
        .messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    lash_core::llm::types::LlmContentBlock::Text {
                        cache_breakpoint: true,
                        ..
                    }
                )
            })
        })
        .map_or(request.messages.len(), |(index, _)| index + 1)
}

/// Removes only provider cache directives. Prompt roles, ordering, content
/// block shape, and bytes remain significant so a string/array wire-shape
/// flip still fails the harness.
pub fn strip_cache_directives(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("cache_control");
            object.remove("__lash_cache_breakpoint");
            for child in object.values_mut() {
                strip_cache_directives(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                strip_cache_directives(child);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
    use std::sync::Arc;

    fn request(messages: Vec<LlmMessage>) -> LlmRequest {
        LlmRequest {
            model: "model".to_string(),
            messages,
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: Default::default(),
            model_variant: Default::default(),
            model_capability: Default::default(),
            scope: lash_core::LlmRequestScope::new("session", "frame", "request"),
            output_spec: None,
            stream_events: None,
            generation: Default::default(),
            provider_trace: None,
        }
    }

    #[test]
    fn explicit_breakpoint_selects_the_last_marked_message() {
        let request = request(vec![
            LlmMessage::text(LlmRole::System, "system"),
            LlmMessage::new(
                LlmRole::User,
                vec![LlmContentBlock::Text {
                    text: "stable".into(),
                    response_meta: None,
                    cache_breakpoint: true,
                }],
            ),
            LlmMessage::text(LlmRole::User, "volatile"),
        ]);

        assert_eq!(stable_message_count(&request), 2);
    }

    #[test]
    fn request_without_breakpoint_uses_the_whole_prior_call() {
        let request = request(vec![LlmMessage::text(LlmRole::User, "stable")]);
        assert_eq!(stable_message_count(&request), 1);
    }
}
