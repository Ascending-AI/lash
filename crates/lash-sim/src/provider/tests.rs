use super::transport::execute_script;
use super::*;
use std::time::Duration;

use lash_core::llm::transport::ProviderFailureKind;
use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmRole, LlmStreamEvent,
    LlmTerminalReason, LlmToolChoice, LlmToolSpec,
};
use lash_core::provider::{
    DefaultProviderFailureClassifier, Provider, ProviderFailureClassifier, ProviderOptions,
    ProviderReliability, RequestTimeout,
};
use lash_llm_transport::LlmHttpBody;
use lash_provider_openai::{OpenAiCompatibleProvider, OpenAiProvider};
use serde_json::json;

use crate::canonical_scripts::{
    CANONICAL_SCRIPTS, OPENAI_COMPAT_DISCONNECT, OPENAI_COMPAT_RATE_LIMIT, OPENAI_COMPAT_TOOL_CALL,
    OPENAI_COMPAT_VALIDATION, OPENAI_RESPONSES_TEXT,
};

const RATE_LIMIT_BODY: &str = "{\"error\":{\"message\":\"Rate limit reached for requests\",\"type\":\"rate_limit_error\",\"code\":\"rate_limit_exceeded\"}}";
const VALIDATION_BODY: &str = "{\"error\":{\"message\":\"Invalid request: tools[0].function.parameters is invalid\",\"type\":\"invalid_request_error\",\"code\":\"invalid_request_error\"}}";

#[tokio::test]
async fn provider_wire_script_openai_compatible_chat_stream_uses_real_provider_parser() {
    let (events, sender) = event_collector();
    let mut provider = scripted_provider(OPENAI_COMPAT_TOOL_CALL);

    let response = provider
        .complete(request(Some(sender)))
        .await
        .expect("scripted response");

    assert_eq!(response.terminal_reason, LlmTerminalReason::ToolUse);
    assert_eq!(response.full_text(), "café ");
    let deltas = text_deltas(&events);
    assert_eq!(deltas, vec!["café ".to_string()]);

    let tool_call = response
        .parts
        .iter()
        .find_map(|part| match part {
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            } => Some((call_id, tool_name, input_json)),
            _ => None,
        })
        .expect("tool call part");
    assert_eq!(tool_call.0, "call_1");
    assert_eq!(tool_call.1, "lookup");
    assert_eq!(tool_call.2, "{\"q\":\"x\"}");
}

#[tokio::test]
async fn provider_wire_script_openai_compatible_rate_limit_error_preserves_envelope() {
    let mut provider = scripted_provider(OPENAI_COMPAT_RATE_LIMIT);

    let err = provider
        .complete(request(None))
        .await
        .expect_err("rate limit error");

    assert_eq!(err.status, Some(429));
    assert_eq!(
        err.raw.as_deref().map(String::as_str),
        Some(RATE_LIMIT_BODY)
    );
    assert_eq!(
        err.headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
            .count(),
        2
    );
    assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
    assert_eq!(request_body_json(&err)["model"], "openai/gpt-5.4");
    assert_eq!(request_body_json(&err)["stream"], false);
    assert_eq!(
        err.retry_verdict,
        TransportRetryVerdict::RetryableThrottle {
            retry_after: Some(Duration::from_secs(7)),
        }
    );

    let classified = DefaultProviderFailureClassifier.classify(err);
    assert_eq!(classified.kind, ProviderFailureKind::Quota);
    assert!(classified.is_retryable());
    assert_eq!(classified.retry_after(), Some(Duration::from_secs(7)));
}

