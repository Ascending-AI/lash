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

/// Serves one interrupted Responses stream followed by its reattachment, while
/// retaining both requests so the resume wire contract is observable.
#[derive(Debug)]
struct ResumableSseTransport {
    responses: std::sync::Mutex<VecDeque<VecDeque<ScriptedByteEvent>>>,
    content_types: std::sync::Mutex<VecDeque<String>>,
    requests: std::sync::Mutex<Vec<LlmHttpRequest>>,
}

impl ResumableSseTransport {
    fn new(responses: Vec<Vec<ScriptedByteEvent>>) -> Arc<Self> {
        let content_types = vec!["text/event-stream".to_string(); responses.len()];
        Self::with_content_types(responses, content_types)
    }

    fn with_content_types(
        responses: Vec<Vec<ScriptedByteEvent>>,
        content_types: Vec<String>,
    ) -> Arc<Self> {
        assert_eq!(responses.len(), content_types.len());
        Arc::new(Self {
            responses: std::sync::Mutex::new(responses.into_iter().map(VecDeque::from).collect()),
            content_types: std::sync::Mutex::new(content_types.into()),
            requests: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<LlmHttpRequest> {
        self.requests.lock_recover().clone()
    }
}

#[async_trait]
impl LlmHttpTransport for ResumableSseTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let request_ordinal = {
            let mut requests = self.requests.lock_recover();
            requests.push(request);
            requests.len()
        };
        let events = self
            .responses
            .lock_recover()
            .pop_front()
            .expect("scripted Responses stream");
        let content_type = self
            .content_types
            .lock_recover()
            .pop_front()
            .expect("scripted Responses content type");
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![
                ("content-type".to_string(), content_type),
                ("x-request-id".to_string(), format!("req_{request_ordinal}")),
            ],
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
                .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
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
                .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
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

#[tokio::test]
async fn responses_handle_resumes_after_the_last_sequence_without_duplicate_output_or_usage() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let evidence_errors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = ResumableSseTransport::new(vec![
        vec![
            sse_chunk(
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_resume","status":"in_progress"}}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"hello "}"#,
            ),
            ScriptedByteEvent::Abort(
                LlmTransportError::new("Stream read failed: scripted disconnect")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
            ),
        ],
        vec![
            // The API may replay the cursor event. The adapter must apply the
            // same strict client-side filter as the official SDK.
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"hello "}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":2,"output_index":0,"delta":"world"}"#,
            ),
            sse_chunk(
                r#"{"type":"response.completed","sequence_number":3,"response":{"id":"resp_resume","status":"completed","output":[{"type":"message","id":"msg_resume","status":"completed","content":[{"type":"output_text","text":"hello world"}]}],"usage":{"input_tokens":11,"output_tokens":2,"total_tokens":13}}}"#,
            ),
        ],
    ]);
    let mut provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(Arc::clone(&transport) as _);
    provider
        .inner
        .wire
        .query_params
        .push(("api-version".to_string(), "preview".to_string()));
    let mut handle = ProviderHandle::new(provider.into_components());

    let mut request = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    request.stream_events = Some(LlmEventSender::new({
        let events = Arc::clone(&events);
        let evidence_errors = Arc::clone(&evidence_errors);
        let merged_evidence = Arc::new(std::sync::Mutex::new(LlmStreamEvidence::default()));
        move |event| {
            if let LlmStreamEvent::Evidence(evidence) = &event
                && let Err(error) = merged_evidence.lock_recover().merge(evidence.clone())
            {
                evidence_errors.lock_recover().push(error.to_string());
            }
            events.lock_recover().push(event);
        }
    }));

    let completion = handle
        .complete(request)
        .await
        .expect("the interrupted Responses generation resumes");

    let requests = transport.requests();
    assert_eq!(requests.len(), 2, "one creation and one reattachment");
    assert_eq!(requests[0].method, LlmHttpMethod::Post);
    assert_eq!(requests[1].method, LlmHttpMethod::Get);
    assert_eq!(
        requests[1].url,
        "https://api.openai.com/v1/responses/resp_resume?api-version=preview&starting_after=1&stream=true",
        "static wire query parameters are preserved on the resume URL"
    );
    assert!(requests[1].body.is_empty());

    assert_eq!(completion.response.full_text, "hello world");
    assert_eq!(
        completion
            .response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        1,
        "the committed turn contains one text output part"
    );
    assert_eq!(
        completion
            .call_record
            .attempts
            .iter()
            .filter(|attempt| attempt.outcome == lash_core::AttemptOutcome::Completed)
            .count(),
        1,
        "the logical generation has exactly one terminal response"
    );
    assert_eq!(completion.response.usage.input_tokens, 11);
    assert_eq!(completion.response.usage.output_tokens, 2);

