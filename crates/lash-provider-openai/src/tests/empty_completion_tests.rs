use super::*;

fn assert_valid_empty_completion(
    response: &LlmResponse,
    expected_finish_reason: &str,
    expected_input_tokens: i64,
) {
    assert!(
        response.parts.is_empty(),
        "empty completion gained output parts"
    );
    assert_eq!(response.full_text(), "");
    assert_eq!(response.terminal_reason, LlmTerminalReason::Stop);
    assert_eq!(response.usage.input_tokens, expected_input_tokens);
    assert!(
        response.provider_usage.is_some(),
        "provider usage was dropped"
    );
    assert_eq!(
        response
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.provider_finish_reason.as_deref()),
        Some(expected_finish_reason),
        "normal terminal evidence was dropped"
    );
}

#[tokio::test]
async fn valid_empty_terminal_completions_succeed_across_chat_and_responses() {
    const CHAT_BUFFERED: &str = r#"{"id":"chat-empty-buffered","model":"provider/model","choices":[{"message":{"role":"assistant","content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":0}}"#;
    let chat_buffered_transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        CHAT_BUFFERED,
    ));
    let mut chat_buffered = openrouter_provider().with_transport(chat_buffered_transport.clone());
    let response = chat_buffered
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("buffered Chat normal stop may carry no content");
    assert_valid_empty_completion(&response, "stop", 9);
    assert_eq!(chat_buffered_transport.requests.lock_recover().len(), 1);

    const CHAT_STREAMED: &str = concat!(
        "data: {\"id\":\"chat-empty-streamed\",\"model\":\"provider/model\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":0}}\n\n",
        "data: [DONE]\n\n"
    );
    let chat_streamed_transport = single_stream_transport(CHAT_STREAMED);
    let mut chat_streamed =
        openrouter_provider().with_transport(Arc::clone(&chat_streamed_transport) as _);
    let response = chat_streamed
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("streamed Chat normal stop may carry no content");
    assert_valid_empty_completion(&response, "stop", 10);
    assert_eq!(chat_streamed_transport.calls(), 1);

    const RESPONSES_BUFFERED: &str = r#"{"id":"resp-empty-buffered","model":"provider/model","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":0,"total_tokens":11}}"#;
    let responses_buffered_transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        RESPONSES_BUFFERED,
    ));
    let mut responses_buffered =
        OpenAiProvider::new("key").with_transport(responses_buffered_transport.clone());
    let response = responses_buffered
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("buffered Responses normal completion may carry no content");
    assert_valid_empty_completion(&response, "completed", 11);
    assert_eq!(
        responses_buffered_transport.requests.lock_recover().len(),
        1
    );

    const RESPONSES_STREAMED: &str = concat!(
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-empty-streamed\",\"model\":\"provider/model\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":0,\"total_tokens\":12}}}\n\n",
        "data: [DONE]\n\n"
    );
    let responses_streamed_transport = single_stream_transport(RESPONSES_STREAMED);
    let mut responses_streamed =
        OpenAiProvider::new("key").with_transport(Arc::clone(&responses_streamed_transport) as _);
    let response = responses_streamed
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("streamed Responses normal completion may carry no content");
    assert_valid_empty_completion(&response, "completed", 12);
    assert_eq!(responses_streamed_transport.calls(), 1);
}

#[tokio::test]
async fn eof_tolerance_does_not_turn_empty_unterminated_streams_into_success() {
    let compat = OpenAiCompat {
        stream_termination: Some(StreamTermination::EofTolerated),
        ..OpenAiCompat::openrouter()
    };
    let chat_transport = single_stream_transport(
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3}}\n\n",
    );
    let mut chat = OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL)
        .with_compat(compat.clone())
        .with_transport(Arc::clone(&chat_transport) as _);
    let chat_error = chat
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect_err("empty Chat EOF without a finish reason remains malformed");
    assert_eq!(chat_error.code.as_deref(), Some("empty_response"));
    assert_eq!(chat_transport.calls(), 1);

    let responses_transport = single_stream_transport(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-empty-eof\",\"status\":\"in_progress\",\"output\":[],\"usage\":{\"input_tokens\":4}}}\n\n",
    );
    let mut responses =
        OpenAiProvider::new("key").with_transport(Arc::clone(&responses_transport) as _);
    let mut req = streamed_request(Arc::new(std::sync::Mutex::new(Vec::new())));
    req.model_capability.stream_termination = Some(StreamTermination::EofTolerated);
    let responses_error = responses
        .complete(req)
        .await
        .expect_err("empty Responses EOF without a terminal event remains malformed");
    assert_eq!(responses_error.code.as_deref(), Some("empty_response"));
    assert_eq!(responses_transport.calls(), 1);
}
