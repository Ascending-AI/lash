use super::transport::execute_script;
use super::*;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use std::time::Duration;

use lash_core::llm::transport::ProviderFailureKind;
use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmRole, LlmStreamEvent,
    LlmTerminalReason, LlmToolChoice, LlmToolSpec,
};
use lash_core::provider::{
    DefaultProviderFailureClassifier, Provider, ProviderFailureClassifier, ProviderOptions,
    ProviderReliability,
};
use lash_llm_transport::LlmHttpBody;
use lash_provider_openai::{OpenAiCompatibleProvider, OpenAiProvider};
use serde_json::json;

use crate::canonical_scripts::{
    OPENAI_COMPAT_RATE_LIMIT, OPENAI_COMPAT_TOOL_CALL, OPENAI_COMPAT_VALIDATION,
    OPENAI_RESPONSES_TEXT,
};

const RATE_LIMIT_BODY: &str = "{\"error\":{\"message\":\"Rate limit reached for requests\",\"type\":\"rate_limit_error\",\"code\":\"rate_limit_exceeded\"}}";
const VALIDATION_BODY: &str = "{\"error\":{\"message\":\"Invalid request: tools[0].function.parameters is invalid\",\"type\":\"invalid_request_error\",\"code\":\"invalid_request_error\"}}";

