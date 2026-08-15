use std::sync::Arc;

use super::*;
use crate::llm::types::ProviderRouteIdentity;

#[test]
fn reasoning_part_roundtrips_when_snapshot_predates_field() {
    let legacy = r#"[{
        "id":"m0","role":"Assistant",
        "parts":[{
            "id":"m0.p0","kind":"Prose","content":"Hi",
            "prune_state":"Intact"
        }]
    }]"#;
    let msgs: Vec<Message> = serde_json::from_str(legacy).expect("legacy snapshot");
    assert!(msgs[0].parts[0].reasoning_meta.is_none());
}

fn replay_request_from_reopened_message(
    minting_route: Option<ProviderRouteIdentity>,
    serving_model: &str,
) -> crate::llm::types::LlmRequest {
    let reasoning = ProviderReasoningReplay {
        item_id: Some("reasoning-item".to_string()),
        encrypted_content: Some("encrypted".to_string()),
        signature: Some("signature".to_string()),
        redacted: false,
        summary: vec!["neutral summary".to_string()],
        origin: minting_route.clone(),
    };
    let tool = ProviderReplayMeta {
        item_id: Some("tool-item".to_string()),
        opaque: Some("opaque".to_string()),
        origin: minting_route,
    };
    let history = vec![Message {
        id: "assistant-1".to_string(),
        role: MessageRole::Assistant,
        parts: Arc::new(vec![
            Part::reasoning(
                "assistant-1.p0".to_string(),
                "neutral summary".to_string(),
                Some(reasoning),
            ),
            Part::tool_call(
                "assistant-1.p1".to_string(),
                "{}".to_string(),
                "call-1".to_string(),
                "lookup".to_string(),
                Some(tool),
            ),
        ]),
        origin: None,
    }];
    let persisted = serde_json::to_vec(&history).expect("session history serializes");
    let reopened: Vec<Message> =
        serde_json::from_slice(&persisted).expect("session history reopens");
    crate::llm::types::LlmRequest {
        model: serving_model.to_string(),
        messages: render_prompt(&reopened).messages,
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: crate::llm::types::LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: Default::default(),
        generation: Default::default(),
        scope: crate::llm::types::LlmRequestScope::new("session-1", "frame-1", "request-1"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

fn assert_neutral_replay_projection(request: &crate::llm::types::LlmRequest) {
    let blocks = request.messages[0].blocks.as_ref();
    assert!(blocks.iter().any(|block| matches!(
        block,
        LlmContentBlock::Text { text, .. } if text.as_ref() == "neutral summary"
    )));
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolCall { replay: None, .. }))
    );
}

#[test]
fn reopened_replay_is_route_locked_for_every_ordered_provider_pair() {
    let routes = [
        ProviderRouteIdentity::for_endpoint(
            "anthropic",
            "https://api.anthropic.com",
            "claude-sonnet-4-6",
        ),
        ProviderRouteIdentity::for_endpoint(
            "google_oauth",
            "https://cloudcode-pa.googleapis.com/v1internal",
            "gemini-2.5-pro",
        ),
        ProviderRouteIdentity::for_endpoint(
            "openai-compatible",
            "https://openrouter.ai/api/v1",
            "gpt-5.4-chat",
        ),
        ProviderRouteIdentity::for_endpoint("openai", "https://api.openai.com/v1", "gpt-5.4"),
    ];

    for minting_route in &routes {
        for serving_route in &routes {
            let mut request = replay_request_from_reopened_message(
                Some(minting_route.clone()),
                &serving_route.model,
            );
            let drops = request.drop_foreign_replay(serving_route);
            if minting_route == serving_route {
                assert!(drops.is_empty(), "same route must replay natively");
                assert!(request.messages[0].blocks.iter().all(|block| match block {
                    LlmContentBlock::Reasoning { replay, .. } => replay.is_some(),
                    LlmContentBlock::ToolCall { replay, .. } => replay.is_some(),
                    _ => true,
                }));
            } else {
                assert_eq!(drops.len(), 2, "{minting_route:?} -> {serving_route:?}");
                assert!(drops.iter().all(|drop| {
                    drop.reason == crate::llm::types::ProviderReplayDropReason::ForeignRoute
                }));
                assert_neutral_replay_projection(&request);
            }
        }

        let mut switched =
            replay_request_from_reopened_message(Some(minting_route.clone()), "different-model");
        let serving_route = ProviderRouteIdentity {
            model: "different-model".into(),
            ..minting_route.clone()
        };
        let drops = switched.drop_foreign_replay(&serving_route);
        assert_eq!(drops.len(), 2, "same-provider model switch must drop");
        assert_neutral_replay_projection(&switched);
    }
}

#[test]
fn session_created_before_replay_provenance_loads_and_drops_as_unstamped() {
    let mut request = replay_request_from_reopened_message(None, "claude-sonnet-4-6");
    let persisted = serde_json::to_value(&request.messages).expect("prompt serializes");
    assert!(!persisted.to_string().contains("origin"));

    let serving_route = ProviderRouteIdentity::for_endpoint(
        "anthropic",
        "https://api.anthropic.com",
        "claude-sonnet-4-6",
    );
    let drops = request.drop_foreign_replay(&serving_route);

    assert_eq!(drops.len(), 2);
    assert!(drops.iter().all(|drop| {
        drop.reason == crate::llm::types::ProviderReplayDropReason::Unstamped
            && drop.minting_route.is_none()
    }));
    assert_neutral_replay_projection(&request);
}

fn remove_json_field(value: &mut serde_json::Value, field: &str) {
    match value {
        serde_json::Value::Object(object) => {
            object.remove(field);
            for child in object.values_mut() {
                remove_json_field(child, field);
            }
        }
        serde_json::Value::Array(array) => {
            for child in array {
                remove_json_field(child, field);
            }
        }
        _ => {}
    }
}

#[test]
fn partially_stripped_replay_provenance_loads_and_drops_as_unstamped() {
    for stripped_field in ["origin"] {
        let route = ProviderRouteIdentity::for_endpoint(
            "anthropic",
            "https://api.anthropic.com",
            "claude-sonnet-4-6",
        );
        let mut request =
            replay_request_from_reopened_message(Some(route.clone()), "claude-sonnet-4-6");
        for block in Arc::make_mut(&mut request.messages[0].blocks) {
            if let LlmContentBlock::Reasoning { text, .. } = block {
                text.clear();
            }
        }
        let mut persisted =
            serde_json::to_value(&request.messages).expect("prompt history serializes");
        remove_json_field(&mut persisted, stripped_field);
        request.messages =
            serde_json::from_value(persisted).expect("field-stripped history reopens");

        let drops = request.drop_foreign_replay(&route);

        assert_eq!(drops.len(), 2, "stripped {stripped_field}");
        assert!(
            drops.iter().all(|drop| {
                drop.reason == crate::llm::types::ProviderReplayDropReason::Unstamped
            })
        );
        assert_neutral_replay_projection(&request);
    }
}
