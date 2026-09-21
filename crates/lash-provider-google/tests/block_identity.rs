//! FIG-3371 review regression: Google has no native block ids, so each
//! contiguous text run gets its own deterministic ordinal block. A text run
//! interrupted by reasoning or a tool call must close and re-open as a new
//! block — never merge into one id.

use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmRequest, LlmRole, LlmStreamEvent, LlmToolChoice, LlmToolSpec,
};
use lash_core::provider::Provider;
use lash_llm_transport::proptest_support::ScriptedSseTransport;
use lash_provider_google::GoogleOAuthProvider;
use serde_json::json;

fn sse_frame(payload: &serde_json::Value) -> String {
    format!("data: {payload}\r\n\r\n")
}

/// text -> functionCall -> text: two separate text runs the old single
/// `text_block` used to merge under one id.
fn text_tool_text_stream_bytes() -> Vec<u8> {
    [
        sse_frame(&json!({ "response": {
            "candidates": [{ "content": { "parts": [{ "text": "before the call" }] } }]
        } })),
        sse_frame(&json!({ "response": {
            "candidates": [{
                "content": { "parts": [{ "functionCall": {
                    "id": "call_1",
                    "name": "lookup",
                    "args": { "q": "x" }
                } }] }
            }]
        } })),
        sse_frame(&json!({ "response": {
            "candidates": [{
                "content": { "parts": [{ "text": "after the call" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 8, "candidatesTokenCount": 4 }
        } })),
    ]
    .concat()
    .into_bytes()
}

fn request(events: Arc<Mutex<Vec<LlmStreamEvent>>>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "gemini-3.1-pro-preview".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "hello")],
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
            events.lock_recover().push(event);
        })),
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    }
}

#[test]
fn text_runs_around_a_tool_call_get_distinct_sealed_blocks() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut provider = GoogleOAuthProvider::new(
        "access-token",
        "refresh-token",
        u64::MAX,
        lash_provider_google::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_project_id(Some("project-1".to_string()))
    .with_transport(Arc::new(ScriptedSseTransport::new(vec![
        text_tool_text_stream_bytes(),
    ])));

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(provider.complete(request(Arc::clone(&events))))
        .expect("canonical stream completes");

    let events = events.lock_recover();
    let started: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            LlmStreamEvent::TextBlockStart { block } => Some(block.id.clone()),
            _ => None,
        })
        .collect();
    let ended: Vec<(String, String)> = events
        .iter()
        .filter_map(|event| match event {
            LlmStreamEvent::TextBlockEnd { block, text } => Some((block.id.clone(), text.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(
        started.len(),
        2,
        "each text run opens its own block: {events:?}"
    );
    assert_ne!(started[0], started[1], "runs have distinct identities");
    assert_eq!(
        ended.len(),
        2,
        "each run seals independently at its boundary"
    );
    assert_eq!(
        ended,
        vec![
            (started[0].clone(), "before the call".to_string()),
            (started[1].clone(), "after the call".to_string()),
        ],
        "each block seals with only its own run's text"
    );
}
