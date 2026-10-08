use super::*;

pub(super) fn request_with_capability(
    model_variant: Option<&str>,
    llm_profile_capability: LlmProfileCapability,
) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder(
                    "gemini-3.1-pro-preview".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(llm_profile_capability)
                .extra_body(Default::default())
                .request_defaults(Default::default())
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(
            model_variant
                .map(|effort| lash_core::provider::ReasoningSelection::Effort(effort.to_string()))
                .unwrap_or_default(),
        ),
        messages: vec![LlmMessage::text(LlmRole::User, "hello")],

        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        attachment_acceptance: crate::attachment_test_acceptance(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: None::<LlmEventSender>,
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    }
}

pub(super) fn request(model_variant: Option<&str>) -> LlmRequest {
    request_with_capability(model_variant, LlmProfileCapability::default())
}
