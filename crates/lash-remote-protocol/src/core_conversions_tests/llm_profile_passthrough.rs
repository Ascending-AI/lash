use super::*;

#[test]
fn remote_model_and_process_llm_profile_config_round_trip_reasoning_selections() {
    for selection in [
        RemoteReasoningSelection::ProviderDefault,
        RemoteReasoningSelection::Disabled,
        RemoteReasoningSelection::Effort("high".to_string()),
    ] {
        let config = RemoteModelConfig {
            key: "remote-key".to_string(),
            metadata: RemoteLlmProfileMetadata {
                wire_model: "remote-model".to_string(),
                extra_body: serde_json::Map::from_iter([(
                    "route".into(),
                    serde_json::json!({"value": 42}),
                )]),
                request_defaults: Default::default(),
                capability: RemoteLlmProfileCapability::default(),
                limits: RemoteProcessModelLimits {
                    context_window_tokens: 4096,
                    output_tokens: lash_sansio::llm_profile::OutputTokenLimits::new(
                        Some(2048),
                        Some(1024),
                    )
                    .expect("valid output limits"),
                },
            },
            reasoning: selection.clone(),
        };
        let config_json = serde_json::to_value(&config).expect("serialize process model config");
        let config_round_trip: RemoteModelConfig =
            serde_json::from_value(config_json).expect("deserialize process model config");
        assert_eq!(config_round_trip.reasoning, selection);
        assert_eq!(
            config_round_trip.metadata.extra_body,
            config.metadata.extra_body
        );
        let core =
            lash_core::LlmProfileConfig::try_from(config.clone()).expect("core model config");
        assert_eq!(core.key().as_str(), "remote-key");
        assert_eq!(core.model.wire_model(), "remote-model");
        assert_eq!(RemoteModelConfig::from(core), config);
    }
}

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
