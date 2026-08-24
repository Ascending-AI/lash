use super::*;
use crate::responses_stream_event::{ResponsesStreamEvent, ResponsesStreamEventClass};

#[test]
fn responses_stream_event_classes_reproduce_the_old_tables() {
    use ResponsesStreamEventClass::{EvidenceOnly, Lifecycle, Structural, Terminal};

    // Frozen from the pre-FIG-1973 evidence and structural string tables. The
    // final two columns are the old evidence-handler return and structural
    // match outcomes, respectively.
    let cases = [
        ("", EvidenceOnly, true, false),
        ("response.audio.delta", EvidenceOnly, true, false),
        ("response.audio.done", EvidenceOnly, true, false),
        ("response.audio.transcript.delta", EvidenceOnly, true, false),
        ("response.audio.transcript.done", EvidenceOnly, true, false),
        (
            "response.code_interpreter_call_code.delta",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.code_interpreter_call_code.done",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.code_interpreter_call.completed",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.code_interpreter_call.in_progress",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.code_interpreter_call.interpreting",
            EvidenceOnly,
            true,
            false,
        ),
        ("response.completed", Terminal, false, true),
        ("response.content_part.added", EvidenceOnly, true, false),
        ("response.content_part.done", EvidenceOnly, true, false),
        ("response.created", Lifecycle, true, false),
        (
            "response.custom_tool_call_input.delta",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.custom_tool_call_input.done",
            EvidenceOnly,
            true,
            false,
        ),
        ("response.debug", EvidenceOnly, true, false),
        ("response.done", Terminal, false, true),
        ("response.failed", Terminal, false, true),
        (
            "response.file_search_call.completed",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.file_search_call.in_progress",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.file_search_call.searching",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.function_call_arguments.delta",
            Structural,
            false,
            true,
        ),
        (
            "response.function_call_arguments.done",
            Structural,
            false,
            true,
        ),
        (
            "response.image_generation_call.completed",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.image_generation_call.generating",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.image_generation_call.in_progress",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.image_generation_call.partial_image",
            EvidenceOnly,
            true,
            false,
        ),
        ("response.in_progress", Lifecycle, true, false),
        ("response.incomplete", Terminal, false, true),
        (
            "response.mcp_call_arguments.delta",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.mcp_call_arguments.done",
            EvidenceOnly,
            true,
            false,
        ),
        ("response.mcp_call.completed", EvidenceOnly, true, false),
        ("response.mcp_call.failed", EvidenceOnly, true, false),
        ("response.mcp_call.in_progress", EvidenceOnly, true, false),
        (
            "response.mcp_list_tools.completed",
            EvidenceOnly,
            true,
            false,
        ),
        ("response.mcp_list_tools.failed", EvidenceOnly, true, false),
        (
            "response.mcp_list_tools.in_progress",
            EvidenceOnly,
            true,
            false,
        ),
        ("response.output_item.added", Structural, false, true),
        ("response.output_item.done", Structural, false, true),
        (
            "response.output_text.annotation.added",
            EvidenceOnly,
            true,
            false,
        ),
        ("response.output_text.delta", Structural, false, true),
        ("response.output_text.done", Structural, false, true),
        ("response.queued", Lifecycle, true, false),
        (
            "response.reasoning_summary_part.added",
            Structural,
            false,
            true,
        ),
        (
            "response.reasoning_summary_part.done",
            Structural,
            false,
            true,
        ),
        (
            "response.reasoning_summary_text.delta",
            Structural,
            false,
            true,
        ),
        (
            "response.reasoning_summary_text.done",
            Structural,
            false,
            true,
        ),
        ("response.reasoning_text.delta", EvidenceOnly, true, false),
        ("response.reasoning_text.done", EvidenceOnly, true, false),
        ("response.refusal.delta", EvidenceOnly, true, false),
        ("response.refusal.done", EvidenceOnly, true, false),
        (
            "response.web_search_call.completed",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.web_search_call.in_progress",
            EvidenceOnly,
            true,
            false,
        ),
        (
            "response.web_search_call.searching",
            EvidenceOnly,
            true,
            false,
        ),
    ];

    assert_eq!(cases.len(), 55);
    for (name, expected_class, old_evidence_handled, old_structural_handled) in cases {
        let event = ResponsesStreamEvent::parse(name);
        let class = event.handling_class();
        assert_ne!(event, ResponsesStreamEvent::Unknown, "known name: {name}");
        assert_eq!(class, expected_class, "wrong class for {name}");
        assert_eq!(event.is_terminal(), expected_class == Terminal, "{name}");
        assert_eq!(
            matches!(class, EvidenceOnly | Lifecycle),
            old_evidence_handled,
            "old evidence-handler outcome changed for {name}"
        );
        assert_eq!(
            matches!(class, Structural | Terminal),
            old_structural_handled,
            "old structural-handler outcome changed for {name}"
        );
    }
}

