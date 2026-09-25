//! Wire tests of the effect envelope's recorded outcomes.

use super::*;

/// FIG-3600 S6 (D3 Q2): a root's recorded config round-trips whole, so a
/// replay adopts every field its first execution ran under.
#[test]
fn a_recorded_turn_config_round_trips_whole() {
    let mut config = crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded);
    config.provider_id = "stub".to_string();
    config.model = crate::ModelSpec::builder("recorded-model")
        .context_window_tokens(32_000)
        .build()
        .expect("a literal model spec builds");
    config.prompt = Some(crate::PromptLayer::default());
    config.protocol_turn_options = Some(crate::ProtocolTurnOptions::default());
    config.config_revision = 3;
    let recorded = RuntimeEffectOutcome::ResolveTurnConfig {
        config: Box::new(config.clone()),
    };
    let wire = serde_json::to_value(&recorded).expect("encode the recorded config");
    let decoded: RuntimeEffectOutcome =
        serde_json::from_value(wire).expect("decode the recorded config");
    assert_eq!(
        decoded
            .into_resolve_turn_config()
            .expect("a turn-config outcome"),
        config
    );
}
