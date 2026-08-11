use super::*;

#[test]
fn responses_output_started_requires_generated_evidence_not_allocated_slots() {
    let mut state = ResponsesStreamState::default();
    for event in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_empty"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_empty"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_empty","summary":[]}}"#,
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_empty","call_id":"call_empty","name":"","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","output_index":2,"item_id":"fc_empty","arguments":""}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_empty","call_id":"call_empty","name":"","arguments":""}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, None).unwrap();
    }
    assert!(!state.output_started());

    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.added","output_index":3,"item":{"type":"reasoning","id":"rs_paid"}}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"reasoning","id":"rs_paid","summary":[],"encrypted_content":"opaque"}}"#,
        &mut state,
        None,
    )
    .unwrap();
    assert!(state.output_started());
}

#[test]
fn responses_specific_content_events_are_recognized_as_output() {
    for event in [
        r#"{"type":"response.refusal.delta","output_index":0,"delta":"refused"}"#,
        r#"{"type":"response.refusal.done","output_index":0,"refusal":"refused"}"#,
        r#"{"type":"response.reasoning_text.delta","output_index":0,"delta":"reasoning"}"#,
        r#"{"type":"response.reasoning_text.done","output_index":0,"text":"reasoning"}"#,
        r#"{"type":"response.output_text.done","output_index":0,"text":"answer"}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_named","call_id":"call_named","name":"lookup","arguments":""}}"#,
    ] {
        let mut state = ResponsesStreamState::default();
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, None).unwrap();
        assert!(
            state.output_started(),
            "event must be output evidence: {event}"
        );
        assert!(
            !state.unrecognized_event_observed,
            "official content event must have an explicit handler: {event}"
        );
    }
}

#[test]
fn official_responses_stream_events_have_explicit_classifications() {
    let official_event_types = [
        "response.audio.delta",
        "response.audio.done",
        "response.audio.transcript.delta",
        "response.audio.transcript.done",
        "response.code_interpreter_call_code.delta",
        "response.code_interpreter_call_code.done",
        "response.code_interpreter_call.completed",
        "response.code_interpreter_call.in_progress",
        "response.code_interpreter_call.interpreting",
        "response.completed",
        "response.content_part.added",
        "response.content_part.done",
        "response.created",
        "error",
        "response.file_search_call.completed",
        "response.file_search_call.in_progress",
        "response.file_search_call.searching",
        "response.function_call_arguments.delta",
        "response.function_call_arguments.done",
        "response.in_progress",
        "response.failed",
        "response.incomplete",
        "response.output_item.added",
        "response.output_item.done",
        "response.reasoning_summary_part.added",
        "response.reasoning_summary_part.done",
        "response.reasoning_summary_text.delta",
        "response.reasoning_summary_text.done",
        "response.reasoning_text.delta",
        "response.reasoning_text.done",
        "response.refusal.delta",
        "response.refusal.done",
        "response.output_text.delta",
        "response.output_text.done",
        "response.web_search_call.completed",
        "response.web_search_call.in_progress",
        "response.web_search_call.searching",
        "response.image_generation_call.completed",
        "response.image_generation_call.generating",
        "response.image_generation_call.in_progress",
        "response.image_generation_call.partial_image",
        "response.mcp_call_arguments.delta",
        "response.mcp_call_arguments.done",
        "response.mcp_call.completed",
        "response.mcp_call.failed",
        "response.mcp_call.in_progress",
        "response.mcp_list_tools.completed",
        "response.mcp_list_tools.failed",
        "response.mcp_list_tools.in_progress",
        "response.output_text.annotation.added",
        "response.queued",
        "response.custom_tool_call_input.delta",
        "response.custom_tool_call_input.done",
    ];

    assert_eq!(official_event_types.len(), 53);
    for event_type in official_event_types {
        let mut state = ResponsesStreamState::default();
        let event = serde_json::json!({ "type": event_type }).to_string();
        let _ = OpenAiCompatibleProvider::process_sse_event(&event, &mut state, None);
        assert!(
            !state.unrecognized_event_observed,
            "official event lacks an explicit classification: {event_type}"
        );
    }
}

#[test]
fn responses_output_started_requires_usage_quantities() {
    let mut state = ResponsesStreamState {
        provider_usage: Some(serde_json::json!({})),
        ..ResponsesStreamState::default()
    };
    assert!(!state.output_started());

    state.provider_usage = Some(serde_json::Value::Null);
    assert!(!state.output_started());

    state.provider_usage = Some(serde_json::json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0
    }));
    assert!(state.output_started());
}

#[tokio::test]
async fn responses_handle_retries_allocation_only_stream_failure() {
    let first = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_empty\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_empty\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_empty\",\"summary\":[]}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_empty\",\"call_id\":\"call_empty\",\"name\":\"\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":2,\"item_id\":\"fc_empty\",\"arguments\":\"\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_empty\",\"call_id\":\"call_empty\",\"name\":\"\",\"arguments\":\"\"}}\n\n"
    );
    let transport = two_responses_streams(first);
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

    let result = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await;

    assert_eq!(transport.calls(), 2);
    let response = result.expect("allocation-only partial is safe to discard and retry");
    assert_eq!(response.full_text, "second generation");
}

async fn assert_streamed_output_stops_retry(first: &'static str) {
    let transport = two_responses_streams(first);
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
        .expect_err("observed stream output must stop the retry ladder");

    assert_eq!(transport.calls(), 1, "output must not be re-bought");
    assert_eq!(
        failure.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
}

#[tokio::test]
async fn responses_handle_does_not_retry_unknown_stream_event() {
    assert_streamed_output_stops_retry(
        "data: {\"type\":\"response.future_output.delta\",\"delta\":\"paid\"}\n\n",
    )
    .await;
}

#[tokio::test]
async fn responses_handle_does_not_retry_refusal_delta_only_stream() {
    assert_streamed_output_stops_retry(
        "data: {\"type\":\"response.refusal.delta\",\"output_index\":0,\"item_id\":\"msg_refusal\",\"content_index\":0,\"delta\":\"I cannot help with that\"}\n\n",
    )
    .await;
}

#[tokio::test]
async fn responses_handle_does_not_retry_reasoning_text_only_stream() {
    assert_streamed_output_stops_retry(
        "data: {\"type\":\"response.reasoning_text.delta\",\"output_index\":0,\"item_id\":\"rs_paid\",\"content_index\":0,\"delta\":\"generated reasoning\"}\n\n",
    )
    .await;
}

#[tokio::test]
async fn responses_handle_does_not_retry_output_text_done_only_stream() {
    assert_streamed_output_stops_retry(
        "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_done\",\"content_index\":0,\"text\":\"paid final text\"}\n\n",
    )
    .await;
}

#[tokio::test]
async fn responses_handle_does_not_retry_name_only_function_call() {
    assert_streamed_output_stops_retry(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_named\",\"call_id\":\"call_named\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\n",
    )
    .await;
}

#[tokio::test]
async fn responses_handle_retries_canonical_empty_failed_response() {
    let first = "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_failed\",\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"The model failed to generate a response.\"},\"output\":[],\"usage\":null}}\n\n";
    let transport = two_responses_streams(first);
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

    let response = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("empty failed response is safe to discard and retry");

    assert_eq!(transport.calls(), 2);
    assert_eq!(response.full_text, "second generation");
}
