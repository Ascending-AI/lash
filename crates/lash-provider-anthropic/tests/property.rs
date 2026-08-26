use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, LlmToolChoice,
    LlmToolSpec,
};
use lash_core::provider::Provider;
use lash_llm_transport::proptest_support::{ScriptedSseTransport, chunk_partitions};
use lash_provider_anthropic::AnthropicProvider;
use proptest::prelude::*;
use serde_json::json;

fn sse_frame(name: &str, payload: &serde_json::Value) -> String {
    format!("event: {name}\ndata: {payload}\n\n")
}

// Canonical text stream with usage, adapted from the conformance fixtures in
// `src/lib.rs`, with multibyte payloads so chunk splits can land inside a
// codepoint.
fn text_stream_bytes() -> Vec<u8> {
    [
        sse_frame(
            "message_start",
            &json!({ "type": "message_start", "message": { "usage": {
                "input_tokens": 12,
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 4
            } } }),
        ),
        sse_frame("ping", &json!({ "type": "ping" })),
        sse_frame(
            "content_block_start",
            &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text" } }),
        ),
        sse_frame(
            "content_block_delta",
            &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "héllo 😀 " } }),
        ),
        sse_frame(
            "content_block_delta",
            &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "世界" } }),
        ),
        sse_frame(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": 0 }),
        ),
        sse_frame(
            "message_delta",
            &json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": {
                "output_tokens": 13,
                "output_tokens_details": { "thinking_tokens": 3 }
            } }),
        ),
        sse_frame("message_stop", &json!({ "type": "message_stop" })),
    ]
    .concat()
    .into_bytes()
}

// Canonical tool-use stream with the tool input JSON split across two
// `input_json_delta` events, adapted from the conformance fixtures in
// `src/lib.rs`.
fn tool_call_stream_bytes() -> Vec<u8> {
    [
        sse_frame(
            "message_start",
            &json!({ "type": "message_start", "message": { "usage": { "input_tokens": 30 } } }),
        ),
        sse_frame(
            "content_block_start",
            &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "call_1", "name": "lookup", "input": {} } }),
        ),
        sse_frame(
            "content_block_delta",
            &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"q\":" } }),
        ),
        sse_frame(
            "content_block_delta",
            &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"日本 🚀\"}" } }),
        ),
        sse_frame(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": 0 }),
        ),
        sse_frame(
            "message_delta",
            &json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 9 } }),
        ),
        sse_frame("message_stop", &json!({ "type": "message_stop" })),
    ]
    .concat()
    .into_bytes()
}

fn request(deltas: Arc<Mutex<Vec<String>>>) -> LlmRequest {
    LlmRequest {
        model: "claude-sonnet-4-6".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "hello")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: Default::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(move |event| {
            if let LlmStreamEvent::Delta(piece) = event {
                deltas.lock_recover().push(piece);
            }
        })),
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    }
}

fn complete_with_chunks(chunks: Vec<Vec<u8>>) -> (LlmResponse, Vec<String>) {
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let mut provider = AnthropicProvider::new("test-key")
        .with_transport(Arc::new(ScriptedSseTransport::new(chunks)));
    let response = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(provider.complete(request(Arc::clone(&deltas))))
        .expect("canonical stream completes");
    let deltas = deltas.lock_recover().clone();
    (response, deltas)
}

fn assert_invariant_under_split(
    canonical: Vec<u8>,
    chunks: Vec<Vec<u8>>,
) -> Result<(), TestCaseError> {
    let (unsplit, unsplit_deltas) = complete_with_chunks(vec![canonical]);
    let (split, split_deltas) = complete_with_chunks(chunks);
    prop_assert_eq!(&split.parts, &unsplit.parts);
    prop_assert_eq!(split.full_text(), unsplit.full_text());
    prop_assert_eq!(split.usage, unsplit.usage);
    prop_assert_eq!(split.terminal_reason, unsplit.terminal_reason);
    prop_assert_eq!(split_deltas, unsplit_deltas);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn text_stream_parsing_is_invariant_under_arbitrary_chunk_splits(
        chunks in chunk_partitions(text_stream_bytes())
    ) {
        assert_invariant_under_split(text_stream_bytes(), chunks)?;
    }

    #[test]
    fn tool_call_stream_parsing_is_invariant_under_arbitrary_chunk_splits(
        chunks in chunk_partitions(tool_call_stream_bytes())
    ) {
        assert_invariant_under_split(tool_call_stream_bytes(), chunks)?;
    }
}