#[tokio::test]
async fn provider_wire_script_openai_compatible_validation_error_preserves_envelope() {
    let mut provider = scripted_provider(OPENAI_COMPAT_VALIDATION);

    let err = provider
        .complete(request(None))
        .await
        .expect_err("validation error");

    assert_eq!(err.status, Some(400));
    assert_eq!(
        err.raw.as_deref().map(String::as_str),
        Some(VALIDATION_BODY)
    );
    assert_eq!(
        err.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
            .map(|(_, value)| value.as_str()),
        Some("req-validation")
    );
    assert_eq!(
        request_body_json(&err)["tools"][0]["function"]["name"],
        "lookup"
    );
    assert!(!err.is_retryable());

    let classified = DefaultProviderFailureClassifier.classify(err);
    assert_eq!(classified.kind, ProviderFailureKind::Validation);
    assert!(!classified.is_retryable());
}

#[tokio::test]
async fn provider_wire_script_openai_compatible_mid_stream_disconnect_surfaces_stream_error() {
    let (events, sender) = event_collector();
    let mut provider = scripted_provider(OPENAI_COMPAT_DISCONNECT);

    let err = provider
        .complete(request(Some(sender)))
        .await
        .expect_err("mid-stream disconnect");

    assert_eq!(err.kind, ProviderFailureKind::Stream);
    assert!(err.is_retryable());
    assert!(err.message.contains("scripted socket closed"));
    assert_eq!(text_deltas(&events), vec!["partial".to_string()]);
}

#[tokio::test]
async fn provider_wire_script_direct_openai_responses_uses_real_provider_parser() {
    let transport =
        Arc::new(ScriptedLlmHttpTransport::from_json_str(OPENAI_RESPONSES_TEXT).unwrap());
    let mut provider = OpenAiProvider::new("test-key").with_transport(transport);

    let response = provider
        .complete(responses_request())
        .await
        .expect("scripted OpenAI Responses response");

    assert_eq!(response.terminal_reason, LlmTerminalReason::Stop);
    assert_eq!(response.full_text(), "Direct answer.");
    assert_eq!(response.usage.input_tokens, 5);
    assert_eq!(response.usage.output_tokens, 2);
}

#[tokio::test]
async fn provider_wire_script_cancellation_before_response_start_commits_no_output() {
    let schedule = ScriptedTransportSchedule::new();
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_json_str(OPENAI_COMPAT_TOOL_CALL)
            .unwrap()
            .with_event_schedule(schedule.clone()),
    );
    let (events, sender) = event_collector();
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(transport);

    let task = tokio::spawn(async move { provider.complete(request(Some(sender))).await });
    schedule.wait_until_blocked(0, 0).await;
    task.abort();

    let join_err = task.await.expect_err("cancelled provider task");
    assert!(join_err.is_cancelled());
    assert!(
        events.lock_recover().is_empty(),
        "no stream events should be committed before response start"
    );
}

#[tokio::test]
async fn scripted_transport_response_start_gate_timeout_uses_production_timeout_envelope() {
    let schedule = ScriptedTransportSchedule::new();
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_json_str(OPENAI_COMPAT_TOOL_CALL)
            .unwrap()
            .with_event_schedule(schedule),
    );
    let provider_transport: Arc<dyn lash_llm_transport::LlmHttpTransport> = transport.clone();
    let (events, sender) = event_collector();
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .request_timeout(Some(RequestTimeout::Millis(1)))
                .stream_chunk_timeout_ms(Some(1)),
            ..ProviderOptions::default()
        })
        .with_transport(provider_transport);

    let err = provider
        .complete(request(Some(sender)))
        .await
        .expect_err("response start gate should time out");

    assert_eq!(err.kind, ProviderFailureKind::Timeout);
    assert_eq!(err.code.as_deref(), Some("timeout"));
    assert!(err.is_retryable());
    assert!(err.status.is_none());
    assert!(
        events.lock_recover().is_empty(),
        "response-start timeout should not commit stream events"
    );

    let exchanges = transport.exchanges().expect("exchange log");
    assert_eq!(exchanges.len(), 1);
    assert_eq!(exchanges[0].request.path, "/chat/completions");
    assert!(
        exchanges[0]
            .request
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("authorization")
                && header.value == "[redacted]")
    );
    assert!(exchanges[0].response.event_names.is_empty());
}

