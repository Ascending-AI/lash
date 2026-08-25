use super::session::{
    CodexWebsocketLease, CodexWebsocketSessionEntry, CodexWebsocketSessions,
    MAX_SESSION_WEBSOCKET_CACHE_ENTRIES, SESSION_WEBSOCKET_CACHE_TTL,
};
use super::*;
use lash_core::llm::transport::ProviderFailureKind;
use lash_core::llm::types::{
    LlmJsonSchema, LlmMessage, LlmOutputPart, LlmProviderTraceSender, LlmRequestScope, LlmResponse,
    LlmRole, LlmTerminalReason, LlmToolChoice, LlmToolSpec, ResponseTextMeta,
};
use lash_core::provider::{
    ModelCapability, Provider, ProviderHandle, ReasoningCapability, RequestTimeout,
    StreamTermination,
};
use lash_llm_transport::openai_terminal_reason_from_response_value;
use lash_sansio::sync::MutexExt;
use shared::ResponsesStreamState as CodexStreamState;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use ws_testing::{
    InjectedAcceptFault, ScriptedWsAction, assistant_item, spawn_scripted_websocket,
    spawn_scripted_websocket_with_injected_accept_faults,
};

#[path = "idle_timeout_tests.rs"]
mod idle_timeout_tests;

fn process_event(state: &mut CodexStreamState, event: Value) {
    CodexProvider::process_sse_event(&event.to_string(), state, None).unwrap();
}

fn process_event_with_parts(
    state: &mut CodexStreamState,
    event: Value,
    emitted_parts: &mut Vec<LlmOutputPart>,
) {
    CodexProvider::process_sse_event(&event.to_string(), state, Some(emitted_parts)).unwrap();
}

fn response_from_state(state: CodexStreamState) -> LlmResponse {
    shared::response_from_stream_state(state, None, "test".to_string())
}

fn reasoning_capability() -> ModelCapability {
    ModelCapability {
        reasoning: Some(ReasoningCapability {
            efforts: vec!["medium".to_string(), "high".to_string()],
            default_effort: Some("medium".to_string()),
            disable: Some(lash_core::provider::ReasoningDisableEncoding::Effort(
                "none".to_string(),
            )),
            ..ReasoningCapability::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
    }
}

fn request(messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        model: "gpt-5.4".to_string(),
        messages,
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: ModelCapability::default(),
        scope: LlmRequestScope::new(
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

fn traced_request(messages: Vec<LlmMessage>, trace: Arc<Mutex<Vec<Value>>>) -> LlmRequest {
    let mut req = request(messages);
    req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
        if let Ok(value) = serde_json::from_str::<Value>(&event.raw) {
            trace.lock_recover().push(value);
        }
    }));
    req
}

fn websocket_diagnostics(trace: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    trace
        .lock_recover()
        .iter()
        .filter(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "lash.codex.websocket_request")
        })
        .cloned()
        .collect()
}

fn websocket_test_provider(
    transport: CodexTransport,
    responses_url: String,
    websocket_url: String,
) -> CodexProvider {
    websocket_test_provider_with_chunk_timeout(transport, responses_url, websocket_url, None)
}

fn websocket_test_provider_with_chunk_timeout(
    transport: CodexTransport,
    responses_url: String,
    websocket_url: String,
    chunk_timeout_ms: Option<u64>,
) -> CodexProvider {
    CodexProvider::new("access", "refresh", 0)
        .with_transport(transport)
        .with_options(ProviderOptions {
            reliability: ProviderReliability::codex()
                .request_timeout(Some(RequestTimeout::Millis(5_000)))
                .stream_chunk_timeout_ms(chunk_timeout_ms),
            ..ProviderOptions::default()
        })
        .with_endpoint_urls(responses_url, websocket_url)
}

const SCRIPTED_WEBSOCKET_IDLE_TIMEOUT_MS: u64 = 50;

async fn advance_scripted_websocket_idle_timeout(ready: &Notify) {
    let reached_idle = tokio::time::timeout(Duration::from_secs(5), ready.notified()).await;
    reached_idle.expect("scripted WebSocket must reach its idle state");
    tokio::time::advance(Duration::from_millis(50)).await;
    tokio::time::resume();
}

fn assistant_message_with_meta(
    route: &lash_core::ProviderRouteIdentity,
    message_id: &str,
    text: &str,
) -> LlmMessage {
    LlmMessage::new(
        LlmRole::Assistant,
        vec![lash_core::llm::types::LlmContentBlock::Text {
            text: text.into(),
            response_meta: Some(ResponseTextMeta {
                id: Some(message_id.to_string()),
                status: Some("completed".to_string()),
                phase: Some("final_answer".to_string()),
                origin: Some(route.clone()),
                ..ResponseTextMeta::default()
            }),
            cache_breakpoint: false,
        }],
    )
}

struct HttpSseServer {
    url: String,
    captured: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl HttpSseServer {
    fn captured(&self) -> Vec<String> {
        self.captured.lock_recover().clone()
    }

    fn captured_len(&self) -> usize {
        self.captured.lock_recover().len()
    }
}

impl Drop for HttpSseServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_http_sse(
    response_id: &'static str,
    message_id: &'static str,
    text: &'static str,
) -> HttpSseServer {
    spawn_http_sse_sequence(vec![(response_id, message_id, text)]).await
}

async fn spawn_http_sse_with_headers(
    response_id: &'static str,
    message_id: &'static str,
    text: &'static str,
    headers: Vec<(String, String)>,
) -> HttpSseServer {
    spawn_http_sse_sequence_with_headers(vec![(response_id, message_id, text)], headers).await
}

async fn spawn_http_sse_sequence(
    responses: Vec<(&'static str, &'static str, &'static str)>,
) -> HttpSseServer {
    spawn_http_sse_sequence_with_headers(responses, Vec::new()).await
}

async fn spawn_http_sse_sequence_with_headers(
    responses: Vec<(&'static str, &'static str, &'static str)>,
    headers: Vec<(String, String)>,
) -> HttpSseServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind http");
    let addr = listener.local_addr().expect("http addr");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let task_captured = Arc::clone(&captured);
    let extra_headers = headers
        .into_iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let task = tokio::spawn(async move {
        for (response_id, message_id, text) in responses {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let Ok(n) = stream.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            task_captured
                .lock_recover()
                .push(String::from_utf8_lossy(&request).into_owned());
            let item = assistant_item(message_id, text);
            let body = format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.completed","response":{"id":response_id,"status":"completed","output":[assistant_item(message_id, text)],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    HttpSseServer {
        url: format!("http://{addr}/codex/responses"),
        captured,
        task,
    }
}

#[test]
fn codex_null_incomplete_details_does_not_map_to_output_limit() {
    let terminal_reason = openai_terminal_reason_from_response_value(
        &json!({"status":"completed","incomplete_details":null}),
        &[LlmOutputPart::Text {
            text: "Hi".to_string(),
            response_meta: None,
        }],
    );
    assert_eq!(terminal_reason, LlmTerminalReason::Stop);
}

#[test]
fn codex_content_filter_incomplete_maps_to_content_filter() {
    let terminal_reason = openai_terminal_reason_from_response_value(
        &json!({"status":"incomplete","incomplete_details":{"reason":"content_filter"}}),
        &[],
    );
    assert_eq!(terminal_reason, LlmTerminalReason::ContentFilter);
}

#[test]
fn codex_request_body_emits_reasoning_from_capability_variant() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "custom-codex-model".to_string();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = reasoning_capability();
    let body = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, true)
        .unwrap();
    assert_eq!(body["reasoning"], json!({ "effort": "high" }));
}

#[test]
fn codex_request_body_emits_none_effort_for_disabled_selection() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
    req.model_capability = reasoning_capability();
    let body = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, true)
        .unwrap();
    assert_eq!(body["reasoning"], json!({ "effort": "none" }));
}

#[test]
fn codex_request_body_omits_reasoning_without_capability() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "custom-codex-model".to_string();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    let body = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, true)
        .unwrap();
    assert!(body.get("reasoning").is_none());
}