    let events = events.lock_recover();
    let deltas = events
        .iter()
        .filter_map(|event| match event {
            LlmStreamEvent::Delta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas, vec!["hello ", "world"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LlmStreamEvent::Usage(_)))
            .count(),
        1,
        "terminal cumulative usage is observed once"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmStreamEvent::AttemptReset)),
        "a resumed generation keeps the existing stream accumulator"
    );
    assert_eq!(
        *evidence_errors.lock_recover(),
        Vec::<String>::new(),
        "reattachment must not conflict with the logical generation's live evidence"
    );
}

#[tokio::test]
async fn responses_checkpoint_does_not_resume_a_different_logical_call() {
    let transport = ResumableSseTransport::new(vec![
        vec![
            sse_chunk(
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_call_a","status":"in_progress"}}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"A partial"}"#,
            ),
            ScriptedByteEvent::Abort(
                LlmTransportError::new("Stream read failed: scripted disconnect")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
            ),
        ],
        vec![
            sse_chunk(
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_call_b","status":"in_progress"}}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"B output"}"#,
            ),
            sse_chunk(
                r#"{"type":"response.completed","sequence_number":2,"response":{"id":"resp_call_b","status":"completed","output":[{"type":"message","id":"msg_call_b","status":"completed","content":[{"type":"output_text","text":"B output"}]}]}}"#,
            ),
        ],
    ]);
    let provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default().max_attempts(1),
            ..ProviderOptions::default()
        })
        .with_transport(Arc::clone(&transport) as _);
    let mut handle = ProviderHandle::new(provider.into_components());

    let mut call_a = streamed_request(Arc::new(std::sync::Mutex::new(Vec::new())));
    call_a.messages = vec![LlmMessage::text(LlmRole::User, "call A")];
    handle
        .complete(call_a)
        .await
        .expect_err("call A exhausts its retry budget after interruption");

    let mut call_b = streamed_request(Arc::new(std::sync::Mutex::new(Vec::new())));
    call_b.messages = vec![LlmMessage::text(LlmRole::User, "call B")];
    let completion = handle
        .complete(call_b)
        .await
        .expect("call B starts and completes a fresh generation");

    let requests = transport.requests();
    assert_eq!(requests.len(), 2, "one request per logical call");
    assert_eq!(requests[0].method, LlmHttpMethod::Post);
    assert_eq!(
        requests[1].method,
        LlmHttpMethod::Post,
        "a different request body must not resume call A"
    );
    assert!(
        String::from_utf8_lossy(&requests[1].body).contains("call B"),
        "the fresh request carries call B's prompt"
    );
    assert_eq!(completion.response.full_text, "B output");
}

#[tokio::test]
async fn responses_resume_event_without_sequence_number_fails_closed() {
    let transport = ResumableSseTransport::new(vec![
        vec![
            sse_chunk(
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_missing_sequence","status":"in_progress"}}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"partial"}"#,
            ),
            ScriptedByteEvent::Abort(
                LlmTransportError::new("Stream read failed: scripted disconnect")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
            ),
        ],
        vec![sse_chunk(
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"unsafe"}"#,
        )],
    ]);
    let provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(Arc::clone(&transport) as _);
    let mut handle = ProviderHandle::new(provider.into_components());

    let failure = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect_err("a resume event without a sequence number is unsafe");

    assert_eq!(
        failure.code.as_deref(),
        Some("responses_resume_event_missing_sequence")
    );
    assert_eq!(transport.requests().len(), 2, "creation then resume");
    assert_eq!(transport.requests()[1].method, LlmHttpMethod::Get);
}

#[tokio::test]
async fn responses_resume_response_without_event_stream_fails_closed() {
    let transport = ResumableSseTransport::with_content_types(
        vec![
            vec![
                sse_chunk(
                    r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_not_streaming","status":"in_progress"}}"#,
                ),
                sse_chunk(
                    r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"partial"}"#,
                ),
                ScriptedByteEvent::Abort(
                    LlmTransportError::new("Stream read failed: scripted disconnect")
                        .with_kind(ProviderFailureKind::Stream)
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
                ),
            ],
            vec![ScriptedByteEvent::Chunk(bytes::Bytes::from(
                r#"{"id":"resp_not_streaming","status":"completed"}"#,
            ))],
        ],
        vec![
            "text/event-stream".to_string(),
            "application/json".to_string(),
        ],
    );
    let provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(Arc::clone(&transport) as _);
    let mut handle = ProviderHandle::new(provider.into_components());

    let failure = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect_err("a resume response must be an event stream");

    assert_eq!(
        failure.code.as_deref(),
        Some("responses_resume_not_streaming")
    );
    assert_eq!(transport.requests().len(), 2, "creation then resume");
    assert_eq!(transport.requests()[1].method, LlmHttpMethod::Get);
}

