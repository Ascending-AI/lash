use super::*;
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::rlm_support::{
    SpawnCreateRequestInput, build_session_policy, build_spawn_create_request,
};
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, LlmRole};
use lash_core::runtime::RuntimeSessionState;
use lash_core::{
    SessionPolicy, TestLocalProcessRegistry, facade_support::LashRuntime,
    facade_support::PluginFactory, facade_support::PluginHost, facade_support::ProcessRuntimeHost,
    facade_support::RuntimeHostConfig, facade_support::TraceRuntimeSubject,
    test_support::RuntimeServices,
};
use lash_core::{ToolArgumentProjectionPolicy, ToolOutputContract, TurnInput};
use lash_lashlang_runtime::{
    LASHLANG_SURFACE_EXTENSION_ID, LashlangAbilities, LashlangHostCatalog,
    LashlangLanguageFeatures, LashlangProcessEngine, LashlangSurface, LashlangSurfaceContribution,
    TraceLashlangGraphStore,
};
use lash_protocol_rlm::RlmTurnInputExt;
use serde_json::json;

const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

fn model_spec(
    model: impl Into<String>,
    variant: Option<String>,
    context_window_tokens: usize,
) -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder(model)
        .variant(
            variant
                .map(lash_core::ReasoningSelection::Effort)
                .unwrap_or_default(),
        )
        .context_window_tokens(context_window_tokens)
        .build()
        .expect("valid model spec")
}

struct SeedProbeState {
    parent_response: String,
    captured_child_prompt: Arc<Mutex<Option<String>>>,
}

fn prompt_advertises_bound_variable(prompt: &str, name: &str) -> bool {
    prompt.contains(&format!("- `{name}`:")) || prompt.contains(&format!("- `{name}` ="))
}

fn lashlang_block(code: &str) -> String {
    format!("<lashlang>\n{code}\n</lashlang>")
}