#[test]
fn unknown_responses_stream_event_stays_unknown_and_fails_safe() {
    let name = "response.completed.future";
    let event = ResponsesStreamEvent::parse(name);
    assert_eq!(event, ResponsesStreamEvent::Unknown);
    assert_eq!(event.handling_class(), ResponsesStreamEventClass::Unknown);
    assert!(!event.is_terminal());

    let mut state = ResponsesStreamState::default();
    OpenAiCompatibleProvider::process_sse_event(
        &serde_json::json!({"type": name}).to_string(),
        &mut state,
        None,
    )
    .unwrap();
    assert!(state.unrecognized_event_observed);
    assert!(state.output_started());
}

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
    fn response_with_text(status: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "resp_schema",
            "object": "response",
            "created_at": 1,
            "status": status,
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "model": "provider/model",
            "tools": [],
            "output": [{
                "id": "msg_schema",
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "generated output",
                    "annotations": [],
                    "logprobs": []
                }],
                "status": "completed"
            }],
            "parallel_tool_calls": false,
            "metadata": {},
            "tool_choice": "auto",
            "temperature": 1,
            "top_p": 1
        })
    }

    let cases = [
        (
            serde_json::json!({"type":"response.audio.delta","delta":"audio","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.audio.done","sequence_number":1,"response_id":"resp_schema"}),
            true,
        ),
        (
            serde_json::json!({"type":"response.audio.transcript.delta","response_id":"resp_schema","delta":"transcript","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.audio.transcript.done","response_id":"resp_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.code_interpreter_call_code.delta","output_index":0,"item_id":"ci_schema","delta":"print(1)","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.code_interpreter_call_code.done","output_index":0,"item_id":"ci_schema","code":"print(1)","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.code_interpreter_call.completed","output_index":0,"item_id":"ci_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.code_interpreter_call.in_progress","output_index":0,"item_id":"ci_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.code_interpreter_call.interpreting","output_index":0,"item_id":"ci_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.completed","response":response_with_text("completed"),"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.content_part.added","item_id":"msg_schema","output_index":0,"content_index":0,"part":{"type":"output_text","text":"generated output","annotations":[],"logprobs":[]},"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.content_part.done","item_id":"msg_schema","output_index":0,"content_index":0,"part":{"type":"output_text","text":"generated output","annotations":[],"logprobs":[]},"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.created","response":response_with_text("in_progress"),"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"error","code":"server_error","message":"failed","param":null,"sequence_number":1}),
            false,
        ),
        (
            serde_json::json!({"type":"response.file_search_call.completed","output_index":0,"item_id":"fs_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.file_search_call.in_progress","output_index":0,"item_id":"fs_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.file_search_call.searching","output_index":0,"item_id":"fs_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc_schema","output_index":0,"delta":"{\"city\":\"Berlin\"}","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.function_call_arguments.done","item_id":"fc_schema","name":"weather","output_index":0,"arguments":"{\"city\":\"Berlin\"}","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.in_progress","response":response_with_text("in_progress"),"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.failed","response":response_with_text("failed"),"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.incomplete","response":response_with_text("incomplete"),"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_schema","type":"message","role":"assistant","content":[{"type":"output_text","text":"generated output","annotations":[],"logprobs":[]}],"status":"in_progress"},"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_schema","type":"message","role":"assistant","content":[{"type":"output_text","text":"generated output","annotations":[],"logprobs":[]}],"status":"completed"},"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.reasoning_summary_part.added","item_id":"rs_schema","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"generated reasoning"},"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.reasoning_summary_part.done","item_id":"rs_schema","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"generated reasoning"},"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_schema","output_index":0,"summary_index":0,"delta":"generated reasoning","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.reasoning_summary_text.done","item_id":"rs_schema","output_index":0,"summary_index":0,"text":"generated reasoning","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.reasoning_text.delta","item_id":"rs_schema","output_index":0,"content_index":0,"delta":"generated reasoning","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.reasoning_text.done","item_id":"rs_schema","output_index":0,"content_index":0,"text":"generated reasoning","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.refusal.delta","item_id":"msg_schema","output_index":0,"content_index":0,"delta":"refused","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.refusal.done","item_id":"msg_schema","output_index":0,"content_index":0,"refusal":"refused","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.output_text.delta","item_id":"msg_schema","output_index":0,"content_index":0,"delta":"generated output","sequence_number":1,"logprobs":[]}),
            true,
        ),
        (
            serde_json::json!({"type":"response.output_text.done","item_id":"msg_schema","output_index":0,"content_index":0,"text":"generated output","sequence_number":1,"logprobs":[]}),
            true,
        ),
        (
            serde_json::json!({"type":"response.web_search_call.completed","output_index":0,"item_id":"ws_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.web_search_call.in_progress","output_index":0,"item_id":"ws_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.web_search_call.searching","output_index":0,"item_id":"ws_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.image_generation_call.completed","output_index":0,"item_id":"ig_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.image_generation_call.generating","output_index":0,"item_id":"ig_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.image_generation_call.in_progress","output_index":0,"item_id":"ig_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.image_generation_call.partial_image","output_index":0,"item_id":"ig_schema","sequence_number":1,"partial_image_index":0,"partial_image_b64":"aW1hZ2U="}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_call_arguments.delta","output_index":0,"item_id":"mcp_schema","delta":"{\"query\":\"docs\"}","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_call_arguments.done","output_index":0,"item_id":"mcp_schema","arguments":"{\"query\":\"docs\"}","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_call.completed","item_id":"mcp_schema","output_index":0,"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_call.failed","item_id":"mcp_schema","output_index":0,"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_call.in_progress","output_index":0,"item_id":"mcp_schema","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_list_tools.completed","item_id":"mcpl_schema","output_index":0,"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_list_tools.failed","item_id":"mcpl_schema","output_index":0,"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.mcp_list_tools.in_progress","item_id":"mcpl_schema","output_index":0,"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.output_text.annotation.added","item_id":"msg_schema","output_index":0,"content_index":0,"annotation_index":0,"annotation":{},"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.queued","response":response_with_text("queued"),"sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.custom_tool_call_input.delta","output_index":0,"item_id":"ctc_schema","delta":"generated input","sequence_number":1}),
            true,
        ),
        (
            serde_json::json!({"type":"response.custom_tool_call_input.done","output_index":0,"item_id":"ctc_schema","input":"generated input","sequence_number":1}),
            true,
        ),
        (serde_json::json!({}), false),
        (
            serde_json::json!({"future_generated_content":"paid output"}),
            true,
        ),
        (
            serde_json::json!({"type":"response.debug","debug":{"echo_upstream_body":{"model":"provider/model","messages":[{"role":"user","content":"hello"}],"stream":true,"max_tokens":1024,"temperature":1}},"sequence_number":0}),
            false,
        ),
    ];

    assert_eq!(cases.len(), 56);
    for (event, expected_output_started) in cases {
        let mut state = ResponsesStreamState::default();
        let event = event.to_string();
        let _ = OpenAiCompatibleProvider::process_sse_event(&event, &mut state, None);
        assert_eq!(
            state.output_started(),
            expected_output_started,
            "wrong output-evidence verdict for {event}"
        );
        assert!(
            !state.unrecognized_event_observed,
            "documented event lacks an explicit classification: {event}"
        );
    }
}

