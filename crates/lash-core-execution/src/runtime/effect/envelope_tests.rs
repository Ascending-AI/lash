//! Wire tests of the effect envelope's recorded outcomes.

use super::*;

/// FIG-3600 S6 (D3 Q2): a run's recorded config round-trips whole, so a
/// replay adopts every field its first execution ran under.
#[test]
fn a_recorded_turn_config_round_trips_whole() {
    let mut config = crate::PersistedSessionConfig::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    );
    config.model = Some(crate::LlmProfileConfig::new(
        crate::RecordedLlmProfile::mint(
            crate::LlmProfileKey::from("recorded-model"),
            crate::LlmProfileMetadata::builder("recorded-model")
                .context_window_tokens(32_000)
                .build()
                .expect("a literal model spec builds"),
        ),
    ));
    config.plugin_config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
    config
        .plugin_config
        .insert("protocol", serde_json::json!({ "dialect": "recorded" }));
    config.config_revision = 3;
    let resolved =
        crate::ResolvedRun::snapshot(config, crate::runtime::TerminationPolicy::default(), 3);
    let recorded = RuntimeEffectOutcome::ResolveTurnConfig {
        resolved: Box::new(resolved.clone()),
    };
    let wire = serde_json::to_value(&recorded).expect("encode the recorded config");
    let decoded: RuntimeEffectOutcome =
        serde_json::from_value(wire).expect("decode the recorded config");
    assert_eq!(
        decoded
            .into_resolve_turn_config()
            .expect("a turn-config outcome"),
        resolved
    );
}
