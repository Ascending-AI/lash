use super::super::model::ProcessExecutionEnvSpec;

fn process_execution_env_identity_corpus() -> [(String, String); 2] {
    let mut plugin_config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
    plugin_config.insert("a:b", serde_json::json!({"enabled": true}));
    let plugin_config = crate::AdmittedPluginConfig::new(plugin_config, 3);
    let policy = crate::SessionPolicy {
        charge_safety: Default::default(),
        model: Some(
            crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
                crate::LlmProfileKey::new("rich-key"),
                crate::LlmProfileMetadata::builder("model:rich")
                    .context_window_tokens(8192)
                    .output_token_capacity(2048)
                    .build()
                    .expect("valid rich model limits")
                    .with_capability(crate::LlmProfileCapability {
                        instruction_role: crate::InstructionRole::Developer,
                        native_mid_conversation_system: true,
                        google_dialect: Default::default(),
                        reasoning: Some(crate::ReasoningCapability {
                            efforts: vec!["low".to_string(), "high".to_string()],
                            encoding: crate::ReasoningEncoding::Budget(
                                std::collections::BTreeMap::from([
                                    ("low".to_string(), 256),
                                    ("high".to_string(), 1024),
                                ]),
                            ),
                            disable: true,
                            mandatory: true,
                        }),
                        cache_control: Some(crate::CacheControlDialect::Anthropic),
                        stream_termination: Some(crate::StreamTermination::EofTolerated),
                        sampling: crate::SamplingCapability::Pinned,
                        reasoning_retention: Default::default(),
                    }),
            ))
            .with_reasoning(crate::ReasoningSelection::Effort("high".to_string())),
        ),
        attachment_acceptance: Default::default(),
        autonomous: true,
        turn_budget: crate::TurnBudget::bounded(1),
        max_tool_calls: crate::MaxToolCalls::new(1024),
        no_progress_budget: Default::default(),
        generation: crate::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(1024),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.25).expect("finite temperature")),
            seed: Some(-7),
            stop_sequences: Vec::new(),
            parallel_tool_calls: None,
            projection_provenance: Default::default(),
        },
    };
    let specs = [
        ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        ),
        ProcessExecutionEnvSpec::new(plugin_config, policy),
    ];
    specs.map(|spec| {
        let bytes = spec.to_store_bytes().expect("encode golden env");
        (
            String::from_utf8(bytes).expect("env bytes are JSON"),
            spec.stable_ref()
                .expect("derive golden env ref")
                .to_string(),
        )
    })
}

#[test]
fn process_execution_env_identity_golden_corpus() {
    let expected: [(String, String); 2] =
        serde_json::from_str(include_str!("fixtures/process_execution_env_identity.json"))
            .expect("generated identity corpus");
    assert_eq!(process_execution_env_identity_corpus(), expected);
}
