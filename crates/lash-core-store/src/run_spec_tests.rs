use super::*;
use crate::{PromptContribution, PromptSlot};

fn snapshot() -> PersistedSessionConfig {
    let mut config = PersistedSessionConfig::new(crate::TurnBudget::Unbounded);
    config.provider_id = "session-provider".to_string();
    config.model = ModelSpec::new(
        "session-model",
        std::num::NonZeroUsize::new(200_000).expect("non-zero window"),
    );
    config.prompt = Some(
        PromptLayer::new().with_contribution(PromptContribution::guidance("Session", "session")),
    );
    config.protocol_turn_options = Some(ProtocolTurnOptions::from_payload(
        serde_json::json!({ "keep": 1, "replace": "session" }),
    ));
    config.config_revision = 7;
    config
}

#[test]
fn the_default_spec_is_no_spec_and_resolves_to_the_snapshot() {
    let spec = RunSpec::default();
    assert!(spec.is_default());
    assert_eq!(spec.hash().expect("hash"), None);
    assert_eq!(
        serde_json::to_value(&spec).expect("encode"),
        serde_json::json!({})
    );
    let resolved = spec.resolve(&snapshot(), None).expect("resolve");
    assert_eq!(resolved, ResolvedRun::snapshot(snapshot()));
    assert_eq!(resolved.base.config_revision, 7);
}

#[test]
fn a_spec_hash_is_canonical_over_prompt_slot_order() {
    let mut forward = PromptLayer::new();
    forward.add_contribution(PromptContribution::guidance("A", "a"));
    forward.clear_slot(PromptSlot::Environment);
    let mut backward = PromptLayer::new();
    backward.clear_slot(PromptSlot::Environment);
    backward.add_contribution(PromptContribution::guidance("A", "a"));
    let spec = |prompt| {
        RunSpec::overrides(RunOverrides {
            prompt: Some(prompt),
            ..RunOverrides::default()
        })
    };
    let hash = spec(forward.clone())
        .hash()
        .expect("hash")
        .expect("non-default");
    assert_eq!(
        Some(hash.clone()),
        spec(backward).hash().expect("hash"),
        "slot insertion order must not name a different spec"
    );
    assert!(hash.as_str().starts_with("run-spec:v1:blake3:"));
    assert_eq!(
        spec(forward.clone()).canonical_json().expect("json"),
        spec(forward).canonical_json().expect("json")
    );
    let other = RunSpec::overrides(RunOverrides {
        provider_id: Some("other".to_string()),
        ..RunOverrides::default()
    });
    assert_ne!(Some(hash), other.hash().expect("hash"));
}

#[test]
fn a_canonical_spec_decodes_back_to_itself() {
    let spec = RunSpec {
        definition: Some(DefinitionRef::new("review", 3)),
        context: serde_json::json!({ "repo": "lash" }),
        overrides: Box::new(RunOverrides {
            provider_id: Some("route".to_string()),
            ..RunOverrides::default()
        }),
    };
    let decoded =
        RunSpec::from_canonical_json(&spec.canonical_json().expect("json")).expect("decode");
    assert_eq!(decoded, spec);
}

#[test]
fn a_provider_only_override_keeps_the_snapshot_model_and_variant() {
    let spec = RunSpec::overrides(RunOverrides {
        provider_id: Some("root-provider".to_string()),
        ..RunOverrides::default()
    });
    let resolved = spec.resolve(&snapshot(), None).expect("resolve");
    assert_eq!(resolved.config().provider_id, "root-provider");
    assert_eq!(resolved.config().model, snapshot().model);
    assert_eq!(resolved.spec, spec.hash().expect("hash"));
    assert_eq!(resolved.base.config_revision, 7);
}

#[test]
fn explicit_overrides_win_over_the_definition_which_wins_over_the_snapshot() {
    let spec = RunSpec {
        definition: Some(DefinitionRef::new("review", 1)),
        context: serde_json::Value::Null,
        overrides: Box::new(RunOverrides {
            protocol_turn_options: Some(ProtocolTurnOptions::from_payload(
                serde_json::json!({ "replace": "explicit" }),
            )),
            prompt: Some(
                PromptLayer::new()
                    .with_contribution(PromptContribution::guidance("Explicit", "explicit")),
            ),
            ..RunOverrides::default()
        }),
    };
    let definition = RunOverrides {
        provider_id: Some("definition-provider".to_string()),
        protocol_turn_options: Some(ProtocolTurnOptions::from_payload(
            serde_json::json!({ "replace": "definition", "added": true }),
        )),
        prompt: Some(
            PromptLayer::new()
                .with_contribution(PromptContribution::guidance("Definition", "definition")),
        ),
        ..RunOverrides::default()
    };
    let resolved = spec
        .resolve(&snapshot(), Some(definition))
        .expect("resolve");
    assert_eq!(resolved.config().provider_id, "definition-provider");
    assert_eq!(
        resolved
            .config()
            .protocol_turn_options
            .clone()
            .expect("options")
            .payload,
        serde_json::json!({ "keep": 1, "replace": "explicit", "added": true })
    );
    let prompt = lash_sansio::session_model::prompt::resolve_prompt_layers([resolved
        .config()
        .prompt
        .as_ref()
        .expect("prompt")]);
    let bodies = prompt
        .contributions
        .iter()
        .map(|contribution| contribution.content.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(bodies, ["session", "definition", "explicit"]);
}

#[test]
fn a_reset_slot_in_an_override_replaces_the_snapshot_slot() {
    let spec = RunSpec::overrides(RunOverrides {
        prompt: Some(PromptLayer::new().with_replaced_slot(
            PromptSlot::Guidance,
            [PromptContribution::guidance("Root", "root only")],
        )),
        ..RunOverrides::default()
    });
    let resolved = spec.resolve(&snapshot(), None).expect("resolve");
    let prompt = lash_sansio::session_model::prompt::resolve_prompt_layers([resolved
        .config()
        .prompt
        .as_ref()
        .expect("prompt")]);
    let bodies = prompt
        .contributions
        .iter()
        .map(|contribution| contribution.content.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(bodies, ["root only"]);
}