#[tokio::test]
async fn scripted_transport_buffers_scheduler_releases_before_provider_parks() {
    let schedule = ScriptedTransportSchedule::new();
    let script = ProviderWireScript::from_json_str(OPENAI_COMPAT_TOOL_CALL).expect("script");
    for (event_index, wire_event) in script.timeline().iter().enumerate() {
        let release = schedule.release(0, event_index, wire_event.event_name(), wire_event.at());
        assert!(
            !release.blocked_before_release,
            "regression setup must release before the provider parks on event {event_index}"
        );
    }
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_scripts([script])
            .expect("valid scripted provider")
            .with_event_schedule(schedule),
    );
    let provider_transport: Arc<dyn lash_llm_transport::LlmHttpTransport> = transport.clone();
    let (events, sender) = event_collector();
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .request_timeout(Some(RequestTimeout::Millis(1)))
                .stream_chunk_timeout_ms(Some(1)),
            ..ProviderOptions::default()
        })
        .with_transport(provider_transport);

    let response = provider
        .complete(request(Some(sender)))
        .await
        .expect("early scheduler releases must be buffered, not retried into no-script");

    assert_eq!(response.terminal_reason, LlmTerminalReason::ToolUse);
    assert_eq!(response.full_text(), "café ");
    assert_eq!(text_deltas(&events), vec!["café ".to_string()]);
    assert_eq!(transport.remaining_scripts().expect("remaining scripts"), 0);
    let exchanges = transport.exchanges().expect("exchange log");
    assert_eq!(
        exchanges.len(),
        1,
        "a success-required turn with one script must not retry after scheduler-owned releases"
    );
}

#[test]
fn provider_wire_script_rejects_non_monotonic_timeline_at_metadata() {
    let err = ProviderWireScript::from_json_str(
        r#"{
              "schema": "lash.provider-wire-script.v1",
              "name": "non-monotonic",
              "provider_kind": "openai-compatible",
              "endpoint": { "method": "POST", "path": "/chat/completions" },
              "request_match": { "any": true },
              "timeline": [
                { "at": 10, "event": "response_start", "status": 200 },
                { "at": 5, "event": "end" }
              ]
            }"#,
    )
    .expect_err("non-monotonic at metadata should fail validation");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("moved backward"));
}

#[test]
fn provider_wire_script_rejects_non_terminal_end() {
    let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "response_start", "status": 200 },
        { "event": "end" },
        { "event": "body", "data": "unreachable" }
    ])))
    .expect_err("end must be the final timeline event");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("end"));
    assert!(err.message.contains("final"));
}

#[test]
fn provider_wire_script_rejects_chunk_before_response_start() {
    let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "chunk", "payload": { "data": "premature" } },
        { "event": "response_start", "status": 200 },
        { "event": "end" }
    ])))
    .expect_err("chunk before response_start must fail validation");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("chunk"));
    assert!(err.message.contains("before response_start"));
}

#[test]
fn provider_wire_script_rejects_empty_chunk_payload_at_load_with_path_and_index() {
    let input = json!({
        "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
        "name": "provider-scripts/witness-empty-chunk.json",
        "provider_kind": "openai-compatible",
        "endpoint": { "method": "POST", "path": "/chat/completions" },
        "request_match": { "any": true },
        "timeline": [
            { "event": "response_start", "status": 200 },
            { "event": "chunk", "payload": { "data": "valid" } },
            { "event": "chunk" },
            { "event": "end" }
        ]
    })
    .to_string();

    let err = ProviderWireScript::from_json_str(&input)
        .expect_err("a chunk without a payload must fail while loading");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(
        err.message
            .contains("provider-scripts/witness-empty-chunk.json")
    );
    assert!(err.message.contains("chunk event at index 2"));
}

