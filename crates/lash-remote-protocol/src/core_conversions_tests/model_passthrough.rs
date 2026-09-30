use super::*;

#[test]
fn remote_model_intent_and_process_model_config_round_trip_reasoning_selections() {
    for selection in [
        RemoteReasoningSelection::ProviderDefault,
        RemoteReasoningSelection::Disabled,
        RemoteReasoningSelection::Effort("high".to_string()),
    ] {
        let intent = RemoteModelIntent {
            model: "remote-model".to_string(),
            extra_body: serde_json::Map::from_iter([(
                "route".into(),
                serde_json::json!({"value":42}),
            )]),
            request_defaults: Default::default(),
            variant: selection.clone(),
            capability: RemoteModelCapability::default(),
            provider: None,
            metadata: HashMap::new(),
        };
        let intent_json = serde_json::to_value(&intent).expect("serialize model intent");
        let intent_round_trip: RemoteModelIntent =
            serde_json::from_value(intent_json).expect("deserialize model intent");
        assert_eq!(intent_round_trip.variant, selection);
        assert_eq!(intent_round_trip.extra_body, intent.extra_body);

        let config = RemoteModelConfig {
            key: "remote-key".to_string(),
            metadata: RemoteModelMetadata {
                wire_model: "remote-model".to_string(),
                extra_body: intent.extra_body.clone(),
                request_defaults: Default::default(),
                capability: RemoteModelCapability::default(),
                limits: RemoteProcessModelLimits {
                    context_window_tokens: 4096,
                    output_token_capacity: None,
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
        let core = lash_core::ModelConfig::try_from(config.clone()).expect("core model config");
        assert_eq!(core.key().as_str(), "remote-key");
        assert_eq!(core.model.wire_model(), "remote-model");
        assert_eq!(RemoteModelConfig::from(core), config);
    }
}
