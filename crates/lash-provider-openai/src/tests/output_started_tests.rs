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