#[test]
fn provider_wire_script_rejects_both_chunk_payloads_at_load_with_path_and_index() {
    let input = json!({
        "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
        "name": "provider-scripts/witness-both-chunk-payloads.json",
        "provider_kind": "openai-compatible",
        "endpoint": { "method": "POST", "path": "/chat/completions" },
        "request_match": { "any": true },
        "timeline": [
            { "event": "response_start", "status": 200 },
            { "event": "chunk", "payload": { "data": "valid" } },
            { "event": "chunk", "data": "text", "bytes": [116, 101, 120, 116] },
            { "event": "end" }
        ]
    })
    .to_string();

    let err = ProviderWireScript::from_json_str(&input)
        .expect_err("a chunk with both payload alternatives must fail while loading");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(
        err.message
            .contains("provider-scripts/witness-both-chunk-payloads.json")
    );
    assert!(err.message.contains("chunk event at index 2"));
}

#[test]
fn provider_wire_script_rejects_empty_request_match_at_load() {
    let input = json!({
        "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
        "name": "provider-scripts/witness-empty-request-match.json",
        "provider_kind": "openai-compatible",
        "endpoint": { "method": "POST", "path": "/chat/completions" },
        "request_match": {},
        "timeline": [{ "event": "transport_error", "message": "fixture error" }]
    })
    .to_string();

    let err = ProviderWireScript::from_json_str(&input)
        .expect_err("an empty request matcher must fail while loading");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(
        err.message
            .contains("provider-scripts/witness-empty-request-match.json")
    );
    assert!(err.message.contains("request matcher"));
}

#[test]
fn provider_wire_script_rejects_sse_before_response_start() {
    let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "sse", "data": "{}" },
        { "event": "response_start", "status": 200 },
        { "event": "end" }
    ])))
    .expect_err("SSE before response_start must fail validation");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("sse"));
    assert!(err.message.contains("before response_start"));
}

#[test]
fn provider_wire_script_rejects_body_before_response_start() {
    let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "body", "data": "premature" },
        { "event": "response_start", "status": 200 },
        { "event": "end" }
    ])))
    .expect_err("body before response_start must fail validation");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("body"));
    assert!(err.message.contains("before response_start"));
}

#[test]
fn provider_wire_script_rejects_http_error_after_response_start() {
    let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "response_start", "status": 200 },
        { "event": "http_error", "status": 429, "body": "rate limited" }
    ])))
    .expect_err("http_error after response_start must fail validation");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("http_error after response_start"));
}

#[test]
fn provider_wire_script_rejects_second_response_start() {
    let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "response_start", "status": 200 },
        { "event": "response_start", "status": 201 },
        { "event": "end" }
    ])))
    .expect_err("second response_start must fail validation");

    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("second response_start"));
}

#[test]
fn canonical_provider_wire_script_plans_cover_every_timeline_event_index() {
    for canonical in CANONICAL_SCRIPTS {
        let script = ProviderWireScript::from_json_str(canonical.content)
            .unwrap_or_else(|error| panic!("{} failed to parse: {error}", canonical.path));
        assert_eq!(
            script.plan().expect("compiled plan").event_indices(),
            (0..script.timeline().len()).collect::<Vec<_>>(),
            "{} plan must cover each timeline event exactly once",
            canonical.path
        );
    }
}

#[test]
fn cloned_provider_wire_script_recompiles_a_replaced_timeline() {
    let script = ProviderWireScript::from_json_str(OPENAI_COMPAT_TOOL_CALL).expect("script");
    let mut failure = script.clone();
    *failure.timeline_mut() = vec![ProviderWireEvent::TransportError {
        at: 0,
        message: "retry me".to_string(),
        retryable: Some(true),
    }];

    assert!(matches!(
        failure.plan().expect("recompiled failure plan"),
        ScriptedResponsePlan::Failure { event_index: 0, .. }
    ));
}

#[test]
fn in_place_provider_wire_script_mutation_recompiles_before_execution() {
    let mut script = ProviderWireScript::from_json_str(OPENAI_COMPAT_TOOL_CALL).expect("script");
    script.plan().expect("initial plan");

    let timeline = script.timeline_mut();
    timeline.clear();
    timeline.push(ProviderWireEvent::TransportError {
        at: 0,
        message: "retry me".to_string(),
        retryable: Some(true),
    });

    let error = execute_script(&script).expect_err("mutated plan should execute as failure");
    assert_eq!(error.kind, ProviderFailureKind::Transport);
    assert_eq!(error.message, "retry me");
}

