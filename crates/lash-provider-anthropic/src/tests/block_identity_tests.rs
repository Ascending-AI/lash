//! FIG-3371 review regressions: reasoning visibility gating and the
/// content-block identity unsigned thinking parts must still carry.
use crate::AnthropicProvider;
use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmRole, LlmStreamEvent, LlmToolChoice,
    LlmToolSpec,
};
use lash_core::provider::{Provider, ProviderOptions};
use lash_core::sync::MutexExt;
use std::sync::Arc;

#[derive(Debug)]
struct StaticSseTransport(&'static str);

#[async_trait::async_trait]
impl lash_llm_transport::LlmHttpTransport for StaticSseTransport {
    async fn send(
        &self,
        _request: lash_llm_transport::LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::facade_support::LlmTransportError>
    {
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: lash_llm_transport::LlmHttpBody::buffered(self.0),
        })
    }
}

fn request(messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "claude-sonnet-4-6".to_string(),
        messages,
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: crate::attachment_test_capability(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: None,
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    }
}

const THINKING_STREAM_UNSIGNED: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"private chain\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"public answer\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn capture() -> (Arc<std::sync::Mutex<Vec<LlmStreamEvent>>>, LlmEventSender) {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    (
        events,
        LlmEventSender::new(move |event| sink.lock_recover().push(event)),
    )
}

/// With `expose_thinking` off the republish path must not leak the thinking
/// text either: no reasoning block events and no `Part(Reasoning)` reach
/// the host, while the visible text still streams.
#[tokio::test]
async fn hidden_thinking_stream_emits_no_reasoning_events() {
    let (events, sender) = capture();
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
    req.stream_events = Some(sender);
    let mut provider = AnthropicProvider::new("key")
        .with_options(ProviderOptions {
            expose_thinking: false,
            ..ProviderOptions::default()
        })
        .with_transport(Arc::new(StaticSseTransport(THINKING_STREAM_UNSIGNED)));

    provider
        .complete(req)
        .await
        .expect("thinking stream completes");

    let events = events.lock_recover();
    assert!(
        events.iter().all(|event| !matches!(
            event,
            LlmStreamEvent::ReasoningBlockStart { .. }
                | LlmStreamEvent::ReasoningDelta { .. }
                | LlmStreamEvent::ReasoningBlockEnd { .. }
                | LlmStreamEvent::Part(LlmOutputPart::Reasoning { .. })
        )),
        "hidden thinking must not republish reasoning to the host: {events:?}"
    );
    assert!(
        events.iter().any(
            |event| matches!(event, LlmStreamEvent::Delta { text, .. } if text == "public answer")
        ),
        "visible text still streams: {events:?}"
    );
}

/// Unsigned thinking blocks still get the deterministic
/// `content_block:{index}` item identity — without it the runtime's
/// republication join treats the completed part as anonymous and emits it a
/// second time.
#[tokio::test]
async fn unsigned_thinking_part_carries_content_block_item_id() {
    let (events, sender) = capture();
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
    req.stream_events = Some(sender);
    let mut provider = AnthropicProvider::new("key")
        .with_options(ProviderOptions {
            expose_thinking: true,
            ..ProviderOptions::default()
        })
        .with_transport(Arc::new(StaticSseTransport(THINKING_STREAM_UNSIGNED)));

    let response = provider
        .complete(req)
        .await
        .expect("thinking stream completes");

    let replay = match &response.parts[0] {
        LlmOutputPart::Reasoning {
            replay: Some(replay),
            ..
        } => replay,
        other => panic!("expected reasoning replay, got {other:?}"),
    };
    assert_eq!(
        replay.item_id.as_deref(),
        Some("content_block:0"),
        "unsigned thinking still names its content block so the completed part joins the streamed block"
    );
    let streamed = events.lock_recover();
    assert!(
        streamed.iter().any(|event| matches!(
            event,
            LlmStreamEvent::ReasoningBlockStart { block }
                if block.item_id.as_deref() == Some("content_block:0")
        )),
        "the live block carries the same identity the completed part joins on: {streamed:?}"
    );
}

#[tokio::test]
async fn streamed_reasoning_parts_are_stamped_at_the_anthropic_boundary() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"summary\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"native-signature\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let event_sink = Arc::clone(&events);
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.stream_events = Some(LlmEventSender::new(move |event| {
        event_sink.lock_recover().push(event);
    }));
    let mut provider = AnthropicProvider::new("key")
        .with_options(ProviderOptions {
            expose_thinking: true,
            ..ProviderOptions::default()
        })
        .with_transport(Arc::new(StaticSseTransport(body)));
    let expected_route = provider.route_identity("claude-sonnet-4-6");

    let response = provider
        .complete(req)
        .await
        .expect("thinking stream completes");
    let replay = match &response.parts[0] {
        LlmOutputPart::Reasoning {
            replay: Some(replay),
            ..
        } => replay,
        other => panic!("expected reasoning replay, got {other:?}"),
    };
    assert_eq!(replay.origin.as_ref(), Some(&expected_route));
    assert!(events.lock_recover().iter().any(|event| {
        matches!(
            event,
            LlmStreamEvent::Part(LlmOutputPart::Reasoning {
                replay: Some(replay),
                ..
            }) if replay.origin.as_ref() == Some(&expected_route)
        )
    }));
}