#[tokio::test]
async fn provider_wire_script_openai_compatible_chat_stream_uses_real_provider_parser() {
    let (events, sender) = event_collector();
    let mut provider = scripted_provider(OPENAI_COMPAT_TOOL_CALL);

    let response = provider
        .complete(
            request(Some(sender)),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
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
        .complete(
            request(None),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("rate limit error");

    assert_eq!(err.http_status, Some(429));
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
        .complete(
            request(None),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("validation error");

    assert_eq!(err.http_status, Some(400));
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
async fn provider_wire_script_direct_openai_responses_uses_real_provider_parser() {
    let transport =
        Arc::new(ScriptedLlmHttpTransport::from_json_str(OPENAI_RESPONSES_TEXT).unwrap());
    let mut provider = OpenAiProvider::new("test-key").with_transport(transport);

    let response = provider
        .complete(
            responses_request(),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
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

    let task = tokio::spawn(async move {
        provider
            .complete(
                request(Some(sender)),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
    });
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
                .request_timeout_ms(Some(1))
                .stream_chunk_timeout_ms(Some(1)),
            ..ProviderOptions::default()
        })
        .with_transport(provider_transport);

    let err = provider
        .complete(
            request(Some(sender)),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("response start gate should time out");

    assert_eq!(err.kind, ProviderFailureKind::Timeout);
    assert_eq!(
        err.code.as_ref().map(|code| code.to_string()),
        Some("lash:timeout".to_string())
    );
    assert!(err.is_retryable());
    assert!(err.http_status.is_none());
    let evidence = err
        .stream_evidence
        .as_deref()
        .expect("attempt observations");
    assert_eq!(evidence.http_status, None);
    assert_eq!(evidence.response_headers_ms, None);
    assert_eq!(evidence.first_body_chunk_ms, None);
    assert_eq!(evidence.bytes_received, None);
    assert!(evidence.partial_output.is_empty());
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
                .request_timeout_ms(Some(1))
                .stream_chunk_timeout_ms(Some(1)),
            ..ProviderOptions::default()
        })
        .with_transport(provider_transport);

    let response = provider
        .complete(
            request(Some(sender)),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
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

fn same_name_error_script(status: u16) -> ProviderWireScript {
    ProviderWireScript::from_json_str(&format!(
        r#"{{
              "schema": "lash.provider-wire-script.v1",
              "name": "shared-label",
              "provider_kind": "openai-compatible",
              "endpoint": {{ "method": "POST", "path": "/chat/completions" }},
              "request_match": {{ "any": true }},
              "timeline": [
                {{ "at": 10, "event": "http_error", "status": {status}, "headers": [], "body": "{{}}" }}
              ]
            }}"#
    ))
    .expect("same-name script")
}

fn raw_http_request(body: &'static str) -> lash_llm_transport::LlmHttpRequest {
    lash_llm_transport::LlmHttpRequest {
        method: lash_llm_transport::LlmHttpMethod::Post,
        url: "https://provider.test/chat/completions".to_string(),
        headers: Vec::new(),
        body: bytes::Bytes::from_static(body.as_bytes()),
        body_for_error: None,
        response_start_timeout_message: None,
    }
}

fn exchange_body_keys(exchange: &ScriptedLlmHttpExchange) -> Value {
    exchange.request.body_shape["keys"].clone()
}

/// Script names are labels, not identities: each scheduled send completes the exchange row
/// it inserted, even while another send under the same script name is still pending or is
/// cancelled before its response starts.
#[tokio::test]
async fn scheduled_send_completes_its_own_exchange_when_script_names_repeat() {
    use lash_llm_transport::LlmHttpTransport as _;

    let schedule = ScriptedTransportSchedule::new();
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_scripts([
            same_name_error_script(429),
            same_name_error_script(400),
            same_name_error_script(503),
        ])
        .expect("valid scripts")
        .with_event_schedule(schedule.clone()),
    );
    let send = |body: &'static str| {
        let transport = transport.clone();
        tokio::spawn(async move { transport.send(raw_http_request(body), None).await })
    };

    let a = send(r#"{"a":1}"#);
    schedule.wait_until_blocked(0, 0).await;
    let b = send(r#"{"b":1}"#);
    schedule.wait_until_blocked(1, 0).await;
    let c = send(r#"{"c":1}"#);
    schedule.wait_until_blocked(2, 0).await;

    schedule.release(0, 0, "http_error", 10);
    let a_status = a.await.expect("send A task").expect("send A").status;
    assert_eq!(a_status, 429);
    let exchanges = transport.exchanges().expect("exchange log");
    assert_eq!(exchange_body_keys(&exchanges[0]), json!(["a"]));
    assert_eq!(
        exchanges[0].response.status,
        Some(429),
        "A must complete the row it inserted"
    );
    assert_eq!(exchange_body_keys(&exchanges[1]), json!(["b"]));
    assert_eq!(
        exchanges[1].response.status, None,
        "B's row must stay pending while B is blocked"
    );

    c.abort();
    assert!(c.await.expect_err("cancelled send C").is_cancelled());
    schedule.release(1, 0, "http_error", 10);
    let b_status = b.await.expect("send B task").expect("send B").status;
    assert_eq!(b_status, 400);

    let exchanges = transport.exchanges().expect("exchange log");
    let rows: Vec<_> = exchanges
        .iter()
        .map(|exchange| (exchange_body_keys(exchange), exchange.response.status))
        .collect();
    assert_eq!(
        rows,
        vec![
            (json!(["a"]), Some(429)),
            (json!(["b"]), Some(400)),
            (json!(["c"]), None),
        ]
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
            LlmStreamEvent::Block(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text,
                ..
            }) => Some(text.clone()),
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
        model: lash_protocol_types::llm_profile::LlmProfileConfig::new(
            lash_protocol_types::llm_profile::RecordedLlmProfile::mint(
                lash_protocol_types::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_protocol_types::llm_profile::LlmProfileMetadata::builder(
                    "openai/gpt-5.4".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(lash_core::LlmProfileCapability::default())
                .extra_body(Default::default())
                .cache_retention(lash_protocol_types::llm::capability::CacheRetention::Short)
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::text(LlmRole::User, "lookup x")],

        tools: Arc::new(vec![LlmToolSpec {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            input_schema: lash_vocabulary::schema_contract::SchemaContract::admit(json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                }
            }))
            .expect("valid declared schema"),
            output_schema: lash_vocabulary::schema_contract::SchemaContract::admit(
                lash_vocabulary::ToolDefinition::any_output(),
            )
            .expect("valid declared schema"),
        }]),
        tool_choice: LlmToolChoice::Auto,
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events,
        provider_trace: None,
        attempt_control: Default::default(),
    }
}

fn responses_request() -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_protocol_types::llm_profile::LlmProfileConfig::new(
            lash_protocol_types::llm_profile::RecordedLlmProfile::mint(
                lash_protocol_types::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_protocol_types::llm_profile::LlmProfileMetadata::builder(
                    "gpt-5.4".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(lash_core::LlmProfileCapability::default())
                .extra_body(Default::default())
                .cache_retention(lash_protocol_types::llm::capability::CacheRetention::Short)
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],

        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(|_event| {})),
        provider_trace: None,
        attempt_control: Default::default(),
    }
}

/// FIG-7553: headers and a non-delta event must survive a stalled stream,
/// through the typed failure, sealed attempt and exported model-call span.
#[tokio::test(start_paused = true)]
async fn stalled_stream_retains_transport_evidence() {
    use crate::runtime_providers::{runtime_provider_components, runtime_script_value_for_text};
    use lash_protocol_types::llm::types::{PartialOutputKind, ProtocolPosition};
    for (kind, event) in [
        ("openai-compatible", r#"{"choices":[]}"#),
        (
            "openai",
            r#"{"type":"response.created","response":{"id":"resp-1","status":"in_progress"}}"#,
        ),
        ("anthropic", r#"{"type":"ping"}"#),
        ("google_oauth", r#"{"response":{}}"#),
    ] {
        let bytes = format!("data: {event}\n\n");
        let mut script = runtime_script_value_for_text(kind, "unused").unwrap();
        script["request_match"] = json!({"any": true});
        script["timeline"] = json!([
            { "at": 5, "event": "response_start", "status": 200,
              "headers": [{"name": "content-type", "value": "text/event-stream"}] },
            { "at": 12, "event": "chunk", "payload": { "data": bytes } },
            { "at": 113, "event": "end" }
        ]);
        let schedule = ScriptedTransportSchedule::new();
        let transport = Arc::new(
            ScriptedLlmHttpTransport::from_json_str(&script.to_string())
                .unwrap()
                .with_event_schedule(schedule.clone()),
        );
        let (mut handle, metadata, _) = runtime_provider_components(kind, &transport).unwrap();
        handle = handle.with_clock(Arc::new(StreamTestClock));
        handle.set_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .request_timeout_ms(Some(1000))
                .stream_chunk_timeout_ms(Some(100)),
            ..Default::default()
        });
        let mut request = request(Some(LlmEventSender::new(|_| {})));
        request.model = lash_protocol_types::llm_profile::LlmProfileConfig::new(
            lash_protocol_types::llm_profile::RecordedLlmProfile::mint(
                lash_protocol_types::llm_profile::LlmProfileKey::new("stream-evidence"),
                metadata,
            ),
        );
        let task = tokio::spawn(async move {
            handle
                .complete(
                    request,
                    lash_core::ChargeSafetyPolicy::RequireGuarantee,
                    lash_protocol_types::ExecutionBudgets::recommended(),
                    &lash_core::provider::NoSlotDeliveries,
                )
                .await
        });
        schedule.wait_until_blocked(0, 0).await;
        tokio::time::advance(Duration::from_millis(5)).await;
        schedule.release(0, 0, "response_start", 5);
        schedule.wait_until_blocked(0, 1).await;
        tokio::time::advance(Duration::from_millis(7)).await;
        schedule.release(0, 1, "chunk", 12);
        schedule.wait_until_blocked(0, 2).await;
        tokio::time::advance(Duration::from_millis(101)).await;
        let failure = task.await.unwrap().expect_err("stalled stream");
        let error = &failure.error;
        assert_eq!(error.kind, ProviderFailureKind::Timeout, "{kind}");
        assert_eq!(error.http_status, Some(200), "{kind}");
        let evidence = error
            .stream_evidence
            .as_deref()
            .expect("typed stream evidence");
        assert_eq!(evidence.http_status, Some(200));
        assert_eq!(evidence.response_headers_ms, Some(5));
        assert_eq!(evidence.first_body_chunk_ms, Some(12));
        assert_eq!(evidence.bytes_received, Some(bytes.len() as u64));
        assert_eq!(
            evidence.partial_output,
            [PartialOutputKind::OtherEvent].into()
        );
        let attempt = &failure.call_record.attempts[0];
        assert_eq!(evidence.protocol_position, Some(attempt.protocol_position));
        assert!(matches!(
            attempt.protocol_position,
            ProtocolPosition::ResponseObserved | ProtocolPosition::OutputStarted
        ));
        assert_eq!(
            attempt.error.as_ref().unwrap().stream_evidence.as_deref(),
            Some(evidence)
        );
        assert_stream_failure_span(&failure, bytes.len() as u64);
    }
}

/// Virtual monotonic time makes both recorded timings and the stall deterministic.
#[derive(Debug)]
struct StreamTestClock;

#[async_trait::async_trait]
impl lash_core::Clock for StreamTestClock {
    fn now(&self) -> std::time::Instant {
        tokio::time::Instant::now().into_std()
    }
    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(std::time::UNIX_EPOCH)
    }
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

fn assert_stream_failure_span(failure: &lash_core::provider::ProviderCompletionError, bytes: u64) {
    use lash_trace::otel::{LashTracerProvider, OtelOptions, OtelTelemetry};
    use lash_trace::*;
    use opentelemetry_sdk::trace::{
        InMemorySpanExporter, RandomIdGenerator, Sampler, SdkTracerProvider,
    };
    let exporter = InMemorySpanExporter::default();
    let provider = LashTracerProvider::new(
        SdkTracerProvider::builder().with_simple_exporter(exporter.clone()),
        Sampler::AlwaysOn,
        RandomIdGenerator::default(),
    );
    let meter = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    let event = TraceEvent::LlmCallFailed {
        model: None,
        request: Default::default(),
        error: TraceError {
            code: None,
            retryable: false,
            failure_kind: failure.error.kind,
            terminal_reason: failure.error.terminal_reason,
        },
        stream_summary: None,
        attempts: Some(
            failure
                .call_record
                .attempts
                .iter()
                .cloned()
                .map(Into::into)
                .collect(),
        ),
    };
    let span = TraceCarrier::parse_w3c(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        None,
    )
    .unwrap();
    let record = TraceRecord::host_owned(TraceContext::default(), event)
        .unwrap()
        .at(Some(TracePosition {
            span,
            parent: None,
            links: TraceLinks::new(),
            role: SpanRole::Whole,
            kind: TraceSpanKind::Model,
            started_at: TraceTimestamp::from_unix_ms(0),
            ended_at: Some(TraceTimestamp::from_unix_ms(113)),
            scope: None,
            attempt: None,
        }))
        .governed(ContentPolicy::standard());
    adapter.append(&record).unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let span = spans
        .iter()
        .find(|span| span.name.starts_with("chat"))
        .expect("model span");
    for (key, expected) in [
        ("http.response.status_code", "200".to_string()),
        ("lash.model.response_headers_ms", "5".to_string()),
        ("lash.model.first_body_chunk_ms", "12".to_string()),
        ("lash.model.bytes_received", bytes.to_string()),
        ("lash.model.partial_output", "[\"other_event\"]".to_string()),
    ] {
        assert_eq!(
            span.attributes
                .iter()
                .find(|a| a.key.as_str() == key)
                .map(|a| a.value.to_string()),
            Some(expected),
            "{key}"
        );
    }
    assert!(
        span.attributes
            .iter()
            .any(|a| a.key.as_str() == "lash.model.protocol_position")
    );
}
