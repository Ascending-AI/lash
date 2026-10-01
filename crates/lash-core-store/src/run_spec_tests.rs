use super::*;
use crate::{ModelConfig, RecordedModel};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::provider::{ModelUnavailable, ModelUnavailableReason, RuntimeModels};

/// A catalog the resolver reads through `snapshot` only: it mints each listed
/// key with a 200k window and counts the mints.
struct Catalog {
    keys: &'static [&'static str],
    snapshots: AtomicUsize,
}

impl Catalog {
    fn serving(keys: &'static [&'static str]) -> Self {
        Self {
            keys,
            snapshots: AtomicUsize::new(0),
        }
    }

    fn snapshots(&self) -> usize {
        self.snapshots.load(Ordering::SeqCst)
    }
}

impl RuntimeModels for Catalog {
    fn snapshot(&self, key: &ModelKey) -> Result<RecordedModel, ModelUnavailable> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        if self.keys.contains(&key.as_str()) {
            Ok(recorded(key.as_str()))
        } else {
            Err(ModelUnavailable::new(
                key.clone(),
                ModelUnavailableReason::UnknownKey,
            ))
        }
    }

    fn bind(
        &self,
        recorded: &RecordedModel,
    ) -> Result<crate::provider::ProviderHandle, ModelUnavailable> {
        panic!(
            "resolving a spec never binds a transport: {}",
            recorded.key()
        )
    }
}

/// `key`'s binding: a 200k window and the `low`/`high` efforts, except the
/// `plain-model` key, whose capability has no reasoning controls.
fn recorded(key: &str) -> RecordedModel {
    let metadata = lash_core_llm::model::ModelMetadata::new(
        format!("{key}-wire"),
        std::num::NonZeroUsize::new(200_000).expect("non-zero window"),
    );
    let metadata = if key == "plain-model" {
        metadata
    } else {
        metadata.with_capability(crate::provider::ModelCapability {
            reasoning: Some(crate::provider::ReasoningCapability {
                efforts: vec!["low".to_string(), "high".to_string()],
                encoding: crate::provider::ReasoningEncoding::Effort,
                disable: false,
                mandatory: false,
            }),
            ..crate::provider::ModelCapability::default()
        })
    };
    RecordedModel::mint(ModelKey::new(key), metadata)
}

fn catalog() -> Catalog {
    Catalog::serving(&[
        "session-model",
        "root-model",
        "definition-model",
        "plain-model",
    ])
}

fn snapshot() -> PersistedSessionConfig {
    let mut config = PersistedSessionConfig::new(crate::TurnBudget::Unbounded);
    config.model = Some(
        ModelConfig::new(recorded("session-model"))
            .with_reasoning(ReasoningSelection::Effort("low".to_string())),
    );
    config.plugin_config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
    config.plugin_config.insert(
        "protocol",
        serde_json::json!({ "keep": 1, "replace": "session" }),
    );
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
    let resolved = spec
        .resolve(&snapshot(), None, TerminationPolicy::default(), &catalog())
        .expect("resolve");
    assert_eq!(
        resolved,
        ResolvedRun::snapshot(snapshot(), TerminationPolicy::default())
    );
    assert_eq!(resolved.base.config_revision, 7);
}