#[test]
fn raw_codex_builder_strips_unstamped_and_foreign_replay_fields() {
    let foreign_route = lash_core::ProviderRouteIdentity::for_endpoint(
        "openai_compatible",
        "https://foreign.example/v1",
        "gpt-5.4",
    );
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![
            lash_core::llm::types::LlmContentBlock::Text {
                text: "portable answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("unstamped-response-id".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            },
            lash_core::llm::types::LlmContentBlock::Reasoning {
                text: "portable summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    encrypted_content: Some("foreign-encrypted-content".to_string()),
                    origin: Some(foreign_route.clone()),
                    ..Default::default()
                }),
            },
            lash_core::llm::types::LlmContentBlock::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "lookup".to_string(),
                input_json: "{}".to_string(),
                replay: Some(lash_core::llm::types::ProviderReplayMeta {
                    item_id: Some("foreign-tool-item".to_string()),
                    opaque: Some("foreign-tool-opaque".to_string()),
                    origin: Some(foreign_route),
                }),
            },
        ],
    )]);
    let body = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, true)
        .expect("Codex request serializes its neutral fallback");
    let wire = body.to_string();
    assert!(wire.contains("portable answer"));
    assert!(!wire.contains("unstamped-response-id"));
    assert!(!wire.contains("foreign-encrypted-content"));
    assert!(!wire.contains("foreign-tool-item"));
    assert!(!wire.contains("foreign-tool-opaque"));
}

fn adversarial_codex_raw_request() -> LlmRequest {
    let foreign_route = lash_core::ProviderRouteIdentity::for_endpoint(
        "openai-compatible",
        "https://foreign.example/v1",
        "gpt-5.4",
    );
    request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![
            lash_core::llm::types::LlmContentBlock::Text {
                text: "portable answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("unstamped-codex-wire-id".to_string()),
                    provider_payload: Some("unstamped-codex-wire-payload".to_string()),
                    ..Default::default()
                }),
                cache_breakpoint: false,
            },
            lash_core::llm::types::LlmContentBlock::Reasoning {
                text: "portable summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    encrypted_content: Some("foreign-codex-wire-reasoning".to_string()),
                    origin: Some(foreign_route.clone()),
                    ..Default::default()
                }),
            },
            lash_core::llm::types::LlmContentBlock::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "lookup".to_string(),
                input_json: "{}".to_string(),
                replay: Some(lash_core::llm::types::ProviderReplayMeta {
                    item_id: Some("foreign-codex-wire-tool-id".to_string()),
                    opaque: Some("foreign-codex-wire-tool-opaque".to_string()),
                    origin: Some(foreign_route),
                }),
            },
        ],
    )])
}

fn assert_codex_adversarial_replay_absent(wire: &str) {
    assert!(wire.contains("portable answer"));
    assert!(!wire.contains("unstamped-codex-wire-id"));
    assert!(!wire.contains("unstamped-codex-wire-payload"));
    assert!(!wire.contains("foreign-codex-wire-reasoning"));
    assert!(!wire.contains("foreign-codex-wire-tool-id"));
    assert!(!wire.contains("foreign-codex-wire-tool-opaque"));
}

#[tokio::test]
async fn raw_provider_complete_filters_codex_sse_and_websocket_wire_captures() {
    let http = spawn_http_sse("resp-http", "msg-http", "done").await;
    let mut sse_provider = websocket_test_provider(
        CodexTransport::Sse,
        http.url.clone(),
        "ws://127.0.0.1:9/unused".to_string(),
    );
    Provider::complete(&mut sse_provider, adversarial_codex_raw_request())
        .await
        .expect("raw Codex SSE completion");
    let sse_wire = http.captured().join("\n");
    assert_codex_adversarial_replay_absent(&sse_wire);
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp-ws",
        message_id: "msg-ws",
        text: "done",
    }])
    .await;
    let mut websocket_provider = websocket_test_provider(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    Provider::complete(&mut websocket_provider, adversarial_codex_raw_request())
        .await
        .expect("raw Codex WebSocket completion");
    let websocket_wire = serde_json::to_string(&ws.captured()).expect("captured websocket JSON");
    assert_codex_adversarial_replay_absent(&websocket_wire);
}

#[test]
fn codex_request_body_exposes_reasoning_summary_only_when_configured() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
    req.model_capability = reasoning_capability();
    let hidden = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, true)
        .unwrap();
    assert_eq!(hidden["reasoning"], json!({ "effort": "medium" }));
    let exposed = CodexProvider::new("access", "refresh", 0)
        .with_options(ProviderOptions {
            expose_thinking: true,
            ..ProviderOptions::default()
        })
        .build_request_body(&req, true)
        .unwrap();
    assert_eq!(exposed["reasoning"]["summary"], "auto");
}

#[test]
fn codex_request_omits_output_token_cap() {
    let provider = CodexProvider::new("access", "refresh", 0).with_options(ProviderOptions {
        max_output_tokens: Some(9_999),
        ..ProviderOptions::default()
    });
    let provider_limited = provider
        .build_request_body(
            &request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            false,
        )
        .unwrap();
    assert!(provider_limited.get("max_output_tokens").is_none());
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.generation.output_token_cap = NonZeroUsize::new(2_048);
    let request_limited = provider.build_request_body(&req, false).unwrap();
    assert!(request_limited.get("max_output_tokens").is_none());
}

#[test]
fn codex_error_summary_uses_top_level_detail() {
    let summary =
        CodexProvider::codex_error_summary(400, r#"{"detail":"Unsupported parameter: foo"}"#);
    assert_eq!(
        summary.as_deref(),
        Some("Codex request failed with 400: Unsupported parameter: foo")
    );
}

#[test]
fn response_failed_server_error_is_retryable() {
    let mut state = CodexStreamState::default();
    let err = CodexProvider::process_sse_event(
            r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"internal stream ended unexpectedly"}}}"#,
            &mut state,
            None,
        )
        .unwrap_err();
    assert!(err.is_retryable());
    assert_eq!(err.message, "internal stream ended unexpectedly");
}

#[test]
fn codex_request_uses_openai_schema_projection() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "empty".to_string(),
        description: "Empty".to_string(),
        input_schema: json!({"type": "object"}).into(),
        output_schema: json!({}).into(),
    }]);
    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "result".to_string(),
        schema: json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } }
        })
        .into(),
        strict: true,
    }));
    let body = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, false)
        .unwrap();
    assert_eq!(body["tools"][0]["parameters"]["properties"], json!({}));
    assert_eq!(
        body["text"]["format"]["schema"]["required"],
        json!(["summary"])
    );
    assert_eq!(
        body["text"]["format"]["schema"]["additionalProperties"],
        false
    );
}

#[test]
fn codex_request_history_preserves_assistant_message_metadata() {
    let provider = CodexProvider::new("access", "refresh", 0);
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![lash_core::llm::types::LlmContentBlock::Text {
            text: "final".into(),
            response_meta: Some(ResponseTextMeta {
                id: Some("msg_1".to_string()),
                status: Some("completed".to_string()),
                phase: Some("final_answer".to_string()),
                origin: Some(provider.route_identity("gpt-5.4")),
                ..ResponseTextMeta::default()
            }),
            cache_breakpoint: false,
        }],
    )]);
    let body = provider.build_request_body(&req, false).unwrap();
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["id"], "msg_1");
    assert_eq!(body["input"][0]["status"], "completed");
    assert_eq!(body["input"][0]["phase"], "final_answer");
    assert_eq!(body["input"][0]["content"][0]["type"], "output_text");
    assert!(body["input"][0]["content"][0]["annotations"].is_array());
}

#[test]
fn codex_cached_continuation_sends_delta_after_prior_request_and_response_items() {
    let provider =
        CodexProvider::new("access", "refresh", 0).with_transport(CodexTransport::WebsocketCached);
    let first = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let first_body = provider.build_request_body(&first, true).unwrap();
    let assistant_item = json!({
        "type": "message",
        "id": "msg_1",
        "role": "assistant",
        "status": "completed",
        "phase": "final_answer",
        "content": [{"type": "output_text", "text": "answer", "annotations": []}]
    });
    let continuation = CodexProvider::continuation_from_response(
        &first_body,
        &json!({
            "id": "resp_1",
            "status": "completed",
            "output": [assistant_item]
        }),
    )
    .expect("completed continuation");
    let second = request(vec![
        LlmMessage::text(LlmRole::User, "hello"),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: "answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    origin: Some(provider.route_identity("gpt-5.4")),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        ),
        LlmMessage::text(LlmRole::User, "next"),
    ]);
    let second_body = provider.build_request_body(&second, true).unwrap();
    let cached_body =
        CodexProvider::cached_websocket_body(&continuation, &second_body).expect("cached body");
    assert_eq!(cached_body["previous_response_id"], "resp_1");
    assert_eq!(
        cached_body["input"].as_array().expect("delta input").len(),
        1
    );
    assert_eq!(cached_body["input"][0]["role"], "user");
    assert_eq!(cached_body["input"][0]["content"][0]["text"], "next");
}

