use super::*;

#[tokio::test]
async fn codex_auto_skips_websocket_while_session_fallback_is_active() {
    let http = spawn_http_sse_sequence(vec![
        ("resp_http_1", "msg_http_1", "fallback-one"),
        ("resp_http_2", "msg_http_2", "fallback-two"),
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Auto,
        http.url.clone(),
        "ws://127.0.0.1:1/codex/responses".to_string(),
    );

    let first = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first SSE fallback response");

    assert_eq!(first.full_text(), "fallback-one");
    assert!(
        provider
            .websocket_fallback_reason(&request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .is_some()
    );

    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp_ws",
        message_id: "msg_ws",
        text: "should-not-run",
    }])
    .await;
    provider.websocket_url = ws.url.clone();
    let second = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "next")]))
        .await
        .expect("second SSE fallback response");

    assert_eq!(second.full_text(), "fallback-two");
    assert_eq!(ws.captured().len(), 0);
    assert_eq!(http.captured_len(), 2);
    let sse_request = http.captured().remove(0);
    let session_affinity =
        LlmRequestScope::new("session-1", "", "").provider_session_affinity_key();
    assert!(sse_request.contains(&format!("session-id: {session_affinity}")));
    assert!(!sse_request.contains("session-id: session-1"));
    assert!(sse_request.contains("x-client-request-id: session-1:request:test"));
    assert!(!sse_request.contains("session_id:"));
}