/// FIG-4389: a root records the termination policy it resolved under, the
/// record round-trips it, and a record without it does not decode: no
/// worker's live policy fills the gap.
#[test]
fn a_resolved_run_records_its_termination_policy() {
    let finishes = TerminationPolicy {
        treat_missing_done_as_failure: false,
    };
    let resolved = RunSpec::default()
        .resolve(&snapshot(), None, finishes.clone(), &catalog())
        .expect("resolve");
    assert_eq!(resolved.termination, finishes);

    let mut encoded = serde_json::to_value(&resolved).expect("encode run");
    assert_eq!(
        encoded["termination"],
        serde_json::json!({ "treat_missing_done_as_failure": false })
    );
    let decoded: ResolvedRun = serde_json::from_value(encoded.clone()).expect("decode run");
    assert_eq!(decoded.termination, finishes);

    encoded
        .as_object_mut()
        .expect("a run encodes as an object")
        .remove("termination");
    assert!(
        serde_json::from_value::<ResolvedRun>(encoded).is_err(),
        "a run record without its termination policy is refused"
    );
    assert!(
        serde_json::from_value::<TerminationPolicy>(serde_json::json!({})).is_err(),
        "a termination policy states its fallback explicitly"
    );
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
    let mut resolved = ResolvedRun::snapshot(snapshot(), TerminationPolicy::default());
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
fn a_spec_hash_is_canonical_over_option_key_order() {
    let forward = serde_json::json!({ "a": 1, "b": 2 });
    let backward: serde_json::Value = serde_json::from_str(r#"{ "b": 2, "a": 1 }"#).expect("json");
    let spec = |options| {
        RunSpec::overrides(RunOverrides {
            protocol_turn_options: Some(ProtocolTurnOptions::from_payload(options)),
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
        "key insertion order must not name a different spec"
    );
    assert!(hash.as_str().starts_with("run-spec:v1:blake3:"));
    assert_eq!(
        spec(forward.clone()).canonical_json().expect("json"),
        spec(forward).canonical_json().expect("json")
    );
    let other = RunSpec::overrides(RunOverrides {
        model: Some(ModelKey::new("other")),
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
            model: Some(ModelKey::new("route")),
            reasoning: Some(ReasoningSelection::Effort("high".to_string())),
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
    let resolved = spec
        .resolve(&snapshot(), None, TerminationPolicy::default(), &catalog())
        .expect("resolve");
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
        "key insertion order must not name a different spec"
    );
}

#[test]
fn a_model_only_override_mints_the_key_once_and_keeps_the_snapshot_reasoning() {
    let spec = RunSpec::overrides(RunOverrides {
        model: Some(ModelKey::new("root-model")),
        ..RunOverrides::default()
    });
    let catalog = catalog();
    let resolved = spec
        .resolve(&snapshot(), None, TerminationPolicy::default(), &catalog)
        .expect("resolve");
    assert_eq!(catalog.snapshots(), 1, "the key is minted exactly once");
    let model = resolved.config().model.clone().expect("model");
    assert_eq!(model.model, recorded("root-model"));
    assert_eq!(
        model.reasoning,
        ReasoningSelection::Effort("low".to_string()),
        "a key alone keeps the snapshot's reasoning"
    );
    assert_eq!(resolved.spec, spec.hash().expect("hash"));
    assert_eq!(resolved.base.config_revision, 7);
}

#[test]
fn a_reasoning_only_override_keeps_the_snapshot_model_without_minting() {
    let spec = RunSpec::overrides(RunOverrides {
        reasoning: Some(ReasoningSelection::Effort("high".to_string())),
        ..RunOverrides::default()
    });
    let catalog = catalog();
    let resolved = spec
        .resolve(&snapshot(), None, TerminationPolicy::default(), &catalog)
        .expect("resolve");
    assert_eq!(catalog.snapshots(), 0, "no key, no mint");
    let model = resolved.config().model.clone().expect("model");
    assert_eq!(model.model, recorded("session-model"));
    assert_eq!(
        model.reasoning,
        ReasoningSelection::Effort("high".to_string())
    );
}

#[test]
fn an_override_naming_an_unserved_key_fails_typed_and_never_falls_back() {
    let spec = RunSpec::overrides(RunOverrides {
        model: Some(ModelKey::new("retired-model")),
        ..RunOverrides::default()
    });
    match spec.resolve(&snapshot(), None, TerminationPolicy::default(), &catalog()) {
        Err(RunResolveError::Model(unavailable)) => {
            assert_eq!(unavailable.key, ModelKey::new("retired-model"));
            assert_eq!(unavailable.reason, ModelUnavailableReason::UnknownKey);
        }
        other => panic!("an unserved key must fail typed, got {other:?}"),
    }
}

/// FIG-4531: a per-run override is judged when the root's shape resolves.
/// A key whose capability has no reasoning controls cannot take the
/// session's recorded effort, and an effort the session's model does not
/// advertise cannot be overridden onto it: both are the typed refusal, never
/// a recorded shape that fails its first turn.
#[test]
fn an_override_whose_reasoning_the_model_refuses_is_refused_typed() {
    let onto_plain = RunSpec::overrides(RunOverrides {
        model: Some(ModelKey::new("plain-model")),
        ..RunOverrides::default()
    });
    match onto_plain.resolve(&snapshot(), None, TerminationPolicy::default(), &catalog()) {
        Err(RunResolveError::Reasoning(refused)) => {
            assert_eq!(refused.key, ModelKey::new("plain-model"));
            assert_eq!(
                refused.reasoning,
                ReasoningSelection::Effort("low".to_string())
            );
        }
        other => panic!("an inherited effort the key cannot take is refused, got {other:?}"),
    }

    let unadvertised = RunSpec::overrides(RunOverrides {
        reasoning: Some(ReasoningSelection::Effort("extreme".to_string())),
        ..RunOverrides::default()
    });
    match unadvertised.resolve(&snapshot(), None, TerminationPolicy::default(), &catalog()) {
        Err(RunResolveError::Reasoning(refused)) => {
            assert_eq!(refused.key, ModelKey::new("session-model"));
        }
        other => panic!("an unadvertised effort is refused, got {other:?}"),
    }

    // The same key with a selection it accepts resolves.
    let accepted = RunSpec::overrides(RunOverrides {
        model: Some(ModelKey::new("plain-model")),
        reasoning: Some(ReasoningSelection::ProviderDefault),
        ..RunOverrides::default()
    });
    accepted
        .resolve(&snapshot(), None, TerminationPolicy::default(), &catalog())
        .expect("the provider's default reasoning fits a model with no controls");
}

#[test]
fn a_reasoning_override_for_a_session_without_a_model_is_refused() {
    let spec = RunSpec::overrides(RunOverrides {
        reasoning: Some(ReasoningSelection::Effort("high".to_string())),
        ..RunOverrides::default()
    });
    let bare = PersistedSessionConfig::new(crate::TurnBudget::Unbounded);
    assert!(matches!(
        spec.resolve(&bare, None, TerminationPolicy::default(), &catalog()),
        Err(RunResolveError::ReasoningWithoutModel)
    ));
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
            ..RunOverrides::default()
        }),
        ..RunSpec::default()
    };
    let definition = RunOverrides {
        model: Some(ModelKey::new("definition-model")),
        protocol_turn_options: Some(ProtocolTurnOptions::from_payload(
            serde_json::json!({ "replace": "definition", "added": true }),
        )),
        ..RunOverrides::default()
    };
    let resolved = spec
        .resolve(
            &snapshot(),
            Some(definition),
            TerminationPolicy::default(),
            &catalog(),
        )
        .expect("resolve");
    assert_eq!(
        resolved.config().model.clone().expect("model").model,
        recorded("definition-model"),
        "the definition's key wins over the snapshot's model"
    );
    assert_eq!(
        resolved
            .config()
            .plugin_config
            .protocol_turn_options()
            .payload,
        serde_json::json!({ "keep": 1, "replace": "explicit", "added": true })
    );
}