#[test]
fn codex_websocket_request_uses_response_create_event_shape() {
    let provider = CodexProvider::new("access", "refresh", 0);
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let body = provider.build_request_body(&req, true).unwrap();
    let websocket_body = CodexProvider::websocket_create_request(&body);
    assert_eq!(websocket_body["type"], "response.create");
    assert_eq!(websocket_body["model"], body["model"]);
    assert_eq!(websocket_body["input"], body["input"]);
    assert_eq!(websocket_body["stream"], true);
    assert!(websocket_body.get("response").is_none());
}

#[test]
fn codex_websocket_request_keeps_cached_previous_response_id() {
    let provider = CodexProvider::new("access", "refresh", 0);
    let req = request(vec![LlmMessage::text(LlmRole::User, "next")]);
    let mut body = provider.build_request_body(&req, true).unwrap();
    body["previous_response_id"] = json!("resp_1");
    body["input"] = json!([]);

    let websocket_body = CodexProvider::websocket_create_request(&body);

    assert_eq!(websocket_body["type"], "response.create");
    assert_eq!(websocket_body["previous_response_id"], "resp_1");
    assert_eq!(websocket_body["input"], json!([]));
}

#[tokio::test]
async fn codex_websocket_scope_cache_prunes_idle_entries_and_caps_oldest() {
    let ws = spawn_scripted_websocket(Vec::new()).await;
    let now = Instant::now();
    let mut sessions = CodexWebsocketSessions::default();
    let (idle_connection, _) = tokio_tungstenite::connect_async(&ws.url)
        .await
        .expect("connect idle websocket");
    sessions.by_scope.insert(
        "idle".to_string(),
        CodexWebsocketSessionEntry::Idle {
            connection: Box::new(idle_connection),
            continuation: None,
            last_used: now - SESSION_WEBSOCKET_CACHE_TTL - Duration::from_secs(1),
            credential_generation: 0,
        },
    );
    sessions.by_scope.insert(
        "busy".to_string(),
        CodexWebsocketSessionEntry::Reserved {
            credential_generation: 0,
        },
    );

    CodexProvider::prune_idle_websocket_sessions(&mut sessions);

    assert!(!sessions.by_scope.contains_key("idle"));
    assert!(sessions.by_scope.contains_key("busy"));

    sessions.by_scope.clear();
    for index in 0..(MAX_SESSION_WEBSOCKET_CACHE_ENTRIES + 3) {
        let (connection, _) = tokio_tungstenite::connect_async(&ws.url)
            .await
            .expect("connect capacity-test websocket");
        sessions.by_scope.insert(
            format!("scope-{index}"),
            CodexWebsocketSessionEntry::Idle {
                connection: Box::new(connection),
                continuation: None,
                last_used: now - Duration::from_secs((100 - index) as u64),
                credential_generation: 0,
            },
        );
    }

    CodexProvider::enforce_websocket_session_cache_cap(&mut sessions);

    assert_eq!(sessions.by_scope.len(), MAX_SESSION_WEBSOCKET_CACHE_ENTRIES);
    assert!(!sessions.by_scope.contains_key("scope-0"));
    assert!(sessions.by_scope.contains_key(&format!(
        "scope-{}",
        MAX_SESSION_WEBSOCKET_CACHE_ENTRIES + 2
    )));
}

#[tokio::test]
async fn codex_websocket_scope_cache_evicts_rotated_credentials() {
    let ws = spawn_scripted_websocket(Vec::new()).await;
    let mut sessions = CodexWebsocketSessions::default();
    let (old_connection, _) = tokio_tungstenite::connect_async(&ws.url)
        .await
        .expect("connect old-generation websocket");
    sessions.by_scope.insert(
        "old".to_string(),
        CodexWebsocketSessionEntry::Idle {
            connection: Box::new(old_connection),
            continuation: None,
            last_used: Instant::now(),
            credential_generation: 4,
        },
    );
    let (current_connection, _) = tokio_tungstenite::connect_async(&ws.url)
        .await
        .expect("connect current-generation websocket");
    sessions.by_scope.insert(
        "current".to_string(),
        CodexWebsocketSessionEntry::Idle {
            connection: Box::new(current_connection),
            continuation: None,
            last_used: Instant::now(),
            credential_generation: 5,
        },
    );

    CodexProvider::evict_websocket_sessions_for_generation(&mut sessions, 5);

    assert!(!sessions.by_scope.contains_key("old"));
    assert!(sessions.by_scope.contains_key("current"));
}

#[test]
fn codex_websocket_scope_cache_generation_eviction_preserves_leased_slot() {
    let mut sessions = CodexWebsocketSessions::default();
    sessions.by_scope.insert(
        "leased".to_string(),
        CodexWebsocketSessionEntry::reserved(4),
    );
    CodexProvider::evict_websocket_sessions_for_generation(&mut sessions, 5);
    assert!(sessions.by_scope.contains_key("leased"));
}

fn assert_cleanup_preserves_newer_generation(cleanup: impl FnOnce(&CodexProvider, String)) {
    let provider = CodexProvider::new("access", "refresh", 0);
    let scope_key = "rotated".to_string();
    let mut sessions = provider.websocket_sessions.inner.lock_recover();
    sessions.by_scope.insert(
        scope_key.clone(),
        CodexWebsocketSessionEntry::Reserved {
            credential_generation: 5,
        },
    );
    drop(sessions);
    cleanup(&provider, scope_key.clone());
    let sessions = provider.websocket_sessions.inner.lock_recover();
    let generation = sessions
        .by_scope
        .get(&scope_key)
        .map(CodexWebsocketSessionEntry::credential_generation);
    assert_eq!(generation, Some(5));
}

#[tokio::test]
async fn codex_websocket_release_preserves_newer_generation_reservation() {
    let ws = spawn_scripted_websocket(Vec::new()).await;
    let (websocket, _) = tokio_tungstenite::connect_async(&ws.url)
        .await
        .expect("connect old-generation websocket");
    assert_cleanup_preserves_newer_generation(move |provider, scope_key| {
        provider.release_websocket_lease(
            CodexWebsocketLease {
                websocket,
                scope_key: Some(scope_key),
                reusable: true,
                reused: false,
                continuation: None,
                credential_generation: 4,
            },
            true,
            None,
        );
    });
}

#[test]
fn codex_websocket_connect_failure_preserves_newer_generation_reservation() {
    assert_cleanup_preserves_newer_generation(|provider, scope_key| {
        provider.remove_websocket_scope(&scope_key, 4);
    });
}

async fn assert_trace_cached_delta_for_transport(transport: CodexTransport) {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "done",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        transport,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    let trace = Arc::new(Mutex::new(Vec::new()));

    provider
        .complete(traced_request(
            vec![LlmMessage::text(LlmRole::User, "hello")],
            Arc::clone(&trace),
        ))
        .await
        .expect("first response");
    let response = provider
        .complete(traced_request(
            vec![
                LlmMessage::text(LlmRole::User, "hello"),
                assistant_message_with_meta(&provider.route_identity("gpt-5.4"), "msg_1", "answer"),
                LlmMessage::text(LlmRole::User, "next"),
            ],
            Arc::clone(&trace),
        ))
        .await
        .expect("cached follow-up response");

    assert_eq!(response.full_text, "done");
    let diagnostics = websocket_diagnostics(&trace);
    assert_eq!(diagnostics.len(), 2, "{transport:?}");
    assert_eq!(diagnostics[0]["transport"], format!("{transport:?}"));
    assert_eq!(diagnostics[0]["cached_request"], false);
    assert_eq!(diagnostics[0]["cache_miss_reason"], "missing_continuation");
    assert_eq!(diagnostics[1]["transport"], format!("{transport:?}"));
    assert_eq!(diagnostics[1]["reused_connection"], true);
    assert_eq!(diagnostics[1]["cached_request"], true);
    assert_eq!(diagnostics[1]["previous_response_id"], "resp_1");
    assert_eq!(diagnostics[1]["sent_input_items"], 1);
    assert_eq!(diagnostics[1]["retry_after_stale_previous_response"], false);
}

