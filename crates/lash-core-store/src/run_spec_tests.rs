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
fn recorded_render_survives_run_and_detached_environment_round_trip() {
    use crate::session_state::facade_ops::RuntimeSessionStateFacadeOps;

    let record = RecordedRender {
        renderer_id: "lash.ax.v1".to_owned(),
        params: serde_json::json!({
            "print": lash_render::RenderParams::default(),
            "preview": lash_render::RenderParams::preview(),
        }),
    };
    let mut resolved = ResolvedRun::snapshot(snapshot());
    resolved.render = Some(record.clone());
    let encoded = serde_json::to_vec(&resolved).expect("encode run");
    let decoded: ResolvedRun = serde_json::from_slice(&encoded).expect("decode run");
    assert_eq!(decoded.render, Some(record.clone()));

    let mut state =
        crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    crate::session_state::adopt_resolved_run(&mut state, &decoded);
    let env = state.process_execution_env_spec(&state.policy);
    assert_eq!(env.render, Some(record.clone()));
    let env_bytes = env.to_store_bytes().expect("encode env");
    let restored =
        crate::ProcessExecutionEnvSpec::from_store_bytes(&env_bytes).expect("decode env");
    assert_eq!(restored.render, Some(record));
}

#[test]
fn unfinished_rendering_requires_the_exact_recorded_renderer() {
    let record = RecordedRender {
        renderer_id: "lash.ax.v1".into(),
        params: serde_json::json!({"print": {"max_chars": 8000}}),
    };
    assert_eq!(
        RecordedRender::require_available(Some(&record), "lash.ax.v1"),
        Ok(&record)
    );
    assert_eq!(
        RecordedRender::require_available(Some(&record), "lash.ax.v2"),
        Err(crate::RuntimeErrorCode::RecordedRendererUnavailable)
    );
    assert_eq!(
        RecordedRender::require_available(None, "lash.ax.v1"),
        Err(crate::RuntimeErrorCode::RecordedRendererUnavailable)
    );
    assert!(crate::RuntimeErrorCode::RecordedRendererUnavailable.is_retryable());
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
        capabilities: [(
            SlotId::new("browser"),
            CapabilityRef {
                contract: ContractRef::new("browser", 2),
                binding: BindingId::new("browser:main"),
                args: serde_json::json!({ "headless": true }),
            },
        )]
        .into_iter()
        .collect(),
    };
    let decoded =
        RunSpec::from_canonical_json(&spec.canonical_json().expect("json")).expect("decode");
    assert_eq!(decoded, spec);
}

#[test]
fn capabilities_are_durable_refs_recorded_on_the_resolution() {
    let mut spec = RunSpec::default();
    assert_eq!(
        serde_json::to_value(&spec).expect("encode"),
        serde_json::json!({}),
        "an empty capability map serializes away with the default spec"
    );
    spec.capabilities.insert(
        SlotId::new("search"),
        CapabilityRef {
            contract: ContractRef::new("search", 1),
            binding: BindingId::new("search:team"),
            args: serde_json::Value::Null,
        },
    );
    assert!(
        !spec.is_default(),
        "a capability alone is a non-default spec"
    );
    assert!(spec.hash().expect("hash").is_some());
    let resolved = spec.resolve(&snapshot(), None).expect("resolve");
    assert_eq!(resolved.capabilities, spec.capabilities);
    assert_eq!(
        resolved.config(),
        &snapshot(),
        "capabilities alone leave the root's config unchanged"
    );
    // The spec's capability refs order canonically, so slot insertion order
    // cannot name a different spec.
    let mut reordered = RunSpec::default();
    reordered.capabilities.insert(
        SlotId::new("zzz"),
        CapabilityRef {
            contract: ContractRef::new("search", 1),
            binding: BindingId::new("search:team"),
            args: serde_json::Value::Null,
        },
    );
    reordered.capabilities.insert(
        SlotId::new("search"),
        spec.capabilities[&SlotId::new("search")].clone(),
    );
    spec.capabilities.insert(
        SlotId::new("zzz"),
        reordered.capabilities[&SlotId::new("zzz")].clone(),
    );
    assert_eq!(
        spec.hash().expect("hash"),
        reordered.hash().expect("hash"),
        "slot insertion order must not name a different spec"
    );
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
        ..RunSpec::default()
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
