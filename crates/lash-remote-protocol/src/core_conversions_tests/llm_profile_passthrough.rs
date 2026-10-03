use super::*;

pub(super) fn request_profile() -> lash_sansio::llm_profile::LlmProfileConfig {
    lash_sansio::llm_profile::LlmProfileConfig::new(
        lash_sansio::llm_profile::RecordedLlmProfile::mint(
            lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
            lash_sansio::llm_profile::LlmProfileMetadata::builder("gpt-test".to_string())
                .context_window_tokens(128_000)
                .capability(core_llm::LlmProfileCapability {
                    instruction_role: core_llm::InstructionRole::Developer,
                    native_mid_conversation_system: true,
                    google_dialect: Default::default(),
                    reasoning: Some(core_llm::ReasoningCapability {
                        efforts: vec!["fast".to_string(), "slow".to_string()],
                        encoding: core_llm::ReasoningEncoding::Budget(
                            std::collections::BTreeMap::from([
                                ("fast".to_string(), 1024u32),
                                ("slow".to_string(), 2048u32),
                            ]),
                        ),
                        disable: true,
                        mandatory: false,
                    }),
                    cache_control: Some(core_llm::CacheControlDialect::Anthropic),
                    stream_termination: Some(core_llm::StreamTermination::RequireTerminalEvidence),
                    sampling: core_llm::SamplingCapability::Pinned,
                    reasoning_retention: Default::default(),
                })
                .extra_body(serde_json::Map::from_iter([(
                    "host_option".to_string(),
                    serde_json::json!({"enabled": true}),
                )]))
                .request_defaults(Default::default())
                .build()
                .expect("valid profile"),
        ),
    )
    .with_reasoning(core_llm::ReasoningSelection::Effort("fast".to_string()))
}