async fn assert_trace_stale_retry_for_transport(transport: CodexTransport) {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::Error {
            message: "Previous response with id 'resp_1' not found",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "recovered",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        transport,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    let trace = Arc::new(Mutex::new(Vec::new()));

    provider
        .complete(traced_request(
            vec![LlmMessage::text(LlmRole::User, "hello")],
            Arc::clone(&trace),
        ))
        .await
        .expect("first response");
    let response = provider
        .complete(traced_request(
            vec![
                LlmMessage::text(LlmRole::User, "hello"),
                assistant_message_with_meta(&provider.route_identity("gpt-5.4"), "msg_1", "answer"),
                LlmMessage::text(LlmRole::User, "next"),
            ],
            Arc::clone(&trace),
        ))
        .await
        .expect("stale retry response");

    assert_eq!(response.full_text, "recovered");
    let diagnostics = websocket_diagnostics(&trace);
    assert_eq!(diagnostics.len(), 3, "{transport:?}");
    assert_eq!(diagnostics[1]["reused_connection"], true);
    assert_eq!(diagnostics[1]["cached_request"], true);
    assert_eq!(diagnostics[1]["previous_response_id"], "resp_1");
    assert_eq!(diagnostics[2]["cached_request"], false);
    assert_eq!(diagnostics[2]["cache_miss_reason"], "disabled");
    assert_eq!(diagnostics[2]["retry_after_stale_previous_response"], true);
    assert!(
        diagnostics[2]
            .get("previous_response_id")
            .is_none_or(Value::is_null)
    );
}

#[tokio::test]
async fn codex_scripted_websocket_trace_diagnostics_cover_cached_delta_and_stale_retry() {
    for transport in [CodexTransport::WebsocketCached, CodexTransport::Auto] {
        assert_trace_cached_delta_for_transport(transport).await;
        assert_trace_stale_retry_for_transport(transport).await;
    }
}

#[tokio::test]
async fn codex_scripted_websocket_default_timeout_survives_a_scheduler_stall() {
    let idle_ready = Arc::new(Notify::new());
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::IdleBeforeStart {
        ready: idle_ready.clone(),
    }])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    let completion = tokio::spawn(async move {
        provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
    });

    tokio::time::timeout(Duration::from_secs(5), idle_ready.notified())
        .await
        .expect("scripted WebSocket must capture the request before the stall");
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(
        SCRIPTED_WEBSOCKET_IDLE_TIMEOUT_MS + 1,
    ))
    .await;
    tokio::task::yield_now().await;

    assert!(
        !completion.is_finished(),
        "ordinary scripted WebSocket traffic must not use the intentional-idle deadline"
    );
    completion.abort();
    let _ = completion.await;
    tokio::time::resume();
}

#[tokio::test]
async fn codex_scripted_websocket_full_turn_sends_response_create() {
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp_1",
        message_id: "msg_1",
        text: "ok",
    }])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("websocket response");

    assert_eq!(response.full_text, "ok");
    let captured = ws.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["type"], "response.create");
    assert!(captured[0].get("previous_response_id").is_none());
    let headers = ws.handshakes();
    assert_eq!(headers.len(), 1);
    let header = |name: &str| {
        headers[0]
            .iter()
            .find_map(|(header_name, value)| (header_name == name).then_some(value.as_str()))
    };
    assert_eq!(header("session-id"), Some("session-1"));
    assert_eq!(
        header("x-client-request-id"),
        Some("session-1:request:test")
    );
    assert_eq!(header("session_id"), None);
}

#[tokio::test]
async fn codex_scripted_websocket_cached_follow_up_omits_previous_assistant_output() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "done",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first response");
    let second = request(vec![
        LlmMessage::text(LlmRole::User, "hello"),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: "answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    origin: Some(provider.route_identity("gpt-5.4")),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        ),
        LlmMessage::text(LlmRole::User, "next"),
    ]);
    let response = provider.complete(second).await.expect("second response");

    assert_eq!(response.full_text, "done");
    assert!(
        response
            .http_summary
            .as_deref()
            .unwrap_or_default()
            .contains("cached=true")
    );
    let captured = ws.captured();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[1]["previous_response_id"], "resp_1");
    assert_eq!(captured[1]["input"].as_array().unwrap().len(), 1);
    assert_eq!(captured[1]["input"][0]["content"][0]["text"], "next");
}

#[tokio::test]
async fn codex_provider_close_sends_websocket_close_frame_for_cached_session() {
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp_1",
        message_id: "msg_1",
        text: "answer",
    }])
    .await;
    let provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    // A completed turn leaves a reusable WebSocket session cached.
    let mut running = provider.clone();
    running
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first response");
    assert_eq!(ws.close_frame_count(), 0, "no close before shutdown");

    // The host-callable close drains the cache with a proper Close frame,
    // not a bare TCP drop. The cache is shared across clones, so closing the
    // retained clone releases the socket the running handle cached.
    provider.close().await.expect("provider close");

    let deadline = Instant::now() + Duration::from_secs(5);
    while ws.close_frame_count() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ws.close_frame_count(),
        1,
        "close() must send a WebSocket Close frame to the cached session"
    );

    // The cache is empty after close.
    assert!(
        provider
            .websocket_sessions
            .inner
            .lock_recover()
            .by_scope
            .is_empty(),
        "close() drains the session cache"
    );
}

#[tokio::test]
async fn codex_provider_close_drains_a_dead_cached_socket_within_bound() {
    // A peer that closed its side leaves a dead socket in the cache. The
    // bounded, best-effort per-socket close must tolerate it: close() returns
    // promptly and still empties the cache, so a wedged socket can never fail
    // the drain or stall the sockets queued behind it.
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::CompleteAndClose {
        response_id: "resp_1",
        message_id: "msg_1",
        text: "answer",
    }])
    .await;
    let provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    let mut running = provider.clone();
    running
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first response");
    // Let the peer's Close frame land so the cached socket is genuinely dead,
    // without a reuse ever polling (and evicting) it first.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        provider
            .websocket_sessions
            .inner
            .lock_recover()
            .by_scope
            .len(),
        1,
        "the completed turn leaves a (now dead) socket cached for the drain"
    );

    // An unbounded close on a wedged socket could hang here; the per-socket
    // timeout keeps the drain moving. The outer guard turns a regression into
    // a failure instead of hanging the whole suite.
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(20), provider.close())
        .await
        .expect("close() must not hang draining a dead cached socket")
        .expect("provider close");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "each socket close is bounded, drain took {:?}",
        started.elapsed()
    );
    assert!(
        provider
            .websocket_sessions
            .inner
            .lock_recover()
            .by_scope
            .is_empty(),
        "close() drains the cache even when a cached socket is dead"
    );
}

#[tokio::test]
async fn codex_scripted_websocket_same_session_different_frame_does_not_reuse_continuation() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "done",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first response");
    let mut second = request(vec![
        LlmMessage::text(LlmRole::User, "hello"),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: "answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    origin: Some(provider.route_identity("gpt-5.4")),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        ),
        LlmMessage::text(LlmRole::User, "next"),
    ]);
    second.scope = LlmRequestScope::new(
        "session-1",
        "session-1:frame:other",
        "session-1:request:other",
    );
    let response = provider.complete(second).await.expect("second response");

    assert_eq!(response.full_text, "done");
    assert!(
        response
            .http_summary
            .as_deref()
            .unwrap_or_default()
            .contains("cache_miss=missing_continuation")
    );
    let captured = ws.captured();
    assert_eq!(captured.len(), 2);
    assert!(captured[1].get("previous_response_id").is_none());
    assert_eq!(captured[1]["input"].as_array().unwrap().len(), 3);
    let handshakes = ws.handshakes();
    assert_eq!(
        handshakes[1]
            .iter()
            .find_map(|(name, value)| (name == "session-id").then_some(value.as_str())),
        Some("session-1")
    );
    assert_eq!(
        handshakes[1].iter().find_map(|(name, value)| {
            (name == "x-client-request-id").then_some(value.as_str())
        }),
        Some("session-1:request:other")
    );
}

