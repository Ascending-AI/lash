//! Driver-seam tests: they drive the production streaming drivers through a
//! scripted byte stream instead of replaying SSE text into the parser, so the
//! wiring between the parser and the caller's stream sender is pinned, not just
//! the state machine. The conformance law's
//! `Scenario::StreamingToolCallAbortEquivalence` covers the parser side; these
//! cover the call sites that hand the completed tool call to the host — one per
//! endpoint, since Chat Completions and Responses have separate drivers and so
//! separate emission calls.

use super::*;
use lash_llm_transport::{LlmByteStream, LlmHttpResponse};

/// One scripted step of a response body: either bytes, or the transport-level
/// failure that models a server hanging up mid-stream.
#[derive(Debug)]
enum ScriptedByteEvent {
    Chunk(bytes::Bytes),
    Abort(LlmTransportError),
}

#[derive(Debug)]
struct ScriptedByteStream {
    events: VecDeque<ScriptedByteEvent>,
}

#[async_trait]
impl LlmByteStream for ScriptedByteStream {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
        match self.events.pop_front() {
            Some(ScriptedByteEvent::Chunk(chunk)) => Ok(Some(chunk)),
            Some(ScriptedByteEvent::Abort(error)) => Err(error),
            None => Ok(None),
        }
    }
}

/// Answers one request with a `text/event-stream` body backed by the scripted
/// steps above. A buffered body cannot express an abort, so this transport is
/// the only way to reach the driver's mid-stream failure path.
#[derive(Debug)]
struct AbortingSseTransport {
    events: std::sync::Mutex<Option<VecDeque<ScriptedByteEvent>>>,
}

impl AbortingSseTransport {
    fn new(events: Vec<ScriptedByteEvent>) -> Arc<Self> {
        Arc::new(Self {
            events: std::sync::Mutex::new(Some(VecDeque::from(events))),
        })
    }
}

#[async_trait]
impl LlmHttpTransport for AbortingSseTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let events = self
            .events
            .lock_recover()
            .take()
            .expect("scripted SSE stream is requested once");
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: LlmHttpBody::streamed(ScriptedByteStream { events }),
        })
    }
}

/// The `(call_id, tool_name, input_json)` of every tool call the driver handed
/// to the caller's stream sender, in emission order.
fn emitted_tool_calls(
    events: &Arc<std::sync::Mutex<Vec<LlmStreamEvent>>>,
) -> Vec<(String, String, String)> {
    events
        .lock_recover()
        .iter()
        .filter_map(|event| match event {
            LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            }) => Some((call_id.clone(), tool_name.clone(), input_json.clone())),
            _ => None,
        })
        .collect()
}

fn sse_chunk(payload: &str) -> ScriptedByteEvent {
    ScriptedByteEvent::Chunk(bytes::Bytes::from(format!("data: {payload}\n\n")))
}

const CHAT_TOOL_CALL_CHUNK: &str = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abort","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]}}]}"#;

/// A chat stream that aborts right after a complete tool call has arrived must
/// still have handed that tool call to the caller — the driver emits completed
/// tool calls as they land, and the post-stream drain is never reached on the
/// failure path. This is the driver seam the conformance law cannot reach:
/// removing the driver's per-event `LlmStreamEvent::Part` emission turns this
/// test red.
#[tokio::test]
async fn aborted_chat_stream_emits_the_completed_tool_call_through_the_driver() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = AbortingSseTransport::new(vec![
        sse_chunk(CHAT_TOOL_CALL_CHUNK),
        ScriptedByteEvent::Abort(
            LlmTransportError::new("Stream read failed: scripted disconnect")
                .with_kind(ProviderFailureKind::Stream)
                .retryable(true),
        ),
    ]);
    let mut provider = openrouter_provider().with_transport(transport);

    let error = provider
        .complete(streamed_request(Arc::clone(&events)))
        .await
        .expect_err("an aborted stream fails the turn");
    assert_eq!(error.kind, ProviderFailureKind::Stream);

    assert_eq!(
        emitted_tool_calls(&events),
        vec![(
            "call_abort".to_string(),
            "lookup".to_string(),
            r#"{"q":"x"}"#.to_string()
        )],
        "the driver must emit the completed tool call exactly once before the abort"
    );

    // The same tool call is also carried on the partial response, so a host that
    // only reads the error still sees the work that was paid for.
    let partial = error
        .partial_response
        .as_ref()
        .expect("an aborted stream reports its partial response");
    assert!(
        partial
            .parts
            .iter()
            .any(|part| matches!(part, LlmOutputPart::ToolCall { tool_name, .. } if tool_name == "lookup")),
        "partial response keeps the tool call: {:?}",
        partial.parts
    );
}

/// The Responses endpoint carries the GPT-5/Codex-class production turns, and it
/// has its own driver with its own emission call — `drive_streaming_chat` being
/// pinned says nothing about it. Same law, same abort shape: a function call that
/// completed before the transport hung up must already have reached the host.
#[tokio::test]
async fn aborted_responses_stream_emits_the_completed_tool_call_through_the_driver() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = AbortingSseTransport::new(vec![
        sse_chunk(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_abort","call_id":"call_abort","name":"lookup","arguments":""}}"#,
        ),
        sse_chunk(
            r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_abort","arguments":"{\"q\":\"x\"}"}"#,
        ),
        sse_chunk(
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_abort","call_id":"call_abort","name":"lookup","arguments":"{\"q\":\"x\"}"}}"#,
        ),
        ScriptedByteEvent::Abort(
            LlmTransportError::new("Stream read failed: scripted disconnect")
                .with_kind(ProviderFailureKind::Stream)
                .retryable(true),
        ),
    ]);
    let mut provider = OpenAiProvider::new("key").with_transport(transport);

    let error = provider
        .complete(streamed_request(Arc::clone(&events)))
        .await
        .expect_err("an aborted stream fails the turn");
    assert_eq!(error.kind, ProviderFailureKind::Stream);

    assert_eq!(
        emitted_tool_calls(&events),
        vec![(
            "call_abort".to_string(),
            "lookup".to_string(),
            r#"{"q":"x"}"#.to_string()
        )],
        "the Responses driver must emit the completed tool call exactly once before the abort"
    );

    let partial = error
        .partial_response
        .as_ref()
        .expect("an aborted stream reports its partial response");
    assert!(
        partial
            .parts
            .iter()
            .any(|part| matches!(part, LlmOutputPart::ToolCall { tool_name, .. } if tool_name == "lookup")),
        "partial response keeps the tool call: {:?}",
        partial.parts
    );
}