#[test]
fn scripted_transport_from_scripts_validates_programmatic_scripts() {
    let timeline = vec![
        ProviderWireEvent::Chunk {
            at: 0,
            payload: ProviderWireChunkPayload::Data("premature".to_string()),
        },
        ProviderWireEvent::ResponseStart {
            at: 0,
            status: 200,
            headers: Vec::new(),
        },
        ProviderWireEvent::End { at: 0 },
    ];
    let json_error = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "chunk", "payload": { "data": "premature" } },
        { "event": "response_start", "status": 200 },
        { "event": "end" }
    ])))
    .expect_err("the JSON path should reject the illegal shape");
    let script = ProviderWireScript::from_parts(
        PROVIDER_WIRE_SCRIPT_SCHEMA.to_string(),
        "invalid-shape".to_string(),
        "openai-compatible".to_string(),
        ProviderWireEndpoint {
            method: "POST".to_string(),
            path: "/chat/completions".to_string(),
        },
        ProviderWireRequestMatch::default(),
        timeline,
    );

    let scripts_error = ScriptedLlmHttpTransport::from_scripts([script])
        .expect_err("from_scripts should reject the illegal shape");
    assert_eq!(scripts_error.kind, ProviderFailureKind::Validation);
    assert_eq!(scripts_error.message, json_error.message);
}

#[tokio::test]
async fn consecutive_body_events_are_streamed_as_individual_chunks() {
    let script = ProviderWireScript::from_json_str(&wire_script_json(json!([
        { "event": "response_start", "status": 200 },
        { "event": "body", "data": "first" },
        { "event": "body", "data": "second" },
        { "event": "chunk", "payload": { "data": "tail" } },
        { "event": "end" }
    ])))
    .expect("streaming body script");
    let response = execute_script(&script).expect("streaming response");
    let LlmHttpBody::Streamed(mut stream) = response.body else {
        panic!("body events followed by a chunk must produce a stream");
    };

    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next_chunk().await.expect("stream chunk") {
        chunks.push(chunk);
    }

    assert_eq!(
        chunks,
        vec![
            Bytes::from("first"),
            Bytes::from("second"),
            Bytes::from("tail")
        ]
    );
    assert_eq!(chunks.len(), 3);
}

fn scripted_provider(script: &str) -> OpenAiCompatibleProvider {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(script).unwrap());
    OpenAiCompatibleProvider::new("test-key", "https://provider.test").with_transport(transport)
}

fn event_collector() -> (Arc<Mutex<Vec<LlmStreamEvent>>>, LlmEventSender) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let sender = LlmEventSender::new(move |event| {
        captured.lock_recover().push(event);
    });
    (events, sender)
}

fn text_deltas(events: &Arc<Mutex<Vec<LlmStreamEvent>>>) -> Vec<String> {
    events
        .lock_recover()
        .iter()
        .filter_map(|event| match event {
            LlmStreamEvent::Delta(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn request_body_json(err: &LlmTransportError) -> serde_json::Value {
    serde_json::from_str(err.request_body.as_deref().expect("request body snapshot"))
        .expect("request body JSON")
}

fn wire_script_json(timeline: Value) -> String {
    json!({
        "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
        "name": "invalid-shape",
        "provider_kind": "openai-compatible",
        "endpoint": { "method": "POST", "path": "/chat/completions" },
        "request_match": { "any": true },
        "timeline": timeline
    })
    .to_string()
}

fn request(stream_events: Option<LlmEventSender>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "openai/gpt-5.4".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "lookup x")],
        resolved_stored: Default::default(),
        tools: Arc::new(vec![LlmToolSpec {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                }
            })
            .into(),
            output_schema: json!({}).into(),
        }]),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events,
        provider_trace: None,
    }
}

fn responses_request() -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "gpt-5.4".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}