#[tokio::test]
async fn retry_guarantee_stays_none_without_a_response_id_and_for_chat_completions() {
    let transport = ResumableSseTransport::new(vec![
        vec![
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":0,"output_index":0,"delta":"paid"}"#,
            ),
            ScriptedByteEvent::Abort(
                LlmTransportError::new("Stream read failed: scripted disconnect")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
            ),
        ],
        vec![sse_chunk(
            r#"{"type":"response.completed","sequence_number":1,"response":{"id":"resp_too_late","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"replacement"}]}]}}"#,
        )],
    ]);
    let provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(Arc::clone(&transport) as _);
    let mut handle = ProviderHandle::new(provider.into_components());

    let failure = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect_err("output without a response id cannot be reattached");

    assert_eq!(transport.requests().len(), 1, "resume path is unreachable");
    assert_eq!(
        failure.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );

    let no_sequence_transport = AbortingSseTransport::new(vec![
        sse_chunk(
            r#"{"type":"response.created","response":{"id":"resp_without_cursor","status":"in_progress"}}"#,
        ),
        ScriptedByteEvent::Abort(
            LlmTransportError::new("Stream read failed: scripted disconnect")
                .with_kind(ProviderFailureKind::Stream)
                .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
        ),
    ]);
    let mut no_sequence = OpenAiProvider::new("key").with_transport(no_sequence_transport);
    let no_sequence_request = streamed_request(Arc::new(std::sync::Mutex::new(Vec::new())));
    no_sequence
        .complete(no_sequence_request.clone())
        .await
        .expect_err("a response id without a sequence cursor remains interrupted");
    assert_eq!(
        no_sequence.generation_retry_guarantee(&no_sequence_request),
        GenerationRetryGuarantee::None,
        "a response id alone cannot prove replay filtering"
    );

    let chat = openrouter_provider();
    let chat_request = streamed_request(Arc::new(std::sync::Mutex::new(Vec::new())));
    assert_eq!(
        chat.generation_retry_guarantee(&chat_request),
        GenerationRetryGuarantee::None,
        "Chat Completions never claims the Responses resume contract"
    );
}

#[tokio::test]
async fn responses_resume_keeps_cumulative_usage_as_one_generation_bill() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = ResumableSseTransport::new(vec![
        vec![
            sse_chunk(
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_usage","status":"in_progress","usage":{"input_tokens":11,"output_tokens":1,"total_tokens":12}}}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"a"}"#,
            ),
            ScriptedByteEvent::Abort(
                LlmTransportError::new("Stream read failed: scripted disconnect")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
            ),
        ],
        vec![
            sse_chunk(
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_usage","status":"in_progress","usage":{"input_tokens":11,"output_tokens":1,"total_tokens":12}}}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":1,"output_index":0,"delta":"a"}"#,
            ),
            sse_chunk(
                r#"{"type":"response.output_text.delta","sequence_number":2,"output_index":0,"delta":"b"}"#,
            ),
            sse_chunk(
                r#"{"type":"response.completed","sequence_number":3,"response":{"id":"resp_usage","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"ab"}]}],"usage":{"input_tokens":11,"output_tokens":2,"total_tokens":13}}}"#,
            ),
        ],
    ]);
    let provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(transport);
    let mut handle = ProviderHandle::new(provider.into_components());

    let completion = handle
        .complete(streamed_request(Arc::clone(&events)))
        .await
        .expect("the usage-bearing generation resumes");

    assert_eq!(completion.response.full_text, "ab");
    assert_eq!(completion.response.usage.input_tokens, 11);
    assert_eq!(completion.response.usage.output_tokens, 2);
    let cumulative_usage = events
        .lock_recover()
        .iter()
        .filter_map(|event| match event {
            LlmStreamEvent::Usage(usage) => Some((usage.input_tokens, usage.output_tokens)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        cumulative_usage,
        vec![(11, 1), (11, 2)],
        "resume advances one cumulative usage snapshot instead of adding a second bill"
    );
}
