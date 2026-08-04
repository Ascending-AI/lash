use super::*;

#[tokio::test]
async fn direct_llm_completion_crosses_controller_and_records_usage_and_trace() {
    let recorder = RecordingEffectController::default().with_replay_by_key();
    let trace_path = unique_trace_path("direct-llm-completion");
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "raw direct answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "raw direct answer".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 4,
                output_tokens: 6,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host = EmbeddedRuntimeHost::new({
        let mut config = runtime_host_config_with_inline_controller(Arc::new(recorder.clone()));
        config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(
            trace_path.clone(),
        )));
        config
    });
    let runtime =
        runtime_with_plugins_and_tools_and_host(Vec::new(), Arc::new(EmptyTools), transport, host)
            .await;

    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(recorder.clone())),
        None,
    );
    let request = LlmRequest {
        model: "mock-model".to_string(),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Text {
                text: Arc::from("raw prompt"),
                response_meta: None,
                cache_breakpoint: false,
            }],
        )],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::None,
        model_variant: Default::default(),
        model_capability: crate::ModelCapability::default(),
        scope: crate::LlmRequestScope::new(
            "direct-llm-test",
            "direct-llm-test:frame",
            "direct-llm-test:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: crate::GenerationOptions::default(),
        provider_trace: None,
    };
    let mut reused_request_id = request.clone();
    reused_request_id.messages = vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Text {
            text: Arc::from("a deliberately different prompt"),
            response_meta: None,
            cache_breakpoint: false,
        }],
    )];
    let mut missing_request_id = request.clone();
    missing_request_id.scope.request_id = "  ".to_string();
    let error = direct
        .direct_llm_completion(missing_request_id, "direct-llm-test")
        .await
        .expect_err("empty request id must be rejected before effect execution");
    assert!(error.to_string().contains("request_id must be non-empty"));
    let completion = direct
        .direct_llm_completion(request, "direct-llm-test")
        .await
        .expect("direct llm completion");
    let replayed = direct
        .direct_llm_completion(reused_request_id, "direct-llm-test")
        .await
        .expect("request-id reuse replays the first direct completion");

    assert_eq!(completion.response.full_text, "raw direct answer");
    assert_eq!(replayed.response.full_text, completion.response.full_text);
    assert_eq!(completion.usage.output_tokens, 6);
    assert_eq!(completion.llm_call.call_id.0, "direct-effect-test");
    assert_eq!(
        recorder.count_kind(RuntimeEffectKind::Direct),
        1,
        "the same request id is the same durable effect even when request content differs"
    );
    let ledger = runtime.shared_token_ledger.lock().expect("token ledger");
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].source, "direct-llm-test");
    assert_eq!(ledger[0].model, "mock-model");
    assert_eq!(
        ledger[0].usage.input_tokens, 8,
        "replaying the durable effect still accounts usage for each caller observation"
    );
}
