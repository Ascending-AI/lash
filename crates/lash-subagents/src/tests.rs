// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_sansio::SessionId;
use std::collections::BTreeMap;

use crate::rlm_support::{
    SpawnCreateRequestInput, build_session_request, build_spawn_create_request,
};
use lash_core::SessionPolicy;
use lash_core::runtime::RuntimeSessionState;
use serde_json::json;

/// `model`'s binding as the host catalog mints it, run with `variant`.
fn llm_profile_spec(
    model: &str,
    variant: Option<String>,
    context_window_tokens: usize,
) -> Option<lash_core::LlmProfileConfig> {
    let config = lash_core::testing::test_llm_profile_config(
        model,
        lash_core::LlmProfileMetadata::builder(model)
            .context_window_tokens(context_window_tokens)
            .build()
            .expect("valid model spec"),
    );
    Some(match variant {
        Some(effort) => config.with_reasoning(lash_core::ReasoningSelection::Effort(effort)),
        None => config,
    })
}

#[test]
fn static_capability_policy_fields_distinguish_inherit_set_and_clear() {
    let current = SessionPolicy {
        model: llm_profile_spec("parent-model", Some("parent-variant".to_string()), 200_000),
        generation: lash_core::GenerationOptions {
            seed: Some(77),
            ..Default::default()
        },
        ..SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let spec = SessionSpec::inherit().model("child-model");
    let registry = CapabilityRegistry::new().with(Arc::new(StaticCapability::new("child", spec)));

    let request = build_session_request(&registry, &current, "child").expect("request");
    assert_eq!(
        request.model,
        Some(lash_core::LlmProfileKey::new("child-model")),
        "the capability's key rides the request, minted when the child is created"
    );
    let policy = request.policy.expect("policy");
    assert_eq!(
        policy.model, current.model,
        "until creation mints the key the child carries the parent's recorded model"
    );
    assert_eq!(
        policy.generation, current.generation,
        "a child inherits the parent's sampling intent instead of falling back to provider defaults"
    );

    let pinned = SessionSpec::inherit().generation(lash_core::GenerationOptions {
        seed: Some(5),
        ..Default::default()
    });
    let registry = CapabilityRegistry::new().with(Arc::new(StaticCapability::new("child", pinned)));
    let policy = build_session_request(&registry, &current, "child")
        .expect("request")
        .policy
        .expect("policy");
    assert_eq!(policy.generation.seed, Some(5));
}

/// `state`'s snapshot, recording the RLM protocol as its parent's protocol.
fn rlm_parent_snapshot(state: &RuntimeSessionState) -> lash_core::SessionSnapshot {
    let mut snapshot = state.to_snapshot();
    snapshot.plugin_config = lash_core::PluginConfig::for_protocol(Some(
        lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
    ));
    snapshot
}

#[test]
fn spawn_schema_is_strict_and_nameless() {
    let registry = default_registry(&BTreeMap::new());
    let tool = rlm::spawn_agent_tool_definition(&registry.names());
    let schema = tool.contract.input_schema.canonical;
    let schema = schema.as_value();
    let retired_key = ["agent", "_", "name"].concat();

    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("spawn schema properties");
    assert!(
        properties
            .get("output")
            .and_then(|value| value.get("description"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|description| description.contains("list[str]")),
        "output schema description should document list field descriptors"
    );
    assert!(
        !properties.contains_key(&retired_key),
        "retired model-authored identity field leaked into spawn schema"
    );
    assert_eq!(
        schema.get("additionalProperties"),
        Some(&serde_json::Value::Bool(false))
    );

    let compiled = jsonschema::validator_for(schema).expect("spawn schema compiles");
    assert!(
        compiled
            .validate(&json!({ "task": "inspect routing", "capability": "explore" }))
            .is_ok()
    );
    let mut rejected = serde_json::Map::new();
    rejected.insert(
        "task".to_string(),
        serde_json::Value::String("inspect routing".to_string()),
    );
    rejected.insert(
        "capability".to_string(),
        serde_json::Value::String("explore".to_string()),
    );
    rejected.insert(
        retired_key,
        serde_json::Value::String("retired".to_string()),
    );
    assert!(
        compiled
            .validate(&serde_json::Value::Object(rejected))
            .is_err(),
        "strict spawn schema must reject retired identity arguments"
    );
}

#[tokio::test]
async fn spawn_uses_live_parent_provider_when_selecting_subagent_model() {
    // Two distinct stub providers so we can verify that spawn
    // resolves against the *live* policy, not the factory's stale
    // one. The final child policy inherits the live policy's explicit
    // model spec.
    let stale_policy = SessionPolicy {
        model: llm_profile_spec("stale-parent", None, 200_000),
        ..SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let live_policy = SessionPolicy {
        model: llm_profile_spec("live-parent", None, 1234),
        ..SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let registry = Arc::new(default_registry(&BTreeMap::new()));
    let current_snapshot = RuntimeSessionState {
        policy: live_policy.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    let tool_access = lash_core::SessionToolAccess::default();

    let request = build_spawn_create_request(SpawnCreateRequestInput {
        fleet_format: lash_core::FleetFormat::current(),
        registry: &registry,
        parent_session_id: &SessionId::from("root"),
        current_snapshot: current_snapshot.to_snapshot(),
        session_spec: &SessionSpec::inherit(),
        tool_access: &tool_access,
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        capability_name: "explore",
        output_schema: None,
        seed: Default::default(),
        parent_subagent: None,
        caused_by: None,
    })
    .expect("spawn request");
    let child_policy = request.policy.expect("child policy");

    // The capability looked up the live policy's provider, not
    // the stale one. This pins the behaviour where the spawn
    // pipeline always resolves models against the *current* session
    // policy snapshot, even when the factory was built earlier.
    let stale_choice = build_session_request(&registry, &stale_policy, "explore")
        .expect("stale request")
        .policy
        .expect("stale policy")
        .model;
    assert_eq!(
        child_policy.context_window_tokens(),
        live_policy.context_window_tokens()
    );
    assert_ne!(child_policy.model, stale_choice);
    assert_eq!(child_policy.model, live_policy.model);
    assert_eq!(
        request.model, None,
        "an explore tier without a key copies the recorded model"
    );
    assert!(request.tool_access.restricted_tools().is_none());
    assert!(
        !request
            .plugin_options
            .plugins
            .contains_key(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID),
        "a child of a parent that records no RLM protocol states no RLM namespace (FIG-4396)"
    );

    let structured_request = build_spawn_create_request(SpawnCreateRequestInput {
        fleet_format: lash_core::FleetFormat::current(),
        registry: &registry,
        parent_session_id: &SessionId::from("root"),
        current_snapshot: rlm_parent_snapshot(&current_snapshot),
        session_spec: &SessionSpec::inherit(),
        tool_access: &tool_access,
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        capability_name: "explore",
        output_schema: Some(
            lash_sansio::JsonSchema::admit(json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"]
            }))
            .expect("valid child output schema"),
        ),
        seed: Default::default(),
        parent_subagent: None,
        caused_by: None,
    })
    .expect("structured spawn request");
    let structured_policy = structured_request
        .policy
        .as_ref()
        .expect("structured child policy");
    assert_eq!(structured_policy.model, live_policy.model);
    let extras = structured_request
        .plugin_options
        .decode::<lash_rlm_types::RlmCreateExtras>(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID)
        .expect("decode rlm extras")
        .expect("rlm extras");
    assert!(matches!(
        extras.termination,
        Some(lash_rlm_types::RlmTermination::FinishRequired { .. })
    ));
    assert!(matches!(
        extras.final_answer_format,
        Some(lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue)
    ));
    assert!(structured_request.tool_access.restricted_tools().is_none());
}

/// FIG-1480: `agents.spawn` authored a `Shape = Type { name: str, ... }`
/// example that the dialect's line rewriter could only dress as
/// `const Shape = Type {...}`, which is not TypeScript. The walker pins a
/// copied corpus and the prose guard excludes examples, so pin this crate's
/// real examples at the rendered surface: respelled through the dialect, then
/// parsed.
#[test]
fn spawn_agent_examples_render_as_parseable_typescript() {
    let definition = spawn_agent_tool_definition(&[]);
    let examples = &definition.contract().examples;

    let rendered = examples
        .iter()
        .map(|example| {
            lash_protocol_rlm::Dialect::render_tool_example(
                &lash_protocol_rlm::TypescriptDialect,
                example,
            )
            .expect("TypeScript spells every authored example")
        })
        .collect::<Vec<_>>();
    // Parsed rather than linked: examples name host modules and free
    // identifiers no isolated environment has, so `UnknownBinding` is expected
    // and a *syntax* error is not.
    let mut unparseable = Vec::new();
    for (example, rendered) in examples.iter().zip(&rendered) {
        if let Err(error) = lash_typescript::parse(rendered) {
            let code = format!("{:?}", error.code);
            if code.contains("UnknownBinding") || code.contains("LinkError") {
                continue;
            }
            unparseable.push(format!("`{example}` -> `{rendered}`: {error}"));
        }
    }
    assert!(unparseable.is_empty(), "{unparseable:#?}");

    assert!(
        rendered.iter().any(|example| example
            .contains(r#"const Shape = { name: "str", tags: "list[str]", status: "str" };"#)),
        "{rendered:#?}"
    );
    for example in &rendered {
        for retired in ["Type {", "enum["] {
            assert!(!example.contains(retired), "{example}");
        }
    }
}
