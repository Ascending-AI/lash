use std::sync::{Arc, Mutex};

use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent,
    LlmToolChoice, LlmToolSpec,
};
use lash_core::provider::Provider;
use lash_llm_transport::proptest_support::{ScriptedSseTransport, chunk_partitions};
use lash_provider_google::GoogleOAuthProvider;
use proptest::prelude::*;
use serde_json::json;

fn sse_frame(payload: &serde_json::Value) -> String {
    format!("data: {payload}\r\n\r\n")
}

// Canonical text stream with usage, adapted from the example tests in
// `src/lib.rs` (same usageMetadata buckets), with multibyte payloads so chunk
// splits can land inside a codepoint.
fn text_stream_bytes() -> Vec<u8> {
    [
        sse_frame(&json!({ "response": {
            "candidates": [{ "content": { "parts": [{ "text": "hé" }] } }],
            "usageMetadata": { "promptTokenCount": 21, "cachedContentTokenCount": 5 }
        } })),
        sse_frame(&json!({ "response": {
            "candidates": [{ "content": { "parts": [{ "text": "llo 😀" }] } }]
        } })),
        sse_frame(&json!({ "response": {
            "candidates": [{
                "content": { "parts": [{ "text": " 世界" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 21,
                "cachedContentTokenCount": 5,
                "candidatesTokenCount": 10,
                "thoughtsTokenCount": 3
            }
        } })),
    ]
    .concat()
    .into_bytes()
}

// Canonical function-call stream. The functionCall carries an explicit `id`
// (as Cloud Code sends) so the assembled tool call is deterministic.
fn function_call_stream_bytes() -> Vec<u8> {
    [
        sse_frame(&json!({ "response": {
            "candidates": [{ "content": { "parts": [{ "text": "Looking that up." }] } }]
        } })),
        sse_frame(&json!({ "response": {
            "candidates": [{
                "content": { "parts": [{ "functionCall": {
                    "id": "call_1",
                    "name": "lookup",
                    "args": { "q": "日本 🚀" }
                } }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 8, "candidatesTokenCount": 4 }
        } })),
    ]
    .concat()
    .into_bytes()
}

fn reasoning_stream_bytes() -> Vec<u8> {
    [
        sse_frame(&json!({ "response": {
            "candidates": [{ "content": { "parts": [{
                "text": "réason ",
                "thought": true
            }] } }]
        } })),
        sse_frame(&json!({ "response": {
            "candidates": [{ "content": { "parts": [{
                "text": "carefully 😀",
                "thought": true,
                "thoughtSignature": "c3BsaXQtaW52YXJpYW50LXNpZw=="
            }] } }]
        } })),
        sse_frame(&json!({ "response": {
            "candidates": [{
                "content": { "parts": [{ "text": "answer" }] },
                "finishReason": "STOP"
            }]
        } })),
    ]
    .concat()
    .into_bytes()
}

fn request(deltas: Arc<Mutex<Vec<String>>>) -> LlmRequest {
    LlmRequest {
        model: "gemini-3.1-pro-preview".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "hello")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::provider::ModelCapability::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(move |event| {
            if let LlmStreamEvent::Delta(piece) = event {
                deltas.lock().expect("delta collector").push(piece);
            }
        })),
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    }
}

fn complete_with_chunks(chunks: Vec<Vec<u8>>) -> (LlmResponse, Vec<String>) {
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let mut provider = GoogleOAuthProvider::new("access-token", "refresh-token", u64::MAX)
        .with_project_id(Some("project-1".to_string()))
        .with_transport(Arc::new(ScriptedSseTransport::new(chunks)));
    let response = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(provider.complete(request(Arc::clone(&deltas))))
        .expect("canonical stream completes");
    let deltas = deltas.lock().expect("delta collector").clone();
    (response, deltas)
}

fn assert_invariant_under_split(
    canonical: Vec<u8>,
    chunks: Vec<Vec<u8>>,
) -> Result<(), TestCaseError> {
    let (unsplit, unsplit_deltas) = complete_with_chunks(vec![canonical]);
    let (split, split_deltas) = complete_with_chunks(chunks);
    prop_assert_eq!(split.parts, unsplit.parts);
    prop_assert_eq!(split.full_text, unsplit.full_text);
    prop_assert_eq!(split.usage, unsplit.usage);
    prop_assert_eq!(split.terminal_reason, unsplit.terminal_reason);
    prop_assert_eq!(split_deltas, unsplit_deltas);
    Ok(())
}

fn assert_reasoning_invariant_under_split(
    canonical: Vec<u8>,
    chunks: Vec<Vec<u8>>,
) -> Result<(), TestCaseError> {
    let (unsplit, unsplit_deltas) = complete_with_chunks(vec![canonical]);
    let (split, split_deltas) = complete_with_chunks(chunks);
    prop_assert_eq!(&split.parts, &unsplit.parts);
    prop_assert_eq!(&split.full_text, &unsplit.full_text);
    prop_assert_eq!(&split.terminal_reason, &unsplit.terminal_reason);
    prop_assert_eq!(&split_deltas, &unsplit_deltas);
    let reasoning = split.parts.iter().find_map(|part| match part {
        LlmOutputPart::Reasoning { text, replay } => Some((text, replay)),
        _ => None,
    });
    prop_assert!(matches!(
        reasoning,
        Some((text, Some(replay)))
            if text == "réason carefully 😀"
                && replay.signature.as_deref() == Some("c3BsaXQtaW52YXJpYW50LXNpZw==")
    ));
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
    fn function_call_stream_parsing_is_invariant_under_arbitrary_chunk_splits(
        chunks in chunk_partitions(function_call_stream_bytes())
    ) {
        assert_invariant_under_split(function_call_stream_bytes(), chunks)?;
    }

    #[test]
    fn reasoning_stream_parsing_is_invariant_under_arbitrary_chunk_splits(
        chunks in chunk_partitions(reasoning_stream_bytes())
    ) {
        assert_reasoning_invariant_under_split(reasoning_stream_bytes(), chunks)?;
    }
}
