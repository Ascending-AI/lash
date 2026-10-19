use super::*;

/// One label per output part in the order a turn sees it.
fn part_order(parts: &[LlmOutputPart]) -> Vec<String> {
    parts
        .iter()
        .map(|part| match part {
            LlmOutputPart::Text { text, .. } => format!("text:{text}"),
            LlmOutputPart::Reasoning { text, .. } => format!("reasoning:{text}"),
            LlmOutputPart::ToolCall { call_id, .. } => format!("tool:{call_id}"),
        })
        .collect()
}

/// Text, tool calls and reasoning interleave within one event and across events.
fn interleaved_wire_events(finish: bool) -> Vec<Value> {
    let mut last = json!({"response":{"candidates":[{"content":{"parts":[
        {"functionCall":{"id":"call-2","name":"lookup","args":{"q":"y"}}},
        {"text":"outro"}
    ]}}]}});
    if finish {
        last["response"]["candidates"][0]["finishReason"] = json!("STOP");
    }
    vec![
        json!({"response":{"candidates":[{"content":{"parts":[{"text":"intro "}]}}]}}),
        json!({"response":{"candidates":[{"content":{"parts":[
            {"functionCall":{"id":"call-1","name":"lookup","args":{"q":"x"}}},
            {"text":"between "},
            {"text":"mid-thought","thought":true,"thoughtSignature":REASONING_SIGNATURE_1}
        ]}}]}}),
        last,
    ]
}

async fn read_stream(
    wire_events: &[Value],
) -> (
    Result<lash_core::llm::types::LlmResponse, lash_transport_failure::LlmTransportError>,
    Vec<LlmStreamEvent>,
) {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let event_sink = Arc::clone(&events);
    let result =
        GoogleOAuthProvider::new(Arc::new(lash_core::provider::ProviderToken::new("access")))
            .with_transport(Arc::new(StaticSseTransport::new(sse_body(wire_events))))
            .execute_request(
                "access",
                json!({ "model": "gemini-test" }),
                Some(LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                })),
                None,
                crate::provider::ResponseReading {
                    stream_termination: StreamTermination::RequireTerminalEvidence,
                    defaults: lash_core::provider::LlmProfileRequestDefaults {
                        expose_thinking: true,
                        ..lash_core::provider::LlmProfileRequestDefaults::new(
                            lash_core::provider::CacheRetention::Short,
                        )
                    },
                },
                None,
            )
            .await;
    let events = events.lock_recover().clone();
    (result, events)
}

/// The completed response, the live stream (and so an aborted turn's synthesized
/// response) and a truncated stream's partial response all keep the wire order of
/// text, tool-call and reasoning parts, the same order the batch path reads.
#[tokio::test]
async fn google_stream_keeps_wire_order_across_text_tool_and_reasoning_parts() {
    let wire_order = [
        "text:intro ",
        "tool:call-1",
        "text:between ",
        "reasoning:mid-thought",
        "tool:call-2",
        "text:outro",
    ];
    let wire_events = interleaved_wire_events(true);
    let batch = GoogleOAuthProvider::for_test()
        .response_parts_from_value(&batch_response_from_stream_events(&wire_events), None);
    assert_eq!(part_order(&batch), wire_order, "batch reads wire order");

    let (completed, events) = read_stream(&wire_events).await;
    let completed = completed.expect("interleaved stream completes");
    assert_eq!(
        part_order(&completed.parts),
        wire_order,
        "completed response keeps wire order"
    );
    let aborted = lash_core::testing::response_synthesized_from_aborted_stream(&events);
    assert_eq!(
        part_order(&aborted.parts),
        wire_order,
        "live stream publishes in wire order; events were {events:?}"
    );

    let (truncated, _) = read_stream(&interleaved_wire_events(false)).await;
    let error = truncated.expect_err("a stream without finishReason is refused");
    let partial = error.partial_response.as_deref().expect("partial response");
    assert_eq!(
        part_order(&partial.parts),
        wire_order,
        "partial response keeps wire order"
    );
}
