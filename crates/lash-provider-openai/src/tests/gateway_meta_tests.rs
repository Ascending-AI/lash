//! Gateway `meta` passthrough on the response-metadata seam (FIG-1381).
//!
//! OpenAI-compatible gateways answer with a top-level `meta` block next to
//! `usage`. These are wire tests: the recorded response bodies are the shapes
//! OpenRouter and Opper send, and no host allowlist is configured, because the
//! block is retained without one.

use super::*;

#[tokio::test]
async fn gateway_meta_routing_is_retained_on_a_buffered_chat_completion() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"meta":{"routing":{"requested":"deepseek/deepseek-chat","served":"deepinfra/deepseek-chat","attempts":2,"strategy":"fallback"}}}"#,
    ));
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1").with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("request succeeds");

    assert_eq!(
        response.response_metadata[lash_llm_transport::GATEWAY_META_KEY],
        json!({
            "routing": {
                "requested": "deepseek/deepseek-chat",
                "served": "deepinfra/deepseek-chat",
                "attempts": 2,
                "strategy": "fallback"
            }
        }),
        "the gateway meta block is retained verbatim without any host allowlist"
    );
}

#[tokio::test]
async fn gateway_meta_routing_is_retained_on_a_streaming_chat_completion() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"meta\":{\"routing\":{\"served\":\"fireworks/deepseek-chat\",\"attempts\":1}}}\n\n",
        "data: [DONE]\n\n"
    );
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("content-type".to_string(), "text/event-stream".to_string())],
        body,
    ));
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1").with_transport(transport);

    let response = provider
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("terminal stream succeeds");

    assert_eq!(
        response.response_metadata[lash_llm_transport::GATEWAY_META_KEY],
        json!({"routing": {"served": "fireworks/deepseek-chat", "attempts": 1}})
    );
}

#[tokio::test]
async fn a_response_without_a_gateway_meta_block_retains_no_key() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"total_tokens":3}}"#,
    ));
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1").with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("request succeeds");

    assert!(
        !response
            .response_metadata
            .contains_key(lash_llm_transport::GATEWAY_META_KEY)
    );
}

#[tokio::test]
async fn a_malformed_gateway_meta_block_is_dropped_without_failing_the_response() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"meta":"deepinfra/deepseek-chat"}"#,
    ));
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1").with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("a meta block of the wrong shape never fails the response");

    assert_eq!(response.full_text(), "done");
    assert!(
        !response
            .response_metadata
            .contains_key(lash_llm_transport::GATEWAY_META_KEY)
    );
}
