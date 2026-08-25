use super::*;

#[test]
fn buffered_wire_preserves_concrete_model_and_explicit_zero_reasoning() {
    let value = json!({
        "id": "gen-123",
        "model": "anthropic/claude-sonnet-4.5",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": {
            "completion_tokens_details": { "reasoning_tokens": 0 }
        }
    });
    let mut state = ChatStreamState::default();
    state
        .capture_response_value(&value)
        .expect("buffered identity is stable");

    assert_eq!(
        state.execution_evidence,
        Some(ExecutionEvidence {
            served_model: Some("anthropic/claude-sonnet-4.5".to_string()),
            provider_response_id: Some("gen-123".to_string()),
            provider_request_id: None,
            reasoning_output_tokens: Some(0),
            provider_finish_reason: Some("stop".to_string()),
            collection_interruption: None,
        })
    );
    assert_ne!(
        state
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.served_model.as_deref()),
        Some("openrouter/auto")
    );
}
