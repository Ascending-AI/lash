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
        .complete(
            request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
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
        .complete(
            streamed_request(Arc::new(std::sync::Mutex::new(Vec::new()))),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
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
        .complete(
            request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
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
        .complete(
            streamed_request(Arc::new(std::sync::Mutex::new(Vec::new()))),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect("streamed Responses normal completion may carry no content");
    assert_valid_empty_completion(&response, "completed", 12);
    assert_eq!(responses_streamed_transport.calls(), 1);
}

#[tokio::test]
async fn eof_tolerance_does_not_turn_empty_unterminated_streams_into_success() {
    for streamed in [false, true] {
        for (endpoint, body, code) in [
            (
                CompletionEndpoint::ChatCompletions,
                "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3}}\n\n",
                TurnFailureCode::StreamEndedBeforeFinishReason,
            ),
            (
                CompletionEndpoint::Responses,
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-empty-eof\",\"status\":\"in_progress\",\"output\":[],\"usage\":{\"input_tokens\":4}}}\n\n",
                TurnFailureCode::StreamEndedBeforeTerminalResponse,
            ),
        ] {
            let transport = Arc::new(ScriptedHttpTransport {
                responses: std::sync::Mutex::new(VecDeque::from([(
                    200,
                    vec![(
                        "content-type".into(),
                        if streamed {
                            "text/event-stream"
                        } else {
                            "application/json"
                        }
                        .into(),
                    )],
                    body,
                )])),
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut provider: Box<dyn Provider> = match endpoint {
                CompletionEndpoint::ChatCompletions => Box::new(
                    OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL)
                        .with_compat(OpenAiCompat::openrouter())
                        .with_transport(transport.clone()),
                ),
                CompletionEndpoint::Responses => {
                    Box::new(OpenAiProvider::new("key").with_transport(transport.clone()))
                }
            };
            let mut req = streamed_request(Arc::new(std::sync::Mutex::new(Vec::new())));
            req.model.metadata_mut().capability.stream_termination =
                Some(StreamTermination::EofTolerated);
            let error = provider
                .complete(
                    req,
                    &lash_core::provider::NoSlotDeliveries,
                    &lash_core::provider::LiveCallHorizon::fixture(),
                )
                .await
                .expect_err("empty EOF without terminal evidence must be retryable truncation");
            assert_eq!(
                error.code,
                Some(code.into()),
                "{endpoint:?} streamed={streamed}"
            );
            assert_eq!(error.kind, ProviderFailureKind::Stream);
            assert_eq!(
                error.retry_verdict,
                TransportRetryVerdict::RetryableTransient
            );
            // Usage remains charge evidence even without generated content.
            assert_eq!(
                error.output_started,
                endpoint == CompletionEndpoint::Responses
            );
            let partial = error
                .partial_response
                .expect("empty truncation keeps its evidence");
            assert!(partial.parts.is_empty());
            assert_eq!(partial.terminal_reason, LlmTerminalReason::Unknown);
            assert_eq!(
                partial.usage.input_tokens,
                if endpoint == CompletionEndpoint::ChatCompletions {
                    3
                } else {
                    4
                }
            );
            assert_eq!(transport.calls(), 1);
        }
    }
}

async fn assert_empty_responses_stream_is_rejected(body: &'static str, description: &str) {
    let transport = single_stream_transport(body);
    let mut provider = OpenAiProvider::new("key").with_transport(Arc::clone(&transport) as _);
    let error = provider
        .complete(
            streamed_request(Arc::new(std::sync::Mutex::new(Vec::new()))),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err(description);
    assert_eq!(
        error.code.as_ref().map(|code| code.to_string()),
        Some("lash:empty_response".to_string())
    );
    assert_eq!(transport.calls(), 1);
}

#[tokio::test]
async fn empty_responses_require_completed_status_in_terminal_payload() {
    const BARE_COMPLETED: &str = "data: {\"type\":\"response.completed\"}\n\n";
    const BARE_INCOMPLETE: &str = "data: {\"type\":\"response.incomplete\"}\n\n";
    const MISSING_STATUS: &str =
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
    const IN_PROGRESS: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"in_progress\",\"output\":[]}}\n\n";
    const UNKNOWN: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"unknown\",\"output\":[]}}\n\n";
    const INCOMPLETE_EVENT_WITH_COMPLETED_STATUS: &str = "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";

    for (body, description) in [
        (
            BARE_COMPLETED,
            "bare response.completed must not prove success",
        ),
        (
            BARE_INCOMPLETE,
            "bare response.incomplete must not default to successful stop",
        ),
        (
            MISSING_STATUS,
            "response.completed without status must not prove success",
        ),
        (
            IN_PROGRESS,
            "response.completed with in_progress must not prove success",
        ),
        (
            UNKNOWN,
            "response.completed with unknown status must not prove success",
        ),
        (
            INCOMPLETE_EVENT_WITH_COMPLETED_STATUS,
            "response.incomplete cannot carry a completed success status",
        ),
    ] {
        assert_empty_responses_stream_is_rejected(body, description).await;
    }
}

#[tokio::test]
async fn empty_buffered_responses_require_completed_status() {
    for (body, description) in [
        (
            r#"{"id":"resp-missing-status","output":[]}"#,
            "buffered response without status must not prove success",
        ),
        (
            r#"{"id":"resp-in-progress","status":"in_progress","output":[]}"#,
            "buffered in_progress response must not prove success",
        ),
        (
            r#"{"id":"resp-unknown","status":"unknown","output":[]}"#,
            "buffered unknown response must not prove success",
        ),
    ] {
        let transport = Arc::new(RecordingHttpTransport::responding_with(Vec::new(), body));
        let mut provider = OpenAiProvider::new("key").with_transport(transport.clone());
        let error = provider
            .complete(
                request(vec![LlmMessage::text(LlmRole::User, "hello")]),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err(description);
        assert_eq!(
            error.code.as_ref().map(|code| code.to_string()),
            Some("lash:empty_response".to_string())
        );
        assert_eq!(transport.requests.lock_recover().len(), 1);
    }
}

#[tokio::test]
async fn empty_chat_requires_wire_stop_even_when_native_evidence_exists() {
    const CHAT_NATIVE_LENGTH_ONLY: &str = r#"{"id":"chat-native-only","model":"provider/model","choices":[{"message":{"role":"assistant","content":""},"native_finish_reason":"length"}]}"#;
    let value: Value = serde_json::from_str(CHAT_NATIVE_LENGTH_ONLY).expect("valid fixture");
    let mut state = ChatStreamState::default();
    state
        .capture_response_value(&value)
        .expect("native evidence is valid");
    assert_eq!(
        state
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.provider_finish_reason.as_deref()),
        Some("length")
    );
    assert!(!state.normal_stop_seen);

    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        CHAT_NATIVE_LENGTH_ONLY,
    ));
    let mut provider = openrouter_provider().with_transport(transport.clone());
    let error = provider
        .complete(
            request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("native evidence cannot replace a missing wire finish_reason");
    assert_eq!(
        error.code.as_ref().map(|code| code.to_string()),
        Some("lash:empty_response".to_string())
    );
    assert_eq!(transport.requests.lock_recover().len(), 1);
}