#[test]
fn static_capability_policy_fields_distinguish_inherit_set_and_clear() {
    let current = SessionPolicy {
        model: model_spec("parent-model", Some("parent-variant".to_string()), 200_000),
        generation: lash_core::GenerationOptions {
            seed: Some(77),
            ..Default::default()
        },
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let spec = SessionSpec::inherit().model(model_spec("child-model", None, 100_000));
    let registry = CapabilityRegistry::new().with(Arc::new(StaticCapability::new("child", spec)));

    let policy = build_session_policy(&registry, &current, "child").expect("policy");

    assert_eq!(policy.model.id, "child-model");
    assert_eq!(
        policy.model.variant,
        lash_core::ReasoningSelection::ProviderDefault
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
    let policy = build_session_policy(&registry, &current, "child").expect("policy");
    assert_eq!(policy.generation.seed, Some(5));
}

struct CustomRequestCapability;

impl Capability for CustomRequestCapability {
    fn name(&self) -> &str {
        "custom"
    }

    fn build_session_request(
        &self,
        ctx: SubagentSpawnContext<'_>,
    ) -> Result<lash_core::SessionCreateRequest, String> {
        let mut tool_access = ctx.base_tool_access.clone();
        tool_access.hidden_tools.insert("custom_hidden".to_string());
        let request = lash_core::SessionCreateRequest::child(
            ctx.parent_session_id,
            lash_core::SessionStartPoint::CurrentSession,
            ctx.base_policy(),
            lash_core::PluginOptions::default(),
            "custom-subagent",
        )
        .with_plugin_source(lash_core::SessionPluginSource::CurrentHostFresh)
        .with_tool_access(tool_access);
        ctx.finalize_request(request, self.name())
    }
}

#[test]
fn capability_can_build_complete_spawn_request() {
    let registry = CapabilityRegistry::new().with(Arc::new(CustomRequestCapability));
    let current_snapshot = RuntimeSessionState {
        policy: SessionPolicy {
            model: model_spec("parent-model", None, 200_000),
            ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let mut tool_access = lash_core::SessionToolAccess::default();
    tool_access.hidden_tools.insert("base_hidden".to_string());

    let request = build_spawn_create_request(SpawnCreateRequestInput {
        registry: &registry,
        parent_session_id: "root",
        current_snapshot: current_snapshot.to_snapshot(),
        session_spec: &SessionSpec::inherit(),
        tool_access: &tool_access,
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        capability_name: "custom",
        output_schema: None,
        seed: Default::default(),
        parent_subagent: None,
        caused_by: None,
    })
    .expect("custom capability request");

    assert!(matches!(
        &request.start,
        lash_core::SessionStartPoint::CurrentSession
    ));
    assert_eq!(request.usage_source.as_deref(), Some("custom-subagent"));
    assert!(request.tool_access.hidden_tools.contains("base_hidden"));
    assert!(request.tool_access.hidden_tools.contains("custom_hidden"));
    assert_eq!(
        request.subagent.expect("subagent context").capability,
        "custom"
    );
}

#[test]
fn rlm_definitions_expose_spawn_without_mini_api() {
    let registry = default_registry(&BTreeMap::new());
    let rlm_defs = rlm::rlm_subagent_tool_definitions(&registry.names());

    assert!(rlm_defs.iter().any(|tool| tool.name() == "spawn_agent"));
    assert_eq!(
        rlm_defs.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
        vec!["spawn_agent"]
    );

    let rlm_spawn = rlm_defs
        .iter()
        .find(|tool| tool.name() == "spawn_agent")
        .expect("rlm spawn_agent");
    assert_eq!(
        rlm_spawn.contract.output_contract,
        ToolOutputContract::from_input_schema("output", None)
    );
    assert_eq!(
        rlm_spawn.manifest.argument_projection,
        ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed")
    );
    assert!(
        rlm_spawn
            .contract
            .examples
            .iter()
            .any(|example| example.contains("await agents.spawn"))
    );
    assert!(!rlm_spawn.description().contains("agents: Agents"));
    assert!(!rlm_spawn.description().contains("list[str]"));
    assert!(!rlm_spawn.description().contains("output` field"));
    assert!(
        rlm_spawn
            .contract
            .examples
            .iter()
            .any(|example| example.contains(r#"queries: "list[str]""#))
    );
    assert!(
        rlm_spawn
            .contract
            .examples
            .iter()
            .all(|example| !example.contains(r#"["str"]"#))
    );
    assert!(!rlm_spawn.description().contains("use `start spawn_agent"));
    let docs = rlm_spawn
        .contract()
        .compact_contract_with_signature_name(&rlm_spawn.manifest(), "agents.spawn")
        .render_markdown();
    assert!(docs.len() <= 3_000, "agents.spawn docs exceeded budget");
}

#[test]
fn spawn_schema_is_strict_and_nameless() {
    let registry = default_registry(&BTreeMap::new());
    let tool = rlm::spawn_agent_tool_definition(&registry.names());
    let schema = tool.contract.input_schema.canonical;
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

    let compiled = jsonschema::JSONSchema::compile(&schema).expect("spawn schema compiles");
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

#[test]
// Architecture lint: lexical guard for retired API names, not behavior proof.
fn lint_subagents_source_does_not_reintroduce_retired_lifecycle_api() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_files = [
        "src/lib.rs",
        "src/rlm.rs",
        "src/rlm_support.rs",
        "src/capability.rs",
    ];
    let banned = [
        ["agent", "_", "name"].concat(),
        ["Subagent", "Host"].concat(),
        ["Local", "Subagent", "Host"].concat(),
        ["Wait", "Agent"].concat(),
        ["Close", "Agent"].concat(),
        ["Spawn", "Agent", "Request"].concat(),
        ["Spawn", "Agent", "Response"].concat(),
        ["Agent", "Metadata"].concat(),
        ["normalize", "_async", "_subagent", "_name"].concat(),
        ["subagent:<", "agent", "_", "name", ">"].concat(),
    ];

    for relative in source_files {
        let text = std::fs::read_to_string(manifest_dir.join(relative))
            .unwrap_or_else(|err| panic!("read {relative}: {err}"));
        for needle in &banned {
            assert!(
                !text.contains(needle),
                "{relative} reintroduced retired sublashlang binding `{needle}`"
            );
        }
    }
}

#[test]
fn single_capability_spawn_can_omit_capability_field() {
    let registry = CapabilityRegistry::new().with(Arc::new(StaticCapability::new(
        "explore",
        lash_core::facade_support::SessionSpec::inherit(),
    )));
    let rlm_spawn = rlm::spawn_agent_tool_definition(&registry.names());

    assert!(
        !rlm_spawn
            .contract
            .input_schema
            .canonical
            .get("required")
            .and_then(serde_json::Value::as_array)
            .expect("required fields")
            .iter()
            .any(|field| field.as_str() == Some("capability")),
        "single-capability spawn should not require explicit capability"
    );
    assert!(
        rlm_spawn
            .contract
            .examples
            .iter()
            .all(|example| !example.contains("capability:")),
        "single-capability examples should not teach redundant capability args"
    );
}

#[tokio::test]
async fn spawn_uses_live_parent_provider_when_selecting_subagent_model() {
    // Two distinct stub providers so we can verify that spawn
    // resolves against the *live* policy, not the factory's stale
    // one. The final child policy inherits the live policy's explicit
    // model spec.
    let stale_policy = SessionPolicy {
        provider_id: "stale-stub".to_string(),
        model: model_spec("stale-parent", None, 200_000),
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let live_policy = SessionPolicy {
        provider_id: "live-stub".to_string(),
        model: model_spec("live-parent", None, 1234),
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let registry = Arc::new(default_registry(&BTreeMap::new()));
    let current_snapshot = RuntimeSessionState {
        policy: live_policy.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let tool_access = lash_core::SessionToolAccess::default();

    let request = build_spawn_create_request(SpawnCreateRequestInput {
        registry: &registry,
        parent_session_id: "root",
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
    let stale_choice = build_session_policy(&registry, &stale_policy, "explore")
        .expect("stale policy")
        .model;
    assert_eq!(child_policy.provider_id, live_policy.provider_id);
    assert_eq!(
        child_policy.model.context_window_tokens(),
        live_policy.model.context_window_tokens()
    );
    assert_ne!(child_policy.model.id, stale_choice.id);
    assert_eq!(child_policy.model.id, "live-parent");
    assert!(request.tool_access.tools.is_empty());

    let structured_request = build_spawn_create_request(SpawnCreateRequestInput {
        registry: &registry,
        parent_session_id: "root",
        current_snapshot: current_snapshot.to_snapshot(),
        session_spec: &SessionSpec::inherit(),
        tool_access: &tool_access,
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        capability_name: "explore",
        output_schema: Some(json!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"]
        })),
        seed: Default::default(),
        parent_subagent: None,
        caused_by: None,
    })
    .expect("structured spawn request");
    let structured_policy = structured_request
        .policy
        .as_ref()
        .expect("structured child policy");
    assert_eq!(structured_policy.model.id, "live-parent");
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
    assert!(structured_request.tool_access.tools.is_empty());
}

#[tokio::test]
async fn rlm_spawn_seed_is_visible_to_child_executor_and_prompt() {
    let (outcome, prompt) = run_seed_probe(
        r#"<lashlang>
result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: Type { len: int }
})?
finish result
</lashlang>"#,
        TurnInput::text("spawn a child with a seeded chunk"),
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
    assert!(
        prompt_advertises_bound_variable(&prompt, "chunk"),
        "child prompt did not advertise seeded `chunk` variable:\n{prompt}"
    );
}

/// A session tree is one dialect: a TypeScript parent's children read a
/// TypeScript prompt.
///
/// Children used to be created with no dialect, which resolves to the Lashlang
/// default, so a TypeScript parent silently spawned Lashlang children. The
/// parity battery judges a row as "typescript" while the child's prompt says
/// otherwise — evidence that disagrees with its own label. This drives a real
/// spawn and reads the prompt the child was actually given.
#[tokio::test]
async fn a_typescript_parent_spawns_typescript_children() {
    let (_outcome, prompt) = run_seed_probe_with_parent_dialect(
        r#"<lashlang>
result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: Type { len: int }
})?
finish result
</lashlang>"#,
        TurnInput::text("spawn a child from a typescript parent"),
        lash_rlm_types::RlmDialect::Typescript,
    )
    .await;

    assert!(
        prompt.contains("## TypeScript execution"),
        "the child of a TypeScript parent must read the TypeScript prompt:\n{prompt}"
    );
    assert!(
        prompt.contains("<typescript>"),
        "the child must be told to write TypeScript cells:\n{prompt}"
    );
}

/// `agents.spawn`'s own doc block, read off a prompt a real session was served.
///
/// The token that spells the nested-shape clause is resolved by the RLM doc
/// renderer, one crate away from where this schema is authored. Every other
/// assertion about it lives in that crate against a synthetic fixture, which
/// cannot show that *this* schema's token is the spelling the renderer knows: a
/// renamed token, a moved substitution, or a doc row the renderer stopped
/// resolving would each leave `{{type_literal_hint}}` sitting in a served
/// prompt. So this drives a real spawn in both dialects and reads the served
/// prompt of the session that ran in each — a TypeScript parent's child is a
/// TypeScript session, which is the reader the leak was measured on.
#[tokio::test]
async fn spawn_agent_doc_resolves_its_dialect_token_in_a_served_prompt() {
    let parent_response = r#"<lashlang>
result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: Type { len: int }
})?
finish result
</lashlang>"#;

    let (_outcome, typescript) = run_seed_probe_with_parent_dialect(
        parent_response,
        TurnInput::text("spawn a child from a typescript parent"),
        lash_rlm_types::RlmDialect::Typescript,
    )
    .await;
    assert!(
        typescript.contains("## TypeScript execution"),
        "this reader must really be a TypeScript session:\n{typescript}"
    );
    assert!(
        typescript.contains("Optional typed result shape"),
        "the served prompt must carry the spawn_agent doc:\n{typescript}"
    );
    assert!(
        !typescript.contains("Type { ... }` literal"),
        "a TypeScript session cannot write a type literal and must not be told to:\n{typescript}"
    );

    let (_outcome, lashlang) = run_seed_probe_with_parent_dialect(
        parent_response,
        TurnInput::text("spawn a child from a lashlang parent"),
        lash_rlm_types::RlmDialect::Lashlang,
    )
    .await;
    assert!(
        lashlang.contains("Optional typed result shape"),
        "the served prompt must carry the spawn_agent doc:\n{lashlang}"
    );
    assert!(
        lashlang.contains("or pass a `Type { ... }` literal for nested shapes"),
        "a Lashlang session keeps the type-literal clause:\n{lashlang}"
    );

    // The token itself never reaches a model in either dialect. Scoped to the
    // rendered tool-doc section, which is the surface the registration guard
    // governs — the dialect's own builtin docs legitimately spell `{{` as the
    // escape for a literal brace in `format`.
    for (dialect, prompt) in [("lashlang", &lashlang), ("typescript", &typescript)] {
        let docs = tool_doc_section(prompt);
        assert!(
            docs.contains("Optional typed result shape"),
            "the {dialect} tool-doc section must be the slice under test:\n{docs}"
        );
        assert!(
            !docs.contains("{{"),
            "unresolved prose token in the {dialect} tool docs:\n{docs}"
        );
    }
}

/// The rendered tool-doc section of an RLM prompt, i.e. every string the
/// registration-time prose guard is responsible for.
fn tool_doc_section(prompt: &str) -> &str {
    let start = prompt
        .find("\n### Tools\n")
        .unwrap_or_else(|| panic!("prompt has no tool-doc section:\n{prompt}"));
    let rest = &prompt[start..];
    match rest.find("\n## ") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// The other direction, so the test above cannot pass by the prompt being
/// TypeScript for everyone.
#[tokio::test]
async fn a_lashlang_parent_still_spawns_lashlang_children() {
    let (_outcome, prompt) = run_seed_probe_with_parent_dialect(
        r#"<lashlang>
result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: Type { len: int }
})?
finish result
</lashlang>"#,
        TurnInput::text("spawn a child from a lashlang parent"),
        lash_rlm_types::RlmDialect::Lashlang,
    )
    .await;

    assert!(
        !prompt.contains("## TypeScript execution"),
        "a Lashlang parent's child must not read the TypeScript prompt:\n{prompt}"
    );
}

#[tokio::test]
async fn rlm_spawn_record_shorthand_returns_child_final_value() {
    let (outcome, _) = run_seed_probe(
        r#"<lashlang>
@label(title: "Spawn subagent")
direct = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
})?
finish direct
</lashlang>"#,
        TurnInput::text("spawn a child with record shorthand output"),
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
}

#[tokio::test]
async fn rlm_spawn_process_handle_returns_child_final_value() {
    let (outcome, prompt) = run_seed_probe(
        r#"<lashlang>
process spawn_child(agents: Agents) {
  result = await agents.spawn({
    capability: "default",
    task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: Type { len: int }
  })?
  finish result
}
handle = start spawn_child(agents: agents)
result = (await handle)?
finish result
</lashlang>"#,
        TurnInput::text("spawn a child with a seeded chunk through start/await"),
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
    assert!(
        prompt_advertises_bound_variable(&prompt, "chunk"),
        "child prompt did not advertise seeded `chunk` variable:\n{prompt}"
    );
}

#[tokio::test]
async fn rlm_spawn_labeled_inside_process_returns_child_final_value() {
    let (outcome, prompt) = run_seed_probe(
        r#"<lashlang>
process spawn_child() {
  @label(title: "Spawn subagent inside process")
  result = await agents.spawn({
    capability: "default",
    task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: Type { len: int }
  })?
  finish result
}
handle = start spawn_child()
result = (await handle)?
finish result
</lashlang>"#,
        TurnInput::text("spawn a labeled child inside a Lashlang process"),
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
    assert!(
        prompt_advertises_bound_variable(&prompt, "chunk"),
        "child prompt did not advertise seeded `chunk` variable:\n{prompt}"
    );
}

#[tokio::test]
async fn rlm_spawn_captured_process_authority_returns_child_final_value() {
    let (outcome, prompt) = run_seed_probe(
        r#"<lashlang>
process spawn_child() {
  result = await agents.spawn({
    capability: "default",
    task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: Type { len: int }
  })?
  finish result
}
handle = start spawn_child()
result = (await handle)?
finish result
</lashlang>"#,
        TurnInput::text("spawn a child with captured agents authority through start/await"),
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
    assert!(
        prompt_advertises_bound_variable(&prompt, "chunk"),
        "child prompt did not advertise seeded `chunk` variable:\n{prompt}"
    );
}

#[tokio::test]
async fn rlm_spawn_links_subagent_process_from_lashlang_graph() {
    let graph_store = Arc::new(TraceLashlangGraphStore::default());
    let (outcome, _) = run_seed_probe_with_graph_store(
        r#"<lashlang>
result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: Type { len: int }
})?
finish result
</lashlang>"#,
        TurnInput::text("spawn a child and link its graph"),
        Some(Arc::clone(&graph_store)),
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
    let graphs = graph_store.graphs();
    let parent = graphs
        .iter()
        .find(|graph| {
            graph.scope.session_id == "root"
                && matches!(
                    &graph.subject,
                    TraceRuntimeSubject::Effect { kind, .. } if kind == "exec_code"
                )
                && graph
                    .children
                    .iter()
                    .any(|child| child.child_entry_name.as_deref() == Some("subagent"))
        })
        .unwrap_or_else(|| panic!("missing parent graph with subagent child link: {graphs:?}"));
    let child = parent
        .children
        .iter()
        .find(|child| child.child_entry_name.as_deref() == Some("subagent"))
        .expect("subagent child link");
    assert!(
        child
            .child_graph_key
            .starts_with("process:process:subagent:"),
        "unexpected child graph key: {}",
        child.child_graph_key
    );
}

#[tokio::test]
async fn rlm_spawn_defaults_single_capability_when_omitted() {
    let (outcome, prompt) = run_seed_probe(
        r#"<lashlang>
result = await agents.spawn({
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: Type { len: int }
})?
finish result
</lashlang>"#,
        TurnInput::text("spawn a child with the default capability"),
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
    assert!(
        prompt.contains("Subagent capability: default. Depth: 1/5."),
        "child prompt did not render subagent authority:\n{prompt}"
    );
}

#[tokio::test]
async fn rlm_spawn_seed_derived_from_projected_binding_is_visible_to_child_prompt() {
    let input = TurnInput::text("spawn a child with a chunk from projected input")
            .rlm_project(
                lash_protocol_rlm::RlmProjectedBindings::new()
                    .bind_json(
                        "input",
                        json!({
                            "context": "Header\nDate: Jan 01, 2026 || Instance: A\nDate: Jan 02, 2026 || Instance: B\n",
                        }),
                    )
                    .expect("bind input"),
            )
            .expect("project input");
    let (outcome, prompt) = run_seed_probe(
        r#"<lashlang>
ctx = to_string(input.context)
lines = split(ctx, "\n")
data = []
for line in lines {
  if starts_with(line, "Date: ") {
    data = push(data, line)
  }
}
chunk = slice(data, 0, 2)
result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: chunk },
  output: Type { len: int }
})?
finish result
</lashlang>"#,
        input,
    )
    .await;

    assert_eq!(
        outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue {
                value: json!({ "len": 2 })
            }
        )
    );
    assert!(
        prompt_advertises_bound_variable(&prompt, "chunk"),
        "child prompt did not advertise projected-derived seeded `chunk` variable:\n{prompt}"
    );
}

fn seed_probe_provider(state: Arc<SeedProbeState>) -> lash_core::testing::TestProvider {
    // The child subagent inherits the parent's live provider handle through the
    // runtime (deployment-level binding); there is no factory rematerialization,
    // so this provider needs no serializable config.
    lash_core::testing::TestProvider::builder()
        .kind("seed-probe")
        .complete(move |request| {
            let state = Arc::clone(&state);
            async move { complete_seed_probe_request(state, request).await }
        })
        .build()
}

async fn complete_seed_probe_request(
    state: Arc<SeedProbeState>,
    request: LlmRequest,
) -> Result<LlmResponse, lash_core::llm::transport::LlmTransportError> {
    let prompt = request_text(&request);
    let is_child = prompt.contains("Subagent capability: default. Depth: 1/5.")
        || prompt_advertises_bound_variable(&prompt, "chunk");
    if is_child {
        *state.captured_child_prompt.lock_recover() = Some(prompt);
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: lashlang_block("finish { len: len(chunk) }"),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..Default::default()
        })
    } else {
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: state.parent_response.clone(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..Default::default()
        })
    }
}

