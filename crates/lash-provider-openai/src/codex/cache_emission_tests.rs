use super::*;
use lash_core::llm::types::LlmContentBlock;

#[test]
fn codex_tool_schema_prompt_cache_key_is_not_cache_emission() {
    let provider = CodexProvider::new("access", "refresh", 0).with_options(ProviderOptions {
        cache_retention: CacheRetention::None,
        ..ProviderOptions::default()
    });
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
        description: "Host tool with a provider-looking property".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "prompt_cache_key": { "type": "string" } }
        })
        .into(),
        output_schema: json!({}).into(),
    }]);

    let (body, cache_control_emitted) = provider
        .build_request_body_with_cache_evidence(&req, false)
        .unwrap();

    assert!(body["tools"][0]["parameters"]["properties"]["prompt_cache_key"].is_object());
    assert!(!cache_control_emitted);
    assert_eq!(
        CodexProvider::generation_disposition(&req, cache_control_emitted).cache,
        lash_core::GenerationOptionOutcome::OmittedUnsupported
    );

    let enabled = CodexProvider::new("access", "refresh", 0);
    let (_, cache_control_emitted) = enabled
        .build_request_body_with_cache_evidence(&req, false)
        .unwrap();
    assert!(cache_control_emitted);
    assert_eq!(
        CodexProvider::generation_disposition(&req, cache_control_emitted).cache,
        lash_core::GenerationOptionOutcome::Applied
    );
}
