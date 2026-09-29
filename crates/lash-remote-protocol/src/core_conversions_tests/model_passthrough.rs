use super::*;

#[test]
fn remote_model_intent_and_process_model_spec_round_trip_reasoning_selections() {
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

        let spec = RemoteProcessModelSpec {
            id: "remote-model".to_string(),
            extra_body: intent.extra_body.clone(),
            variant: selection.clone(),
            capability: RemoteModelCapability::default(),
            limits: RemoteProcessModelLimits::default(),
        };
        let spec_json = serde_json::to_value(&spec).expect("serialize process model spec");
        let spec_round_trip: RemoteProcessModelSpec =
            serde_json::from_value(spec_json).expect("deserialize process model spec");
        assert_eq!(spec_round_trip.variant, selection);
        assert_eq!(spec_round_trip.extra_body, spec.extra_body);
    }
}
