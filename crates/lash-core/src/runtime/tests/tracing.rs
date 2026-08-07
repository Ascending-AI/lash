use super::*;
use ::tracing::Instrument;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

#[derive(Clone, Debug, Default)]
struct SpanCapture {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedSpan {
    name: String,
    parent: Option<String>,
}

impl SpanCapture {
    fn snapshot(&self) -> Vec<CapturedSpan> {
        self.spans.lock().expect("lock captured spans").clone()
    }
}

impl<S> Layer<S> for SpanCapture
where
    S: ::tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        _attrs: &::tracing::span::Attributes<'_>,
        id: &::tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("new span present in registry");
        let captured = CapturedSpan {
            name: span.metadata().name().to_string(),
            parent: span
                .parent()
                .map(|parent| parent.metadata().name().to_string()),
        };
        self.spans
            .lock()
            .expect("lock captured spans")
            .push(captured);
    }
}

#[tokio::test]
async fn runtime_session_graph_service_routes_rolling_history_event_to_real_sink() {
    let trace_path = std::env::temp_dir().join(format!(
        "lash-runtime-plugin-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let runtime = standard_runtime_with_transport_and_host(
        mock_provider(Vec::new()),
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;
    let graph = runtime
        .session_graph_service()
        .expect("resident runtime exposes its graph service");

    graph
        .emit_trace_event(
            crate::TraceContext::default().for_session("emitter-supplied-id"),
            crate::TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 0 },
        )
        .await
        .expect("runtime graph service should route trace records");

    let logged = std::fs::read_to_string(&trace_path).expect("read runtime trace sink");
    let record: crate::TraceRecord =
        serde_json::from_str(logged.lines().next().expect("one trace record"))
            .expect("typed trace record");
    assert_eq!(record.context.session_id.as_deref(), Some("root"));
    assert!(matches!(
        record.event,
        crate::TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 0 }
    ));
    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn provider_spans_are_children_of_the_turn_span() {
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_request| async {
            let provider_span = ::tracing::info_span!("provider.complete");
            drop(provider_span);
            Ok(LlmResponse {
                full_text: "done".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build();
    let mut runtime = standard_runtime_with_transport(provider).await;
    let capture = SpanCapture::default();
    let subscriber = Registry::default().with(capture.clone());
    ::tracing::subscriber::set_global_default(subscriber).expect("install capture subscriber");
    let turn_span = ::tracing::info_span!("runtime.turn");

    runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "provider-span-parentage"),
        )
        .instrument(turn_span)
        .await
        .expect("turn");

    let spans = capture.snapshot();
    assert_eq!(
        spans
            .iter()
            .find(|span| span.name == "provider.complete")
            .and_then(|span| span.parent.as_deref()),
        Some("runtime.turn"),
        "provider span must inherit the turn span; captured spans: {spans:?}"
    );
}

#[tokio::test]
async fn standard_runtime_emits_single_tool_call_trace_pair_per_call() {
    // A standard-mode tool call must produce exactly one ToolCallStarted and
    // one ToolCallCompleted trace record: the emission moved to the shared
    // tool-execution seam, and the old standard-only path must not double it.
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "call-1".to_string(),
                    tool_name: "echo_tool".to_string(),
                    input_json: r#"{"value":"sample"}"#.to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "done".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-tool-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "call the tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "trace-standard-tool-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();

    let started = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_started"))
        .collect::<Vec<_>>();
    let completed = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_completed"))
        .collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        1,
        "expected exactly one ToolCallStarted trace: {entries:?}"
    );
    assert_eq!(
        completed.len(),
        1,
        "expected exactly one ToolCallCompleted trace: {entries:?}"
    );
    assert_eq!(
        started[0].get("call_id").and_then(|v| v.as_str()),
        Some("call-1")
    );
    assert_eq!(
        started[0].get("name").and_then(|v| v.as_str()),
        Some("echo_tool")
    );
    // Span identity is stamped from session/turn context so the tool nests
    // under its turn as `tool:<call_id>`.
    assert_eq!(
        completed[0]
            .get("context")
            .and_then(|context| context.get("graph_node_id"))
            .and_then(|v| v.as_str()),
        Some("tool:call-1")
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn standard_runtime_trace_records_stream_event_entries() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("Hello ".to_string()),
            LlmStreamEvent::Delta("world".to_string()),
            LlmStreamEvent::Part(LlmOutputPart::Text {
                text: "Hello world".to_string(),
                response_meta: None,
            }),
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
            full_text: "Hello world".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Hello world".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path_and_stream_events(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "trace-stream-events-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();

    assert!(
        entries
            .iter()
            .any(|entry| entry.get("type").and_then(|v| v.as_str())
                == Some("runtime_stream_event")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("event_name"))
                    .and_then(|v| v.as_str())
                    == Some("delta")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("raw_text"))
                    .and_then(|v| v.as_str())
                    == Some("Hello ")),
        "expected delta stream event in trace: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.get("type").and_then(|v| v.as_str())
                == Some("runtime_stream_event")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("event_name"))
                    .and_then(|v| v.as_str())
                    == Some("text_part")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("raw_text"))
                    .and_then(|v| v.as_str())
                    == Some("Hello world")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("visible_text"))
                    .is_none_or(|v| v.is_null())),
        "expected text_part stream event in trace: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_completed")),
        "expected final llm trace entry in trace: {entries:?}"
    );
    let response_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_completed"))
        .expect("completed llm call entry");
    let stream_summary = response_entry
        .get("stream_summary")
        .and_then(|value| value.as_object())
        .expect("stream summary");
    assert_eq!(
        stream_summary
            .get("text_delta_count")
            .and_then(|value| value.as_u64()),
        Some(2)
    );
    assert_eq!(
        stream_summary
            .get("visible_chunk_count")
            .and_then(|value| value.as_u64()),
        Some(2)
    );
    assert_eq!(
        stream_summary
            .get("max_visible_chunk_chars")
            .and_then(|value| value.as_u64()),
        Some(6)
    );
    let avg_chunk_chars = stream_summary
        .get("avg_visible_chunk_chars")
        .and_then(|value| value.as_f64())
        .expect("avg visible chunk chars");
    assert!((avg_chunk_chars - 5.5).abs() < f64::EPSILON);
    assert!(
        stream_summary
            .get("first_visible_token_latency_ms")
            .is_some_and(|value| !value.is_null())
    );
    assert!(
        stream_summary
            .get("stream_duration_ms")
            .is_some_and(|value| !value.is_null())
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn extended_runtime_trace_records_provider_request_and_stream_events() {
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|req| async move {
            if let Some(tx) = req.provider_trace.as_ref() {
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "responses",
                    serde_json::json!({
                        "model": "mock-model",
                        "input": "x".repeat(3_000),
                    })
                    .to_string(),
                ));
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "chat/completions",
                    r#"{"model":"small"}"#.to_string(),
                ));
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "invalid",
                    "not-json".to_string(),
                ));
                tx.send(LlmProviderTraceEvent {
                    provider: "mock",
                    event_name: "response.output_item.done".to_string(),
                    raw: serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": { "id": "msg_1" }
                    })
                    .to_string(),
                });
                tx.send(LlmProviderTraceEvent {
                    provider: "mock",
                    event_name: "response.output_item.done".to_string(),
                    raw: serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 1,
                        "item": { "id": "msg_2" }
                    })
                    .to_string(),
                });
            }
            Ok(LlmResponse {
                full_text: "Hello".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "Hello".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let trace_path = std::env::temp_dir().join(format!(
        "lash-provider-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path_and_stream_events(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "trace-provider-stream-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();
    let provider_events = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("provider_stream_event"))
        .collect::<Vec<_>>();
    let provider_requests = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("provider_request"))
        .collect::<Vec<_>>();
    assert_eq!(provider_requests.len(), 3, "provider traces: {entries:?}");
    let request_event = provider_requests
        .iter()
        .find(|entry| entry["event"]["endpoint"] == "responses")
        .expect("large provider request")["event"]
        .clone();
    let expected_body = serde_json::json!({
        "model": "mock-model",
        "input": "x".repeat(3_000),
    });
    let expected_serialized = expected_body.to_string();
    assert_eq!(request_event["provider"], "mock");
    assert_eq!(request_event["endpoint"], "responses");
    assert!(request_event.get("body_json").is_none());
    assert_eq!(request_event["body_json_omitted_reason"], "size_limit");
    assert_eq!(request_event["body_len"], expected_serialized.len());
    assert!(request_event["body_len"].as_u64().unwrap() > 2_048);
    assert_eq!(
        request_event["body_sha256"],
        lash_trace::sha256_hex(expected_serialized.as_bytes())
    );
    let small_request = provider_requests
        .iter()
        .find(|entry| entry["event"]["endpoint"] == "chat/completions")
        .expect("small provider request");
    assert_eq!(small_request["event"]["body_json"]["model"], "small");
    assert!(
        small_request["event"]
            .get("body_json_omitted_reason")
            .is_none()
    );
    let invalid_request = provider_requests
        .iter()
        .find(|entry| entry["event"]["endpoint"] == "invalid")
        .expect("invalid provider request");
    assert!(invalid_request["event"].get("body_json").is_none());
    assert_eq!(
        invalid_request["event"]["body_json_omitted_reason"],
        "invalid_json"
    );
    assert_eq!(invalid_request["event"]["body_len"], "not-json".len());
    assert_eq!(
        invalid_request["event"]["body_sha256"],
        lash_trace::sha256_hex(b"not-json")
    );
    assert_eq!(
        provider_events.len(),
        2,
        "provider trace entries: {entries:?}"
    );
    assert_eq!(provider_events[0]["event"]["item_id"], "msg_1");
    assert_eq!(provider_events[0]["event"]["output_index"], 0);
    assert_eq!(provider_events[1]["event"]["item_id"], "msg_2");
    assert_eq!(
        provider_events[1]["event"]["raw_json"]["item"]["id"],
        "msg_2"
    );
    assert!(
        provider_events[1]["event"]["raw_sha256"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty())
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn provider_request_trace_sender_requires_extended_level_and_sink() {
    async fn assert_sender_absent(host: EmbeddedRuntimeHost, turn_id: &str) {
        let transport = TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(|req| async move {
                assert!(req.provider_trace.is_none());
                Ok(LlmResponse {
                    full_text: "Hello".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "Hello".to_string(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            })
            .build();
        let mut runtime = standard_runtime_with_transport_and_host(transport, host).await;
        runtime
            .run_turn_assembled(
                TurnInput {
                    items: vec![InputItem::Text {
                        text: "hello".to_string(),
                    }],
                    protocol_turn_options: None,
                    trace_turn_id: None,
                    protocol_extension: None,
                    turn_context: crate::TurnContext::default(),
                },
                CancellationToken::new(),
                named_turn_scope("root", turn_id),
            )
            .await
            .expect("turn");
    }

    let trace_path = std::env::temp_dir().join(format!(
        "lash-provider-gate-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    assert_sender_absent(
        test_host_config_with_trace_path(trace_path.clone()),
        "standard-trace-level",
    )
    .await;

    let mut no_sink = test_host_config();
    no_sink.core.tracing.trace_level = lash_trace::TraceLevel::Extended;
    assert_sender_absent(no_sink, "extended-without-sink").await;

    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn standard_runtime_trace_omits_stream_event_entries_by_default() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("Hello ".to_string()),
            LlmStreamEvent::Delta("world".to_string()),
        ],
        response: Ok(LlmResponse {
            full_text: "Hello world".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Hello world".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-trace-summary-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "trace-standard-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();

    assert!(
        !entries.iter().any(
            |entry| entry.get("type").and_then(|v| v.as_str()) == Some("runtime_stream_event")
        ),
        "stream event entries should be opt-in: {entries:?}"
    );
    let response_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_completed"))
        .expect("completed llm call entry");
    assert!(
        response_entry
            .get("stream_summary")
            .is_some_and(|value| !value.is_null()),
        "stream summary should remain in completed LLM trace: {response_entry:?}"
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn standard_runtime_trace_records_failed_llm_calls() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Err(crate::llm::transport::LlmTransportError::new(
            "HTTP request failed: builder error",
        )
        .with_code("builder")
        .with_raw("transport raw body")
        .with_request_body("{\"model\":\"mock-model\"}")),
    }]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-trace-error-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "trace-failed-llm-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert_eq!(turn.errors.len(), 1);
    assert_eq!(turn.errors[0].raw.as_deref(), Some("transport raw body"));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();
    let error_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_failed"))
        .expect("llm error entry");
    assert_eq!(
        error_entry["error"]["message"].as_str(),
        Some("HTTP request failed: builder error")
    );
    assert_eq!(error_entry["error"]["code"].as_str(), Some("builder"));
    assert_eq!(
        error_entry["error"]["raw"].as_str(),
        Some("transport raw body")
    );
    let request_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_started"))
        .expect("llm request entry");
    assert_eq!(
        request_entry["request"]["model"].as_str(),
        Some("mock-model")
    );
}

#[test]
fn normalize_prompt_usage_uses_prompt_total() {
    let usage = TokenUsage {
        input_tokens: 80,
        output_tokens: 0,
        cache_read_input_tokens: 20,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    let prompt_usage = normalize_prompt_usage(&usage).expect("prompt usage");
    assert_eq!(prompt_usage.prompt_context_tokens, 100);
    assert_eq!(prompt_usage.context_budget_tokens, 100);
}