#[test]
fn empty_reasoning_summary_parts_are_not_output_evidence() {
    for event in [
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rs_empty","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""},"sequence_number":1}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rs_empty","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""},"sequence_number":1}"#,
    ] {
        let mut state = ResponsesStreamState::default();
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, None).unwrap();
        assert!(!state.output_started(), "empty part was evidence: {event}");
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
async fn responses_handle_does_not_retry_untyped_future_generated_content() {
    assert_streamed_output_stops_retry("data: {\"future_generated_content\":\"paid output\"}\n\n")
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
async fn responses_handle_does_not_retry_reasoning_summary_part_added_only_stream() {
    assert_streamed_output_stops_retry(
        "data: {\"type\":\"response.reasoning_summary_part.added\",\"item_id\":\"rs_paid\",\"output_index\":0,\"summary_index\":0,\"part\":{\"type\":\"summary_text\",\"text\":\"generated reasoning\"},\"sequence_number\":1}\n\n",
    )
    .await;
}

#[tokio::test]
async fn responses_handle_does_not_retry_reasoning_summary_part_done_only_stream() {
    assert_streamed_output_stops_retry(
        "data: {\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs_paid\",\"output_index\":0,\"summary_index\":0,\"part\":{\"type\":\"summary_text\",\"text\":\"generated reasoning\"},\"sequence_number\":1}\n\n",
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

#[tokio::test]
async fn responses_handle_retries_after_ping_and_response_debug() {
    let first = concat!(
        "event: ping\n",
        "data: {}\n\n",
        "data: {\"type\":\"response.debug\",\"debug\":{\"echo_upstream_body\":{\"model\":\"provider/model\",\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}],\"stream\":true,\"max_tokens\":1024,\"temperature\":1}},\"sequence_number\":0}\n\n"
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

    let response = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("benign metadata before failure must preserve recovery");

    assert_eq!(transport.calls(), 2);
    assert_eq!(response.full_text, "second generation");
}
