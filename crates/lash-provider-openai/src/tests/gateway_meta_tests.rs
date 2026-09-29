//! Gateway `meta` capture through the host allowlist (FIG-4104).
//!
//! OpenAI-compatible gateways answer with a top-level `meta` block next to
//! `usage` — `meta.routing` names the requested and served routes, the
//! attempt count and the strategy. There is no special case for it: like
//! every other body member it enters `response_metadata` only because the
//! host named its JSON pointer. These wire tests allowlist `/meta` and the
//! nested `/meta/routing` and cover the buffered, buffered-SSE and
//! streaming paths on both endpoints.

use super::*;

fn meta_allowlist() -> ProviderOptions {
    ProviderOptions {
        response_metadata_body_paths: vec!["/meta".to_string(), "/meta/routing".to_string()],
        ..ProviderOptions::default()
    }
}

fn assert_meta_block(metadata: &BTreeMap<String, Value>, served: &str) {
    assert_eq!(
        metadata["body:/meta"],
        json!({"routing": {"requested": "auto", "served": served}}),
        "the allowlisted /meta pointer captures the gateway block"
    );
    assert_eq!(
        metadata["body:/meta/routing"],
        json!({"requested": "auto", "served": served}),
        "the nested /meta/routing pointer captures inside the block"
    );
}

fn chat_meta_stream() -> &'static str {
    concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}],\"meta\":{\"routing\":{\"requested\":\"auto\",\"served\":\"first\"}}}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"meta\":{\"routing\":{\"requested\":\"auto\",\"served\":\"last\"}}}\n\n",
        "data: [DONE]\n\n"
    )
}

fn responses_meta_stream() -> &'static str {
    concat!(
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"done\",\"meta\":{\"routing\":{\"requested\":\"auto\",\"served\":\"first\"}}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg-1\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}]},\"meta\":{\"routing\":{\"requested\":\"auto\",\"served\":\"last\"}}}\n\n"
    )
}

#[tokio::test]
async fn allowlisted_meta_is_captured_on_a_buffered_chat_completion() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"meta":{"routing":{"requested":"auto","served":"deepinfra","attempts":2,"strategy":"fallback"}}}"#,
    ));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_options(meta_allowlist())
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("request succeeds");

    assert_eq!(
        response.response_metadata["body:/meta"],
        json!({
            "routing": {
                "requested": "auto",
                "served": "deepinfra",
                "attempts": 2,
                "strategy": "fallback"
            }
        })
    );
    assert_eq!(
        response.response_metadata["body:/meta/routing"],
        json!({
            "requested": "auto",
            "served": "deepinfra",
            "attempts": 2,
            "strategy": "fallback"
        })
    );
}

#[tokio::test]
async fn allowlisted_meta_capture_is_last_wins_on_a_buffered_sse_chat_completion() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        chat_meta_stream(),
    ));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_options(meta_allowlist())
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("buffered SSE-shaped response succeeds");

    assert_meta_block(&response.response_metadata, "last");
}

#[tokio::test]
async fn allowlisted_meta_capture_is_last_wins_on_a_streaming_chat_completion() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("content-type".to_string(), "text/event-stream".to_string())],
        chat_meta_stream(),
    ));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_options(meta_allowlist())
        .with_transport(transport);

    let response = provider
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("terminal stream succeeds");

    assert_meta_block(&response.response_metadata, "last");
}

#[tokio::test]
async fn allowlisted_meta_is_captured_on_a_buffered_responses_request() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        r#"{"id":"resp-123","model":"test-model","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"meta":{"routing":{"requested":"auto","served":"deepinfra","attempts":2,"strategy":"fallback"}}}"#,
    ));
    let mut provider = OpenAiProvider::new("key")
        .with_options(meta_allowlist())
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("Responses request succeeds");

    assert_eq!(
        response.response_metadata["body:/meta"],
        json!({
            "routing": {
                "requested": "auto",
                "served": "deepinfra",
                "attempts": 2,
                "strategy": "fallback"
            }
        })
    );
    assert_eq!(
        response.response_metadata["body:/meta/routing"],
        json!({
            "requested": "auto",
            "served": "deepinfra",
            "attempts": 2,
            "strategy": "fallback"
        })
    );
}

#[tokio::test]
async fn allowlisted_meta_capture_is_last_wins_on_a_buffered_sse_responses_request() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        responses_meta_stream(),
    ));
    let mut provider = OpenAiProvider::new("key")
        .with_options(meta_allowlist())
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("buffered SSE-shaped Responses body succeeds");

    assert_meta_block(&response.response_metadata, "last");
}

#[tokio::test]
async fn allowlisted_meta_capture_is_last_wins_on_a_streaming_responses_request() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("content-type".to_string(), "text/event-stream".to_string())],
        responses_meta_stream(),
    ));
    let mut provider = OpenAiProvider::new("key")
        .with_options(meta_allowlist())
        .with_transport(transport);

    let response = provider
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("terminal stream succeeds");

    assert_meta_block(&response.response_metadata, "last");
}

#[tokio::test]
async fn without_an_allowlist_a_gateway_meta_block_captures_nothing() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"meta":{"routing":{"requested":"auto","served":"deepinfra"}}}"#,
    ));
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1").with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("request succeeds");

    assert!(
        response.response_metadata.is_empty(),
        "a buffered gateway meta block is not captured without an allowlist"
    );

    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("content-type".to_string(), "text/event-stream".to_string())],
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"meta\":{\"routing\":{\"requested\":\"auto\",\"served\":\"last\"}}}\n\n",
            "data: [DONE]\n\n"
        ),
    ));
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1").with_transport(transport);

    let response = provider
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("terminal stream succeeds");

    assert!(
        response.response_metadata.is_empty(),
        "a streamed gateway meta block is not captured without an allowlist"
    );
}