async fn run_seed_probe(
    parent_response: &'static str,
    input: TurnInput,
) -> (lash_core::facade_support::TurnOutcome, String) {
    run_seed_probe_with_graph_store(parent_response, input, None).await
}

/// The probe with the parent session already pinned to a dialect, so the
/// captured child prompt shows what a child of that parent actually reads.
async fn run_seed_probe_with_parent_dialect(
    parent_response: &'static str,
    input: TurnInput,
    parent_dialect: lash_rlm_types::RlmDialect,
) -> (lash_core::facade_support::TurnOutcome, String) {
    let probe = run_seed_probe_inner_dispatch(
        parent_response.to_string(),
        input,
        None,
        Some(parent_dialect),
    )
    .await;
    let child_prompt = probe.child_prompt().to_string();
    (probe.outcome, child_prompt)
}

async fn run_seed_probe_with_graph_store(
    parent_response: &'static str,
    input: TurnInput,
    graph_store: Option<Arc<TraceLashlangGraphStore>>,
) -> (lash_core::facade_support::TurnOutcome, String) {
    let probe =
        run_seed_probe_inner_dispatch(parent_response.to_string(), input, graph_store, None).await;
    let child_prompt = probe.child_prompt().to_string();
    (probe.outcome, child_prompt)
}

