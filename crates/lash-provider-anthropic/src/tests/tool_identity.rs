use super::*;
use lash_core::llm::transport::{ProviderFailureKind, TransportRetryVerdict};

fn call(id: &str) -> LlmContentBlock {
    LlmContentBlock::ToolCall {
        call_id: id.into(),
        tool_name: "lookup".into(),
        input_json: "{}".into(),
        replay: None,
    }
}

fn result(id: &str) -> LlmContentBlock {
    LlmContentBlock::ToolResult {
        call_id: id.into(),
        tool_name: Some("lookup".into()),
        content: vec![lash_sansio::ModelToolReturnPart::text("found")],
    }
}

#[test]
fn empty_tool_identities_are_rejected_by_request_builder() {
    for (calls, results) in [
        (vec![call("")], vec![result("")]),
        (vec![call("")], vec![]),
        (vec![], vec![result("")]),
        (vec![call(""), call("")], vec![result(""), result("")]),
        (vec![call("valid")], vec![result("")]),
    ] {
        let req = request(vec![
            LlmMessage::new(LlmRole::Assistant, calls),
            LlmMessage::new(LlmRole::User, results),
        ]);
        let error = AnthropicProvider::new("key")
            .build_request_body(&req)
            .expect_err("empty tool identities must be rejected before encoding");
        assert_eq!(error.kind, ProviderFailureKind::Validation);
    }
}

#[test]
fn nonempty_tool_identities_match_and_encode_repeatably() {
    for (id, expected) in [
        ("call_1-ABC".to_string(), "call_1-ABC".to_string()),
        ("call.a/é".to_string(), "call_a__".to_string()),
        ("x".repeat(80), "x".repeat(64)),
    ] {
        let req = request(vec![
            LlmMessage::new(LlmRole::Assistant, vec![call(&id)]),
            LlmMessage::new(LlmRole::User, vec![result(&id)]),
        ]);
        let provider = AnthropicProvider::new("key");
        let body = provider.build_request_body(&req).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["id"], expected);
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], expected);
        assert_eq!(body, provider.build_request_body(&req).unwrap());
    }
}

#[test]
fn missing_or_empty_stream_tool_identity_is_rejected_before_publication() {
    for id in [None, Some(json!("")), Some(Value::Null), Some(json!(123))] {
        let mut block = json!({"type": "tool_use", "name": "lookup", "input": {}});
        if let Some(id) = id {
            block["id"] = id;
        }
        let event = json!({"type": "content_block_start", "index": 0, "content_block": block});
        let mut state = StreamState::default();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = Arc::clone(&events);
        let sender = LlmEventSender::new(move |event| output.lock_recover().push(event));
        let error = AnthropicProvider::process_sse_event(
            &event.to_string(),
            &mut state,
            Some(&sender),
            true,
        )
        .expect_err("malformed tool identity must fail at block start");
        assert_eq!(error.kind, ProviderFailureKind::Stream);
        assert_eq!(error.retry_verdict, TransportRetryVerdict::NotRetryable);
        assert!(events.lock_recover().is_empty());
        let (parts, _, _) = AnthropicProvider::finalize(state, "claude-test");
        assert!(
            parts
                .iter()
                .all(|part| !matches!(part, LlmOutputPart::ToolCall { .. }))
        );
    }
}