#[tokio::test]
async fn codex_scripted_websocket_stale_previous_response_retries_full_context_once() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::Error {
            message: "Previous response with id 'resp_1' not found",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "recovered",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first response");
    let second = request(vec![
        LlmMessage::text(LlmRole::User, "hello"),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: "answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    origin: Some(provider.route_identity("gpt-5.4")),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        ),
        LlmMessage::text(LlmRole::User, "next"),
    ]);
    let full_body = provider.build_request_body(&second, true).unwrap();
    let response = provider
        .complete(second)
        .await
        .expect("stale retry response");

    assert_eq!(response.full_text, "recovered");
    assert!(
        response
            .http_summary
            .as_deref()
            .unwrap_or_default()
            .contains("retry_after_stale=true")
    );
    let captured = ws.captured();
    assert_eq!(captured.len(), 3);
    assert_eq!(captured[1]["previous_response_id"], "resp_1");
    assert!(captured[2].get("previous_response_id").is_none());
    assert_eq!(captured[2]["input"], full_body["input"]);
}

#[tokio::test]
async fn codex_stale_continuation_after_allocation_only_event_still_recovers() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::AllocationThenError {
            message_id: "msg_empty",
            message: "Previous response with id 'resp_1' not found",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "recovered",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first response");
    let second = request(vec![
        LlmMessage::text(LlmRole::User, "hello"),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: "answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    origin: Some(provider.route_identity("gpt-5.4")),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        ),
        LlmMessage::text(LlmRole::User, "next"),
    ]);
    let full_body = provider.build_request_body(&second, true).unwrap();

    let result = provider.complete(second).await;
    let captured = ws.captured();
    assert_eq!(captured.len(), 3);
    let response = result.expect("allocation-only stale response retries with full context");
    assert_eq!(response.full_text, "recovered");
    assert_eq!(captured[1]["previous_response_id"], "resp_1");
    assert!(captured[2].get("previous_response_id").is_none());
    assert_eq!(captured[2]["input"], full_body["input"]);
}

#[tokio::test]
async fn codex_scripted_websocket_dead_reused_socket_reconnects_full_context() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::CompleteAndClose {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "reconnected",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first response");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let second = request(vec![
        LlmMessage::text(LlmRole::User, "hello"),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: "answer".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    origin: Some(provider.route_identity("gpt-5.4")),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        ),
        LlmMessage::text(LlmRole::User, "next"),
    ]);
    let full_body = provider.build_request_body(&second, true).unwrap();
    let response = provider
        .complete(second)
        .await
        .expect("dead reused socket reconnect response");

    assert_eq!(response.full_text, "reconnected");
    assert!(
        response
            .http_summary
            .as_deref()
            .unwrap_or_default()
            .contains("retry_after_dead_reused=true")
    );
    let captured = ws.captured();
    assert_eq!(captured.len(), 2);
    assert!(captured[1].get("previous_response_id").is_none());
    assert_eq!(captured[1]["input"], full_body["input"]);
    assert_eq!(ws.handshakes().len(), 2);
}

#[tokio::test]
async fn codex_scripted_websocket_incomplete_terminal_response_is_not_cached() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Incomplete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "partial",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "fresh",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );

    provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("incomplete terminal response");
    let second = request(vec![
        LlmMessage::text(LlmRole::User, "hello"),
        assistant_message_with_meta(&provider.route_identity("gpt-5.4"), "msg_1", "partial"),
        LlmMessage::text(LlmRole::User, "next"),
    ]);
    let full_body = provider.build_request_body(&second, true).unwrap();
    let response = provider
        .complete(second)
        .await
        .expect("fresh response after incomplete terminal");

    assert_eq!(response.full_text, "fresh");
    assert!(
        response
            .http_summary
            .as_deref()
            .unwrap_or_default()
            .contains("cache_miss=missing_continuation")
    );
    let captured = ws.captured();
    assert_eq!(captured.len(), 2);
    assert!(captured[1].get("previous_response_id").is_none());
    assert_eq!(captured[1]["input"], full_body["input"]);
}

#[tokio::test]
async fn codex_auto_with_distinct_scopes_uses_uncached_websockets() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "one",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "two",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Auto,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    let mut first = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    first.scope = LlmRequestScope::new("direct-a", "direct-a:frame", "direct-a:request");
    let mut second = request(vec![LlmMessage::text(LlmRole::User, "next")]);
    second.scope = LlmRequestScope::new("direct-b", "direct-b:frame", "direct-b:request");

    let first_response = provider.complete(first).await.expect("first response");
    let second_response = provider.complete(second).await.expect("second response");

    assert_eq!(first_response.full_text, "one");
    assert_eq!(second_response.full_text, "two");
    assert!(
        second_response
            .http_summary
            .as_deref()
            .unwrap_or_default()
            .contains("reused=false")
    );
    assert_eq!(ws.captured().len(), 2);
    let handshakes = ws.handshakes();
    assert_eq!(handshakes.len(), 2);
    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find_map(|(header_name, value)| (header_name == name).then_some(value.as_str()))
    }
    assert_eq!(header(&handshakes[0], "session-id"), Some("direct-a"));
    assert_eq!(
        header(&handshakes[0], "x-client-request-id"),
        Some("direct-a:request")
    );
    assert_eq!(header(&handshakes[1], "session-id"), Some("direct-b"));
    assert_eq!(
        header(&handshakes[1], "x-client-request-id"),
        Some("direct-b:request")
    );
}

#[tokio::test]
async fn codex_uncached_websockets_survive_a_failed_accept_between_connections() {
    // The scripted server's accept loop must outlive a failed `accept()`. An
    // uncached transport opens one connection per request, so a loop that ends
    // on the first error leaves every later request without a server: the
    // client falls back to the unroutable HTTP endpoint and the turn fails,
    // which is how FIG-1267 showed up on an oversubscribed runner (a descriptor
    // limit hit between two handshakes). The injected fault puts that failure
    // exactly in the gap between the two connections.
    let ws = spawn_scripted_websocket_with_injected_accept_faults(
        vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "one",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "two",
            },
        ],
        vec![InjectedAcceptFault {
            after_accepted_connections: 1,
            kind: std::io::ErrorKind::ConnectionAborted,
        }],
    )
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Auto,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    let mut first = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    first.scope = LlmRequestScope::new("direct-a", "direct-a:frame", "direct-a:request");
    let mut second = request(vec![LlmMessage::text(LlmRole::User, "next")]);
    second.scope = LlmRequestScope::new("direct-b", "direct-b:frame", "direct-b:request");

    let first_response = provider.complete(first).await.expect("first response");
    let second_response = provider.complete(second).await.expect("second response");

    assert_eq!(first_response.full_text, "one");
    assert_eq!(second_response.full_text, "two");
    assert_eq!(
        ws.handshakes().len(),
        2,
        "the second connection must reach the scripted server over WebSocket"
    );
    assert!(
        second_response
            .http_summary
            .as_deref()
            .unwrap_or_default()
            .contains("reused=false"),
        "the second request must be a fresh connection, not a reused one"
    );
}

#[tokio::test]
async fn codex_scripted_websocket_accept_loop_retries_every_error_kind() {
    // The loop retries without classifying. That is the whole point of the
    // FIG-1267 fix: `accept(2)` reports the pending-connection errnos (EPROTO,
    // ENETDOWN, ENETUNREACH, EHOSTUNREACH) with no stable `ErrorKind`, so any
    // allowlist of survivable kinds re-creates the silent death for exactly the
    // errors it failed to name. `ErrorKind::Other` stands in for that
    // uncategorised class here — `ErrorKind::Uncategorized` is unstable and
    // cannot be constructed from a test — alongside two kinds no plausible
    // allowlist would carry.
    let ws = spawn_scripted_websocket_with_injected_accept_faults(
        vec![ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "one",
        }],
        vec![
            InjectedAcceptFault {
                after_accepted_connections: 0,
                kind: std::io::ErrorKind::Other,
            },
            InjectedAcceptFault {
                after_accepted_connections: 0,
                kind: std::io::ErrorKind::PermissionDenied,
            },
            InjectedAcceptFault {
                after_accepted_connections: 0,
                kind: std::io::ErrorKind::InvalidData,
            },
        ],
    )
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Auto,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    let mut only = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    only.scope = LlmRequestScope::new("direct-a", "direct-a:frame", "direct-a:request");

    let response = provider.complete(only).await.expect("response");

    assert_eq!(response.full_text, "one");
    assert_eq!(
        ws.handshakes().len(),
        1,
        "the connection must reach the scripted server after the injected faults"
    );
    assert_eq!(
        ws.accept_failure(),
        None,
        "three failures in a row is far short of the give-up bound"
    );
}

