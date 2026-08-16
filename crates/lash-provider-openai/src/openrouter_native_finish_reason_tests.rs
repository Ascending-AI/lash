use super::*;

fn assert_native_finish_reason(state: &ChatStreamState) {
    let evidence = state.execution_evidence().expect("execution evidence");
    assert_eq!(
        evidence.provider_finish_reason.as_deref(),
        Some("stop_sequence")
    );
}

#[test]
fn buffered_wire_prefers_native_finish_reason() {
    let value = json!({
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop",
            "native_finish_reason": "stop_sequence"
        }]
    });
    let mut state = ChatStreamState::default();
    state
        .capture_response_value(&value)
        .expect("buffered identity is stable");
    assert_native_finish_reason(&state);
}

#[test]
fn stream_wire_prefers_native_finish_reason() {
    let mut state = ChatStreamState::default();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"choices":[{"delta":{},"finish_reason":"stop","native_finish_reason":"stop_sequence"}]}"#,
        &mut state,
    )
    .expect("SSE chunk parses");
    assert_native_finish_reason(&state);
}
