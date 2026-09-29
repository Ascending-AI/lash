use super::*;
use lash_core::llm::transport::ProviderFailureKind;

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
    for id in [
        "call_1-ABC".to_string(),
        "call.a/é".to_string(),
        "x".repeat(80),
    ] {
        let req = request(vec![
            LlmMessage::new(LlmRole::Assistant, vec![call(&id)]),
            LlmMessage::new(LlmRole::User, vec![result(&id)]),
        ]);
        let provider = AnthropicProvider::new("key");
        let body = provider.build_request_body(&req).unwrap();
        let wire_id = body["messages"][0]["content"][0]["id"].as_str().unwrap();
        assert!(!wire_id.is_empty());
        assert!(wire_id.len() <= 64);
        assert!(
            wire_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        );
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], wire_id);
        if id == "call_1-ABC" {
            assert_eq!(wire_id, id);
        } else {
            assert_ne!(wire_id, id);
        }
        assert_eq!(body, provider.build_request_body(&req).unwrap());
    }
}

#[test]
fn rewritten_tool_ids_do_not_collide_and_results_follow_their_calls() {
    let prefix = "x".repeat(64);
    let ids = [
        format!("{prefix}a"),
        format!("{prefix}b"),
        "call.a/é".into(),
    ];
    let req = request(vec![
        LlmMessage::new(LlmRole::Assistant, ids.iter().map(|id| call(id)).collect()),
        LlmMessage::new(LlmRole::User, ids.iter().map(|id| result(id)).collect()),
    ]);
    let provider = AnthropicProvider::new("key");
    let body = provider.build_request_body(&req).unwrap();
    let calls = body["messages"][0]["content"].as_array().unwrap();
    let results = body["messages"][1]["content"].as_array().unwrap();
    let wire_ids: Vec<&str> = calls
        .iter()
        .map(|call| call["id"].as_str().unwrap())
        .collect();
    assert_eq!(wire_ids.len(), 3);
    assert_eq!(
        wire_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    for (index, wire_id) in wire_ids.iter().enumerate() {
        assert!(wire_id.len() <= 64);
        assert!(
            wire_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        );
        assert_eq!(results[index]["tool_use_id"], *wire_id);
    }
    assert_eq!(body, provider.build_request_body(&req).unwrap());
}

#[test]
fn legal_tool_id_wins_a_collision_with_rewritten_id() {
    let invalid = "lookup/one";
    let provider = AnthropicProvider::new("key");
    let first = provider
        .build_request_body(&request(vec![
            LlmMessage::new(LlmRole::Assistant, vec![call(invalid)]),
            LlmMessage::new(LlmRole::User, vec![result(invalid)]),
        ]))
        .unwrap();
    let candidate = first["messages"][0]["content"][0]["id"].as_str().unwrap();
    let req = request(vec![
        LlmMessage::new(LlmRole::Assistant, vec![call(invalid), call(candidate)]),
        LlmMessage::new(LlmRole::User, vec![result(invalid), result(candidate)]),
    ]);
    let body = provider.build_request_body(&req).unwrap();
    let rewritten = body["messages"][0]["content"][0]["id"].as_str().unwrap();
    assert_ne!(rewritten, candidate);
    assert_eq!(body["messages"][0]["content"][1]["id"], candidate);
    assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], rewritten);
    assert_eq!(body["messages"][1]["content"][1]["tool_use_id"], candidate);
    assert_eq!(body, provider.build_request_body(&req).unwrap());
}

#[test]
fn missing_or_empty_stream_tool_identity_reaches_core_as_empty() {
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
        AnthropicProvider::process_sse_event(&event.to_string(), &mut state, Some(&sender), true)
            .expect("core repairs the missing identity after assembly");
        let stop = json!({"type": "content_block_stop", "index": 0});
        AnthropicProvider::process_sse_event(&stop.to_string(), &mut state, Some(&sender), true)
            .unwrap();
        let (parts, _, _) = AnthropicProvider::finalize(state, "claude-test");
        assert!(matches!(&parts[0], LlmOutputPart::ToolCall { call_id, .. } if call_id.is_empty()));
    }
}