#[tokio::test]
async fn codex_scripted_websocket_accept_loop_gives_up_loudly_after_the_bound() {
    // A listener that never recovers must not leave the test hanging or, worse,
    // failing as a client-side transport error. Past the bound the loop records
    // why it stopped, and `accept_failure()` is the seam a test reads before
    // blaming the code under test.
    let faults = std::iter::repeat_n(
        InjectedAcceptFault {
            after_accepted_connections: 0,
            kind: std::io::ErrorKind::Other,
        },
        ws_testing::MAX_CONSECUTIVE_ACCEPT_FAILURES as usize,
    )
    .collect();
    let ws = spawn_scripted_websocket_with_injected_accept_faults(Vec::new(), faults).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    let reason = loop {
        if let Some(reason) = ws.accept_failure() {
            break reason;
        }
        assert!(
            Instant::now() < deadline,
            "the accept loop must give up within the bound, not hang"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    assert!(
        reason.contains("scripted websocket server stopped listening"),
        "the give-up must name the harness, got {reason}"
    );
    assert!(
        reason.contains(&ws_testing::MAX_CONSECUTIVE_ACCEPT_FAILURES.to_string()),
        "the give-up must report how many failures in a row it saw, got {reason}"
    );
}

#[tokio::test]
async fn codex_auto_allocation_only_event_failure_does_not_fallback_to_sse() {
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::AllocationThenError {
        message_id: "msg_empty",
        message: "stream exploded",
    }])
    .await;
    let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
    let mut provider =
        websocket_test_provider(CodexTransport::Auto, http.url.clone(), ws.url.clone());

    let err = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect_err("events-seen websocket failure");
    assert!(err.message.contains("stream exploded"));
    assert!(!err.output_started, "allocation alone is not host output");
    assert_eq!(http.captured_len(), 0, "SSE must not replay the request");
    assert_eq!(ws.captured().len(), 1);
}

#[tokio::test]
async fn codex_auto_output_started_failure_does_not_fallback_to_sse() {
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::MidStreamError {
        message_id: "msg_1",
        text: "partial",
        message: "stream exploded",
    }])
    .await;
    let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
    let mut provider =
        websocket_test_provider(CodexTransport::Auto, http.url.clone(), ws.url.clone());

    let err = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect_err("output-started websocket failure");
    assert!(err.message.contains("stream exploded"));
    assert!(err.output_started, "the text delta commits provider output");
    assert_eq!(http.captured_len(), 0, "SSE must not replay the request");
    assert_eq!(ws.captured().len(), 1);
}

#[tokio::test]
async fn codex_sse_stream_evidence_carries_allowlisted_response_headers() {
    let http = spawn_http_sse_with_headers(
        "resp_http",
        "msg_http",
        "done",
        vec![
            ("x-request-cost".to_string(), "0.05".to_string()),
            ("set-cookie".to_string(), "secret".to_string()),
        ],
    )
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Sse,
        http.url.clone(),
        "ws://127.0.0.1:9/unused".to_string(),
    )
    .with_options(ProviderOptions {
        reliability: ProviderReliability::codex()
            .request_timeout(Some(RequestTimeout::Millis(5_000)))
            .stream_chunk_timeout_ms(Some(50)),
        response_metadata_headers: vec!["X-Request-Cost".to_string()],
        ..ProviderOptions::default()
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_sink = Arc::clone(&events);
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.stream_events = Some(lash_core::llm::types::LlmEventSender::new(move |event| {
        event_sink.lock_recover().push(event);
    }));

    let response = provider.complete(req).await.expect("SSE response");

    let expected_route = provider.route_identity("gpt-5.4");
    assert!(response.parts.iter().any(|part| {
        matches!(
            part,
            LlmOutputPart::Text {
                response_meta: Some(meta),
                ..
            } if meta.origin.as_ref() == Some(&expected_route)
        )
    }));
    assert!(events.lock_recover().iter().any(|event| {
        matches!(
            event,
            lash_core::llm::types::LlmStreamEvent::Part(LlmOutputPart::Text {
                response_meta: Some(meta),
                ..
            }) if meta.origin.as_ref() == Some(&expected_route)
        )
    }));

    assert!(events.lock_recover().iter().any(|event| {
        matches!(
            event,
            lash_core::llm::types::LlmStreamEvent::Evidence(evidence)
                if evidence.response_metadata.get("header:x-request-cost")
                    == Some(&json!("0.05"))
                    && !evidence.response_metadata.contains_key("header:set-cookie")
        )
    }));
}

#[tokio::test]
async fn codex_auto_skips_websocket_while_session_fallback_is_active() {
    let http = spawn_http_sse_sequence(vec![
        ("resp_http_1", "msg_http_1", "fallback-one"),
        ("resp_http_2", "msg_http_2", "fallback-two"),
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Auto,
        http.url.clone(),
        "ws://127.0.0.1:1/codex/responses".to_string(),
    );

    let first = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("first SSE fallback response");

    assert_eq!(first.full_text, "fallback-one");
    assert!(
        provider
            .websocket_fallback_reason(&request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .is_some()
    );

    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp_ws",
        message_id: "msg_ws",
        text: "should-not-run",
    }])
    .await;
    provider.websocket_url = ws.url.clone();
    let second = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "next")]))
        .await
        .expect("second SSE fallback response");

    assert_eq!(second.full_text, "fallback-two");
    assert_eq!(ws.captured().len(), 0);
    assert_eq!(http.captured_len(), 2);
    let sse_request = http.captured().remove(0);
    assert!(sse_request.contains("session-id: session-1"));
    assert!(sse_request.contains("x-client-request-id: session-1:request:test"));
    assert!(!sse_request.contains("session_id:"));
}

#[tokio::test]
async fn codex_websocket_output_started_error_stops_provider_handle_retry() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::IdleAfterStart {
            message_id: "msg_paid",
            text: "paid partial",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_second",
            message_id: "msg_second",
            text: "second generation",
        },
    ])
    .await;
    let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
    let provider =
        websocket_test_provider(CodexTransport::Websocket, http.url.clone(), ws.url.clone())
            .with_options(ProviderOptions {
                reliability: ProviderReliability::codex()
                    .request_timeout(Some(RequestTimeout::Millis(5_000)))
                    .stream_chunk_timeout_ms(Some(50))
                    .max_attempts(2)
                    .base_delay_ms(0)
                    .max_delay_ms(0),
                ..ProviderOptions::default()
            });
    let mut handle = ProviderHandle::new(provider.into_components());

    let result = handle
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await;

    assert_eq!(
        ws.captured().len(),
        1,
        "paid WebSocket output must not be re-bought"
    );
    let failure = result.expect_err("output-started WebSocket failure must stop the ladder");
    assert_eq!(
        failure.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    assert!(!failure.is_retryable());
    assert_eq!(http.captured_len(), 0);
}

#[tokio::test]
async fn codex_websocket_output_started_forced_delay_pins_hardened_ordering() {
    // Regression law for FIG-1414: on the old ordering (empty allocation frame
    // first), a >50ms stall between frames caused the stream chunk timeout to
    // snapshot output_started=false and retry unsafely. Under the hardened
    // ordering, the first observable frame itself is paid-output evidence, so
    // any subsequent stall or idle window halts the retry ladder immediately
    // without re-buying.
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::IdleAfterStart {
            message_id: "msg_paid",
            text: "paid partial",
        },
        ScriptedWsAction::Complete {
            response_id: "resp_second",
            message_id: "msg_second",
            text: "second generation",
        },
    ])
    .await;
    let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
    let provider =
        websocket_test_provider(CodexTransport::Websocket, http.url.clone(), ws.url.clone())
            .with_options(ProviderOptions {
                reliability: ProviderReliability::codex()
                    .request_timeout(Some(RequestTimeout::Millis(5_000)))
                    .stream_chunk_timeout_ms(Some(50))
                    .max_attempts(2)
                    .base_delay_ms(0)
                    .max_delay_ms(0),
                ..ProviderOptions::default()
            });
    let mut handle = ProviderHandle::new(provider.into_components());

    let result = handle
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await;

    assert_eq!(
        ws.captured().len(),
        1,
        "paid WebSocket output must not be re-bought"
    );
    let failure = result.expect_err("output-started WebSocket failure must stop the ladder");
    assert_eq!(
        failure.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    assert!(!failure.is_retryable());
    assert_eq!(http.captured_len(), 0);
}