async fn run_seed_probe_inner_dispatch(
    parent_response: String,
    input: TurnInput,
    graph_store: Option<Arc<TraceLashlangGraphStore>>,
    parent_dialect: Option<lash_rlm_types::RlmDialect>,
) -> SeedProbe {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("subagent-seed-probe".to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(move || {
            let test = Box::pin(run_seed_probe_inner(
                parent_response,
                input,
                graph_store,
                parent_dialect,
            ));
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test);
            let _ = tx.send(result);
        })
        .expect("spawn seed-probe thread");
    rx.await.expect("seed-probe thread result")
}

async fn run_seed_probe_inner(
    parent_response: String,
    input: TurnInput,
    graph_store: Option<Arc<TraceLashlangGraphStore>>,
    parent_dialect: Option<lash_rlm_types::RlmDialect>,
) -> SeedProbe {
    let captured_child_prompt: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let state = Arc::new(SeedProbeState {
        parent_response,
        captured_child_prompt: Arc::clone(&captured_child_prompt),
    });
    let provider = seed_probe_provider(Arc::clone(&state)).into_handle();
    let execution_sink: Option<Arc<dyn lash_core::facade_support::TraceSink>> = graph_store
        .as_ref()
        .map(|store| Arc::clone(store) as Arc<dyn lash_core::facade_support::TraceSink>);
    let trace_context = lash_core::TraceContext::default();
    let language_features = LashlangLanguageFeatures::default().with_label_annotations();
    let process_env_store =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    // The RLM protocol plugin (which compiles + stores the parent turn's process
    // artifacts) and the process engine that the worker runs those artifacts
    // through must share ONE artifact store; otherwise the worker cannot load the
    // module the parent wrote. This mirrors the pre-ADR-0013 wiring where both
    // defaulted to the global in-memory store.
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());

    let factories: Vec<Arc<dyn PluginFactory>> = vec![
        Arc::new(
            lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build()
                    .with_lashlang_language_features(language_features),
                Arc::clone(&artifact_store),
            )
            .with_lashlang_execution_trace(execution_sink.clone(), trace_context.clone())
            // This harness assembles the plugin host and process engine by hand
            // (no core install step records lifecycle availability), so declare
            // it explicitly: the worker below runs real processes.
            .with_process_lifecycle(true),
        ),
        Arc::new(SubagentsPluginFactory::new(Arc::new(
            CapabilityRegistry::new().with(Arc::new(StaticCapability::new(
                "default",
                lash_core::facade_support::SessionSpec::inherit(),
            ))),
        ))),
    ];
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let host_plugins = PluginHost::new(factories.clone());
    let process_abilities = LashlangAbilities::default()
        .with_processes()
        .with_sleep()
        .with_process_signals();
    let mut extensions = host_plugins.extensions().clone();
    extensions.insert(
        lash_core::facade_support::PluginExtensionContribution::new(
            LASHLANG_SURFACE_EXTENSION_ID,
            LashlangSurfaceContribution::new(
                process_abilities,
                language_features,
                LashlangHostCatalog::new(),
            ),
        )
        .expect("lashlang surface contribution serializes"),
    );
    let process_surface = LashlangSurface::new(
        process_abilities,
        language_features,
        LashlangHostCatalog::new(),
    )
    .with_plugin_extensions(&extensions)
    .expect("process lashlang surface should merge plugin extensions");
    let process_engine = Arc::new(
        LashlangProcessEngine::new(Arc::clone(&artifact_store), process_surface)
            .with_execution_trace(execution_sink, trace_context),
    );
    let plugins = host_plugins
        .with_extensions(extensions)
        .build_session("root")
        .expect("plugin session");
    let embedded = lash_core::facade_support::EmbeddedRuntimeHost::new({
        let mut config = RuntimeHostConfig::in_memory(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        config.providers.provider_resolver = Arc::new(
            lash_core::facade_support::SingleProviderResolver::new(provider.clone()),
        );
        config = config
            .with_process_env_store(process_env_store.clone())
            .with_process_engine(process_engine.clone());
        config
    });
    let policy = SessionPolicy {
        provider_id: provider.kind().to_string(),
        model: model_spec("seed-probe-model", None, 64_000),
        turn_budget: lash_core::TurnBudget::bounded(4),
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    // `agents.spawn(...)` starts a SessionTurn (subagent) process that the
    // lease-protected worker executes — not inline. A SINGLE inline runner over
    // the same registry + an explicit in-memory store factory runs it (and
    // provider re-supply reaches the child). One runner suffices even for the
    // nested case here (`handle = start spawn_child` then `await handle`) because
    // the worker runs each process on its own task, so the parent's await never
    // parks the runner away from the child.
    let worker_registry = Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>;
    let watched = lash_core::facade_support::watch_process_registry(worker_registry);
    let worker = lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::from_plugin_factories(
            factories,
            {
                let mut config = RuntimeHostConfig::in_memory(
                    lash_core::CommitBudget::bounded(1024 * 1024, 512),
                    lash_core::QueuedWorkBatchingConfig::new(1),
                );
                config.providers.provider_resolver = Arc::new(
                    lash_core::facade_support::SingleProviderResolver::new(provider.clone()),
                );
                config = config
                    .with_process_env_store(process_env_store)
                    .with_process_engine(process_engine);
                config
            },
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched.clone()),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(policy.clone()),
    )
    .expect("valid test native substrate config");
    let process_port: Arc<dyn lash_core::ProcessWorkSubstrate> =
        Arc::new(lash_core::NativeProcessWork::new(&watched, worker));
    let host = ProcessRuntimeHost::with_ports(
        embedded,
        lash_core::ProcessWorkWiring::new(watched, process_port),
        Arc::new(lash_core::NoQueuedWork::new()),
    );
    let mut runtime = LashRuntime::from_background_state(
        policy.clone(),
        host,
        RuntimeServices::new(plugins),
        RuntimeSessionState {
            session_id: "root".to_string(),
            policy,
            protocol_turn_options: match parent_dialect {
                Some(dialect) => {
                    lash_core::ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
                        dialect: Some(dialect),
                        termination: Some(lash_rlm_types::RlmTermination::FinishRequired {
                            schema: None,
                        }),
                        final_answer_format: None,
                    })
                    .expect("encode parent dialect")
                }
                None => lash_core::ProtocolTurnOptions::default(),
            },
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        },
        lash_core::LeaseOwnerIdentity::opaque(
            "lash-subagents-test-worker",
            "lash-subagents-test-boot",
        ),
    )
    .await
    .expect("runtime");

    let scoped_effect_controller = lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
        lash_core::ExecutionScope::turn("root", "subagent-test-turn"),
    )
    .expect("test execution scope");
    let turn = Box::pin(runtime.run_turn_assembled(
        input,
        tokio_util::sync::CancellationToken::new(),
        scoped_effect_controller,
    ))
    .await
    .expect("turn");

    let prompt = captured_child_prompt.lock_recover().clone();
    SeedProbe {
        outcome: turn.outcome,
        child_prompt: prompt,
    }
}

