use super::*;

pub(super) fn request(messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder(
                    "claude-sonnet-4-6".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(Default::default())
                .extra_body(Default::default())
                .request_defaults(Default::default())
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages,

        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        attachment_acceptance: crate::attachment_test_acceptance(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: None,
        // Anthropic requires a cap, and lash invents none.
        generation: lash_core::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(16_384),
            ..lash_core::GenerationOptions::default()
        },
        provider_trace: None,
    }
}