#[tokio::test]
async fn codex_websocket_clean_eof_requires_terminal_event_unless_explicitly_tolerated() {
    let action = ScriptedWsAction::CloseAfterStart {
        response_id: "resp_partial",
        message_id: "msg_partial",
        text: "partial",
    };
    let strict_ws = spawn_scripted_websocket(vec![action.clone()]).await;
    let http = spawn_http_sse("resp_http", "msg_http", "unused").await;
    let mut strict = websocket_test_provider(
        CodexTransport::Websocket,
        http.url.clone(),
        strict_ws.url.clone(),
    );

    let error = strict
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect_err("clean EOF without a terminal event must fail");

    assert_eq!(
        error.code.as_deref(),
        Some("websocket_closed_before_completed")
    );
    let partial = error.partial_response.as_deref().expect("partial response");
    assert_eq!(partial.full_text, "partial");
    assert_eq!(partial.usage.input_tokens, 4);
    assert_eq!(partial.usage.output_tokens, 1);
    assert!(partial.provider_usage.is_some());
    assert_eq!(http.captured_len(), 0);

    let tolerant_ws = spawn_scripted_websocket(vec![action]).await;
    let mut tolerant = websocket_test_provider(
        CodexTransport::Websocket,
        http.url.clone(),
        tolerant_ws.url.clone(),
    );
    let mut tolerant_request = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    tolerant_request.model_capability.stream_termination = Some(StreamTermination::EofTolerated);
    let response = tolerant
        .complete(tolerant_request)
        .await
        .expect("explicit EOF tolerance accepts clean close");
    assert_eq!(response.full_text, "partial");
    assert_eq!(http.captured_len(), 0);
}

#[test]
fn codex_schema_projection_failure_is_local_validation_error() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "bad".to_string(),
        schema: json!({"type": "object", "allOf": []}).into(),
        strict: true,
    }));

    let err = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, false)
        .unwrap_err();
    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("allOf"));
}

#[test]
fn codex_stream_response_carries_raw_usage_sidecar() {
    let mut state = CodexStreamState::default();
    process_event(
        &mut state,
        json!({"type":"response.completed","response":{
            "id":"resp_usage",
            "status":"completed",
            "output":[assistant_item("msg_usage","hi")],
            "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
        }}),
    );

    let response = response_from_state(state);

    assert_eq!(
        response.provider_usage,
        Some(json!({"input_tokens":3,"output_tokens":2,"total_tokens":5}))
    );
    assert_eq!(response.usage.input_tokens, 3);
    assert_eq!(response.usage.output_tokens, 2);
}

#[test]
fn codex_stream_assembles_single_message_item_once() {
    let mut state = CodexStreamState::default();

    process_event(
        &mut state,
        json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1","status":"in_progress","phase":"commentary"}}),
    );
    process_event(
        &mut state,
        json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Hel"}),
    );
    process_event(
        &mut state,
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Hello"}]}}),
    );

    let response = response_from_state(state);
    assert_eq!(response.full_text, "Hello");
    assert_eq!(response.parts.len(), 1);
    assert_eq!(
        response.parts[0],
        LlmOutputPart::Text {
            text: "Hello".to_string(),
            response_meta: Some(ResponseTextMeta {
                id: Some("msg_1".to_string()),
                status: Some("completed".to_string()),
                phase: Some("commentary".to_string()),
                ..ResponseTextMeta::default()
            }),
        }
    );
}

#[test]
fn codex_stream_replayed_message_item_does_not_duplicate_text() {
    let mut state = CodexStreamState::default();

    for event in [
        json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"The sentence."}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"The sentence."}]}}),
        json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"The sentence."}]}}),
    ] {
        process_event(&mut state, event);
    }

    let response = response_from_state(state);
    assert_eq!(response.full_text, "The sentence.");
    assert_eq!(
        response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        1
    );
}

#[test]
fn codex_stream_completed_response_merges_existing_message_by_id() {
    let mut state = CodexStreamState::default();

    for event in [
        json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Final answer."}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Final answer."}]}}),
        json!({"type":"response.completed","response":{"id":"resp_1","output_text":"Final answer.","output":[{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Final answer."}]}]}}),
    ] {
        process_event(&mut state, event);
    }

    let response = response_from_state(state);
    assert_eq!(response.full_text, "Final answer.");
    assert_eq!(response.parts.len(), 1);
}

#[test]
fn codex_stream_distinct_message_ids_stay_separate_without_inserted_separator() {
    let mut state = CodexStreamState::default();

    for event in [
        json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"One."}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"One."}]}}),
        json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_2"}}),
        json!({"type":"response.output_text.delta","item_id":"msg_2","delta":"Two."}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_2","status":"completed","content":[{"type":"output_text","text":"Two."}]}}),
    ] {
        process_event(&mut state, event);
    }

    let response = response_from_state(state);
    assert_eq!(response.full_text, "One.Two.");
    assert_eq!(response.parts.len(), 2);
    assert_eq!(
        response.parts,
        vec![
            LlmOutputPart::Text {
                text: "One.".to_string(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: None,
                    ..ResponseTextMeta::default()
                }),
            },
            LlmOutputPart::Text {
                text: "Two.".to_string(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_2".to_string()),
                    status: Some("completed".to_string()),
                    phase: None,
                    ..ResponseTextMeta::default()
                }),
            },
        ]
    );
}

#[test]
fn codex_stream_preserves_reasoning_message_and_tool_call_once() {
    let mut state = CodexStreamState::default();
    let mut emitted_parts = Vec::new();

    for event in [
        json!({"type":"response.reasoning_summary_part.added"}),
        json!({"type":"response.reasoning_summary_text.delta","delta":"Think"}),
        json!({"type":"response.reasoning_summary_part.done"}),
        json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Think"}],"encrypted_content":"enc"}}),
        json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1","phase":"final_answer"}}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Hi"}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Hi"}]}}),
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"x\""}),
        json!({"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"x\":1}"}),
        json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":"{\"x\":1}","status":"completed"}}),
    ] {
        process_event_with_parts(&mut state, event, &mut emitted_parts);
    }
    process_event_with_parts(
        &mut state,
        json!({"type":"response.completed","response":{"id":"resp_1","output":[
                {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Think"}],"encrypted_content":"enc"},
                {"type":"message","id":"msg_1","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Hi"}]},
                {"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":"{\"x\":1}","status":"completed"}
            ],"output_text":"Hi"}}),
        &mut emitted_parts,
    );

    let response = response_from_state(state);
    assert_eq!(response.full_text, "Hi");
    assert_eq!(emitted_parts.len(), 3);
    assert_eq!(
        emitted_parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Reasoning { .. }))
            .count(),
        1
    );
    assert_eq!(
        emitted_parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        1
    );
    assert_eq!(
        emitted_parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
            .count(),
        1
    );
    assert_eq!(
        response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Reasoning { .. }))
            .count(),
        1
    );
    assert_eq!(
        response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        1
    );
    assert_eq!(
        response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
            .count(),
        1
    );
}

/// Cross-provider response-normalization conformance. Codex shares OpenAI's
/// Responses-API normalizers (`shared::*`), so this wires those into the
/// shared suite with Responses-API wire fixtures.
#[cfg(feature = "testing")]
mod conformance {
    use super::super::{PROVIDER, shared};
    use super::{CodexProvider, request};
    use lash_core::llm::types::{LlmMessage, LlmOutputPart, LlmTerminalReason, LlmUsage};
    use lash_core::provider::Provider;
    use lash_llm_transport::conformance::{
        CanonicalUsage as U, ProviderConformanceSpec, ProviderNormalizer, ProviderWire, Scenario,
        StreamAssembly, provider_conformance,
    };
    use lash_llm_transport::{
        openai_terminal_reason_from_response_value, openai_usage_from_response_value,
    };
    use serde_json::{Value, json};

    struct CodexNormalizer;

