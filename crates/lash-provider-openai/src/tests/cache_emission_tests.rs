use super::*;

#[test]
fn gemini_cache_dialect_reports_fallback_emission_when_marked_text_is_empty() {
    let mut req = request_with_instructions(
        "stable system prompt",
        vec![LlmMessage::new(
            LlmRole::User,
            vec![
                LlmContentBlock::Text {
                    text: "last stable text".into(),
                    response_meta: None,
                    cache_breakpoint: false,
                },
                LlmContentBlock::Text {
                    text: "".into(),
                    response_meta: None,
                    cache_breakpoint: true,
                },
            ],
        )],
    );
    req.model = "custom/model-v1".to_string();
    enable_cache_control(&mut req, CacheControlDialect::Gemini);

    let (body, diagnostics) = openrouter_provider()
        .build_chat_request_body_with_diagnostics(&req, true)
        .unwrap();

    assert_eq!(count_object_key(&body, "cache_control"), 1);
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert!(body["messages"][0]["content"].is_array());
    assert_eq!((diagnostics.requested, diagnostics.emitted), (1, 0));
    assert!(diagnostics.cache_control_emitted);
    assert_eq!(
        crate::common::generation_disposition(&req, &body, diagnostics.cache_control_emitted).cache,
        lash_core::GenerationOptionOutcome::Applied
    );
}

#[test]
fn tool_schema_cache_key_does_not_count_as_adapter_cache_emission() {
    use crate::common::generation_disposition;

    let provider = OpenAiCompatibleProvider::new("key", "https://provider.example/v1");
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Text {
            text: "stable history".into(),
            response_meta: None,
            cache_breakpoint: true,
        }],
    )]);
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "cache-shaped-input".to_string(),
        description: "Host tool with provider-looking property names".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "cache_control": { "type": "string" },
                "prompt_cache_key": { "type": "string" },
                "cachedContent": { "type": "string" }
            }
        })
        .into(),
        output_schema: json!({}).into(),
    }]);

    let (body, diagnostics) = provider
        .build_chat_request_body_with_diagnostics(&req, false)
        .unwrap();

    assert!(body["tools"][0]["function"]["parameters"]["properties"]["cache_control"].is_object());
    assert_eq!(
        generation_disposition(&req, &body, diagnostics.cache_control_emitted).cache,
        lash_core::GenerationOptionOutcome::OmittedUnsupported
    );

    let (body, cache_control_emitted) = provider
        .build_responses_request_body_with_cache_evidence(&req, false)
        .unwrap();
    assert!(body["tools"][0]["parameters"]["properties"]["prompt_cache_key"].is_object());
    assert_eq!(
        generation_disposition(&req, &body, cache_control_emitted).cache,
        lash_core::GenerationOptionOutcome::OmittedUnsupported
    );
}