/// What one probe run observed: the turn's outcome plus the prompt the child was
/// actually served.
struct SeedProbe {
    outcome: lash_core::facade_support::TurnOutcome,
    child_prompt: Option<String>,
}

impl SeedProbe {
    fn child_prompt(&self) -> &str {
        self.child_prompt
            .as_deref()
            .unwrap_or_else(|| panic!("child prompt was not captured; outcome={:?}", self.outcome))
    }
}

fn request_text(request: &LlmRequest) -> String {
    let mut out = String::new();
    for message in &request.messages {
        let role = match message.role {
            LlmRole::System => "system",
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
        };
        out.push_str(role);
        out.push('\n');
        for block in message.blocks.iter() {
            match block {
                LlmContentBlock::Text { text, .. } => out.push_str(text),
                LlmContentBlock::ToolCall { input_json, .. } => out.push_str(input_json),
                LlmContentBlock::ToolResult { content, .. } => out.push_str(content),
                LlmContentBlock::Reasoning { text, .. } => out.push_str(text),
                LlmContentBlock::Attachment { .. } => {}
            }
            out.push('\n');
        }
    }
    out
}

#[tokio::test]
async fn subagents_plugin_builds_without_mode_context() {
    let factory = SubagentsPluginFactory::new(Arc::new(default_registry(&BTreeMap::new())));
    let ctx = PluginSessionContext {
        session_id: "parent".to_string(),
        tool_access: lash_core::SessionToolAccess::default(),
        subagent: None,
        extensions: Default::default(),
        plugin_options: Default::default(),
        protocol_turn_options: Default::default(),
        materialization: lash_core::plugin::PluginSessionMaterialization::Creation,
        parent_session_id: None,
    };
    let plugin = factory.build(&ctx).expect("plugin");
    assert_eq!(plugin.id(), "subagents");
}