    impl ProviderNormalizer for CodexNormalizer {
        fn name(&self) -> &str {
            "codex-responses"
        }

        fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire> {
            let wire = match scenario {
                    Scenario::PlainTextStop => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "completed",
                            "content": [{ "type": "output_text", "text": "hello" }]
                        }],
                        "usage": { "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT }
                    })),
                    Scenario::OutputCapped => ProviderWire::body(json!({
                        "status": "incomplete",
                        "incomplete_details": { "reason": "max_output_tokens" },
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "incomplete",
                            "content": [{ "type": "output_text", "text": "trunc" }]
                        }]
                    })),
                    Scenario::ContentFilter => ProviderWire::body(json!({
                        "status": "incomplete",
                        "incomplete_details": { "reason": "content_filter" },
                        "output": []
                    })),
                    Scenario::NonStreamingToolUse => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "function_call", "id": "fc_1", "call_id": "call_1",
                            "name": "lookup", "arguments": "{\"q\":\"x\"}", "status": "completed"
                        }]
                    })),
                    Scenario::StreamingTextAssembly => {
                        ProviderWire::body(json!({})).with_text_stream(
                            vec![
                                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"hello "}"#.to_string(),
                                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"world"}"#.to_string(),
                            ],
                            "hello world",
                        )
                    }
                    Scenario::StreamingToolArgumentMerge => {
                        ProviderWire::body(json!({})).with_tool_call_stream(
                            vec![
                                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#.to_string(),
                                // arguments deliberately split across two delta events
                                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"q\":"}"#.to_string(),
                                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"x\"}"}"#.to_string(),
                                r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}","status":"completed"}}"#.to_string(),
                            ],
                            "lookup",
                            json!({ "q": "x" }),
                        )
                    }
                    Scenario::StreamingToolCallAbortEquivalence => {
                        ProviderWire::body(json!({})).with_aborted_tool_call_stream(
                            vec![
                                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_abort","call_id":"call_abort","name":"lookup","arguments":""}}).to_string(),
                                json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_abort","delta":"{\"q\":\"x\"}"}).to_string(),
                                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_abort","call_id":"call_abort","name":"lookup","arguments":"{\"q\":\"x\"}","status":"completed"}}).to_string(),
                            ],
                            "lookup",
                            json!({ "q": "x" }),
                        )
                    }
                    Scenario::UsageCacheHit => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "completed",
                            "content": [{ "type": "output_text", "text": "ok" }]
                        }],
                        "usage": {
                            "input_tokens": U::BASE_INPUT,
                            "output_tokens": U::BASE_OUTPUT,
                            "input_tokens_details": { "cached_tokens": U::CACHED_INPUT }
                        }
                    })),
                    Scenario::UsageReasoning => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "completed",
                            "content": [{ "type": "output_text", "text": "ok" }]
                        }],
                        "usage": {
                            "input_tokens": U::BASE_INPUT,
                            "output_tokens": U::OUTPUT_WITH_REASONING,
                            "output_tokens_details": { "reasoning_tokens": U::REASONING }
                        }
                    })),
                    Scenario::ReasoningExtraction => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [
                            {
                                "type": "reasoning", "id": "rs_1",
                                "summary": [{ "type": "summary_text", "text": "thinking about it" }]
                            },
                            {
                                "type": "message", "id": "msg_1", "status": "completed",
                                "content": [{ "type": "output_text", "text": "answer" }]
                            }
                        ]
                    }))
                    .with_reasoning_text("thinking about it"),
                    Scenario::ReasoningReplayRoundTrip => {
                        // Codex shares the Responses-API fixtures with the
                        // OpenAI adapter; the dialect is the same wire.
                        crate::tests::conformance::reasoning_replay_wire("codex-responses")
                    }
                    Scenario::ToolCallReplayRoundTrip => {
                        crate::tests::conformance::tool_call_replay_wire()
                    }
                    Scenario::StreamingUsageMerge => {
                        ProviderWire::body(json!({})).with_usage_merge_stream(vec![
                            // input arrives on an early event
                            format!(
                                r#"{{"type":"response.output_text.delta","delta":"hi","usage":{{"input_tokens":{}}}}}"#,
                                U::BASE_INPUT
                            ),
                            // output arrives on a later event; merge must keep input
                            format!(
                                r#"{{"type":"response.output_text.delta","delta":"!","usage":{{"output_tokens":{}}}}}"#,
                                U::BASE_OUTPUT
                            ),
                        ])
                    }
                };
            Some(wire)
        }

        fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart> {
            shared::response_parts_from_value(body)
        }

        fn usage_from_wire(&self, body: &Value) -> LlmUsage {
            openai_usage_from_response_value(body)
        }

        fn terminal_from_wire(&self, body: &Value, parts: &[LlmOutputPart]) -> LlmTerminalReason {
            openai_terminal_reason_from_response_value(body, parts)
        }

        fn assemble_stream(&self, scenario: Scenario, sse_events: &[String]) -> StreamAssembly {
            let mut state = shared::ResponsesStreamState::default();
            let mut stream_events = Vec::new();
            let provider = CodexProvider::new("access", "refresh", 0);
            let route = provider.route_identity("gpt-5.4");
            for raw in sse_events {
                let mut emitted_parts = Vec::new();
                let capture_parts = matches!(scenario, Scenario::StreamingToolCallAbortEquivalence)
                    .then_some(&mut emitted_parts);
                shared::process_sse_event(PROVIDER, raw, &mut state, capture_parts)
                    .expect("responses sse event parses");
                for part in &mut emitted_parts {
                    part.stamp_replay_origin(&route)
                        .expect("conformance stream output accepts its minting route");
                }
                stream_events.extend(
                    emitted_parts
                        .into_iter()
                        .map(lash_core::llm::types::LlmStreamEvent::Part),
                );
            }
            let mut parts = state.response_parts();
            for part in &mut parts {
                part.stamp_replay_origin(&route)
                    .expect("conformance output accepts its minting route");
            }
            StreamAssembly {
                parts,
                usage: state.usage.clone(),
                stream_events,
            }
        }

        fn build_next_request(&self, _scenario: Scenario, messages: Vec<LlmMessage>) -> Value {
            CodexProvider::new("access", "refresh", 0)
                .build_request_body(&request(messages), false)
                .expect("codex next request serializes")
        }
    }

    struct CodexRlmHistoryNormalizer;

    impl ProviderNormalizer for CodexRlmHistoryNormalizer {
        fn name(&self) -> &str {
            "codex-rlm-history"
        }

        fn conformance_spec(&self) -> ProviderConformanceSpec {
            ProviderConformanceSpec::with_unsupported(&[(
                Scenario::ToolCallReplayRoundTrip,
                "RLM history carries no native tool calls; tool use is projected into lashlang \
                 cells, so there is no function_call item to replay",
            )])
        }

        fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire> {
            if matches!(scenario, Scenario::ToolCallReplayRoundTrip) {
                return None;
            }
            CodexNormalizer.wire_for(scenario)
        }

        fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart> {
            CodexNormalizer.parts_from_wire(body)
        }

        fn usage_from_wire(&self, body: &Value) -> LlmUsage {
            CodexNormalizer.usage_from_wire(body)
        }

        fn terminal_from_wire(&self, body: &Value, parts: &[LlmOutputPart]) -> LlmTerminalReason {
            CodexNormalizer.terminal_from_wire(body, parts)
        }

        fn assemble_stream(&self, scenario: Scenario, sse_events: &[String]) -> StreamAssembly {
            CodexNormalizer.assemble_stream(scenario, sse_events)
        }

        fn build_next_request(&self, scenario: Scenario, messages: Vec<LlmMessage>) -> Value {
            let rendered =
                lash_protocol_rlm::project_conformance_messages_through_rlm_history(messages);
            assert!(
                rendered.is_ok(),
                "RLM conformance history bridge failed: {}",
                rendered.as_ref().err().map(String::as_str).unwrap_or("")
            );
            let messages = rendered.unwrap_or_default();
            CodexNormalizer.build_next_request(scenario, messages)
        }
    }

    #[test]
    fn codex_satisfies_provider_conformance() {
        provider_conformance(&CodexNormalizer);
    }

    #[test]
    fn codex_rlm_history_satisfies_provider_conformance() {
        provider_conformance(&CodexRlmHistoryNormalizer);
    }
}