#[test]
fn subagents_plugin_final_answer_format_defaults_raw_and_can_be_overridden() {
    let factory = SubagentsPluginFactory::new(Arc::new(default_registry(&BTreeMap::new())));
    assert_eq!(
        factory.final_answer_format,
        lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue
    );

    let factory = factory.with_final_answer_format(lash_rlm_types::RlmFinalAnswerFormat::Markdown);
    assert_eq!(
        factory.final_answer_format,
        lash_rlm_types::RlmFinalAnswerFormat::Markdown
    );
}

#[tokio::test]
async fn rlm_provider_does_not_require_process_support() {
    let factory = SubagentsPluginFactory::new(Arc::new(default_registry(&BTreeMap::new())));
    let ctx = PluginSessionContext {
        session_id: "parent".to_string(),
        tool_access: lash_core::SessionToolAccess::default(),
        subagent: None,
        extensions: Default::default(),
        plugin_options: Default::default(),
        protocol_turn_options: Default::default(),
        materialization: lash_core::plugin::PluginSessionMaterialization::Creation,
        parent_session_id: None,
    };

    let plugin = factory.build(&ctx).expect("rlm plugin");
    assert_eq!(plugin.id(), "subagents");
}

#[test]
fn sublashlang_binding_reports_authority_notes() {
    let authority = lash_core::SubagentSessionContext {
        parent_session_id: "root".to_string(),
        capability: "explore".to_string(),
        depth: 1,
        max_depth: 5,
    };

    assert_eq!(
        rlm_support::subagent_capability_note(&authority),
        "Subagent capability: explore. Depth: 1/5."
    );
}
