// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::rlm_support::{
    SpawnCreateRequestInput, build_session_policy, build_spawn_create_request,
};
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, LlmRole};
use lash_core::runtime::RuntimeSessionState;
use lash_core::{
    SessionPolicy, facade_support::LashRuntime, facade_support::PluginFactory,
    facade_support::PluginHost, facade_support::ProcessRuntimeHost,
    facade_support::RuntimeHostConfig, facade_support::TraceRuntimeSubject,
    test_support::RuntimeServices,
};
use lash_core::{ToolArgumentProjectionPolicy, ToolOutputContract, TurnInput};
use lash_lashlang_runtime::{
    LASHLANG_SURFACE_EXTENSION_ID, LashlangAbilities, LashlangHostCatalog,
    LashlangLanguageFeatures, LashlangProcessEngine, LashlangSurface, LashlangSurfaceContribution,
    TraceLashlangGraphStore,
};
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
    child_response: String,
    child_execution_count: Arc<AtomicUsize>,
    captured_child_prompt: Arc<Mutex<Option<String>>>,
}

struct BoundaryValidationCapability;

impl Capability for BoundaryValidationCapability {
    fn name(&self) -> &str {
        "default"
    }

    fn build_session_request(
        &self,
        mut ctx: SubagentSpawnContext<'_>,
    ) -> Result<lash_core::SessionCreateRequest, String> {
        ctx.output_schema = None;
        ctx.rlm_request(
            self.name(),
            &SessionSpec::inherit(),
            lash_core::SessionPluginSource::CurrentHostFresh,
        )
    }
}

fn prompt_advertises_bound_variable(prompt: &str, name: &str) -> bool {
    prompt.contains(&format!("- `{name}`:")) || prompt.contains(&format!("- `{name}` ="))
}

fn typescript_block(code: &str) -> String {
    format!("<typescript>\n{code}\n</typescript>")
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
        tool_access
            .hide_tool("custom_hidden")
            .map_err(|error| error.to_string())?;
        let request = lash_core::SessionCreateRequest::child(
            ctx.parent_session_id,
            lash_core::SessionStartPoint::Empty,
            ctx.base_policy(),
            lash_core::PluginOptions::default(),
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
    let tool_access = lash_core::SessionToolAccess::ambient()
        .with_hidden_tools(["base_hidden"])
        .expect("valid hidden name");

    let request = build_spawn_create_request(SpawnCreateRequestInput {
        registry: &registry,
        parent_session_id: &SessionId::from("root"),
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
        lash_core::SessionStartPoint::Empty
    ));
    assert!(request.tool_access.hidden_tools().contains("base_hidden"));
    assert!(request.tool_access.hidden_tools().contains("custom_hidden"));
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
    assert!(request.tool_access.restricted_tools().is_none());

    let structured_request = build_spawn_create_request(SpawnCreateRequestInput {
        registry: &registry,
        parent_session_id: &SessionId::from("root"),
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
    assert!(structured_request.tool_access.restricted_tools().is_none());
}

#[tokio::test]
async fn rlm_spawn_seed_is_visible_to_child_executor_and_prompt() {
    let (outcome, prompt) = run_seed_probe(
        r#"<typescript>
const result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(result);
</typescript>"#,
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

/// Every child session reads the TypeScript prompt.
///
/// Children used to be created with no dialect, which resolved to a Lashlang
/// default, so a TypeScript parent silently spawned Lashlang children. Since
/// ADR 0096 there is only one language, so the guarantee is stated directly:
/// the prompt a child is actually served is the TypeScript one. This drives a
/// real spawn and reads that prompt rather than a synthesised one.
#[tokio::test]
async fn a_typescript_parent_spawns_typescript_children() {
    let (_outcome, prompt) = run_seed_probe(
        r#"<typescript>
const result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(result);
</typescript>"#,
        TurnInput::text("spawn a child from a typescript parent"),
    )
    .await;

    assert!(
        prompt.contains("## TypeScript execution"),
        "a child must read the TypeScript prompt:\n{prompt}"
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
/// prompt. So this drives a real spawn and reads the served prompt of the
/// session that ran — a TypeScript session, which is the reader the leak was
/// measured on and, since ADR 0096, the only reader there is.
#[tokio::test]
async fn spawn_agent_doc_resolves_its_dialect_token_in_a_served_prompt() {
    let parent_response = r#"<typescript>
const result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(result);
</typescript>"#;

    let (_outcome, typescript) = run_seed_probe(
        parent_response,
        TurnInput::text("spawn a child from a typescript parent"),
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

    // The token itself never reaches a model. Scoped to the rendered tool-doc
    // section, which is the surface the registration guard governs — the
    // builtin docs legitimately spell `{{` as the escape for a literal brace
    // in `format`.
    let docs = tool_doc_section(&typescript);
    assert!(
        docs.contains("Optional typed result shape"),
        "the tool-doc section must be the slice under test:\n{docs}"
    );
    assert!(
        !docs.contains("{{"),
        "unresolved prose token in the tool docs:\n{docs}"
    );
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

// `a_lashlang_parent_still_spawns_lashlang_children` was deleted with the
// second dialect (ADR 0096): there is no other direction to hold open.
#[tokio::test]
async fn rlm_spawn_record_shorthand_returns_child_final_value() {
    let (outcome, _) = run_seed_probe(
        r#"<typescript>
const direct = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(direct);
</typescript>"#,
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
async fn schema_mismatch_stops_after_one_child_execution_and_reaches_parent_failure() {
    let probe = run_seed_probe_inner_dispatch_with(
        typescript_block(
            r#"
try {
  const result = await agents.spawn({
    capability: "default",
    task: "Return a len value.",
    output: { len: "int" }
  });
  finish(result);
} catch (error) {
  finish({ ok: false, error: error.message });
}"#,
        ),
        typescript_block(r#"finish("not-an-object");"#),
        TurnInput::text("reject the child's invalid typed result"),
        Arc::new(BoundaryValidationCapability),
    )
    .await;

    let child_executions_at_rejection = probe.child_execution_count();
    assert_eq!(
        child_executions_at_rejection, 1,
        "the terminal boundary rejection must follow exactly one child execution"
    );
    tokio::task::yield_now().await;
    assert_eq!(
        probe.child_execution_count(),
        child_executions_at_rejection,
        "the parent must not start another child execution after terminal rejection"
    );
    let lash_core::facade_support::TurnOutcome::Finished(
        lash_core::facade_support::TurnFinish::FinalValue { value },
    ) = probe.outcome
    else {
        panic!(
            "schema mismatch must reach the parent's failure outcome: {:?}",
            probe.outcome
        );
    };
    assert!(
        value["ok"] == json!(false)
            && value["error"].as_str().is_some_and(|message| message
                .starts_with("subagent task result did not match the declared output schema:")),
        "unexpected boundary rejection: {value}"
    );
}

/// FIG-2975: a child that ends its task with `submit_error` reaches the
/// parent, its own process record and its terminal process event carrying the
/// exact reason it wrote — not a generic substitute for it.
#[tokio::test]
async fn submitted_child_failure_reason_reaches_parent_record_and_terminal_event() {
    const REASON: &str = "missing shard amber";
    let probe = run_seed_probe_inner_dispatch_with(
        typescript_block(
            r#"
try {
  const result = await agents.spawn({
    capability: "default",
    task: "Fail with a distinctive reason.",
    output: { len: "int" }
  });
  finish({ ok: true, result });
} catch (error) {
  finish({ ok: false, error: error.message });
}"#,
        ),
        typescript_block(r#"await task.fail({ reason: "missing shard amber" });"#),
        TurnInput::text("carry the child's own failure reason to the parent"),
        Arc::new(StaticCapability::new("default", SessionSpec::inherit())),
    )
    .await;

    // 1. The parent's spawn result.
    let lash_core::facade_support::TurnOutcome::Finished(
        lash_core::facade_support::TurnFinish::FinalValue { value },
    ) = &probe.outcome
    else {
        panic!(
            "the failing child must reach the parent's catch block: {:?}",
            probe.outcome
        );
    };
    assert_eq!(
        value["ok"],
        json!(false),
        "unexpected parent result: {value}"
    );
    assert_eq!(
        value["error"],
        json!(REASON),
        "the parent's spawn result must carry the child's own reason: {value}"
    );

    // 2. The child's process record, as a polling host observes it.
    let child = probe.observed_subagent_process().await;
    assert_eq!(child.lifecycle, lash_core::ProcessStatus::Failed);
    assert_eq!(child.error.as_deref(), Some(REASON));
    assert_eq!(
        child.error_code,
        Some(lash_core::ObservedProcessFailure::Failed {
            class: lash_core::ToolFailureClass::Execution,
            code: "process_session_turn_tool_error".to_string(),
        }),
        "the record's typed classification must name the child's stop, not a shared code"
    );

    // 3. The terminal process event.
    let events = lash_core::ProcessEventLogTestSupport::full_event_window(
        probe.process_registry.as_ref(),
        &child.process_id,
        0,
    )
    .await
    .expect("load the child's process lifecycle events");
    let mut terminals = events
        .iter()
        .filter(|event| event.semantics.terminal.is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "expected one terminal event for {}: {events:?}",
        child.process_id
    );
    let terminal_event = terminals.remove(0);
    assert_eq!(terminal_event.event_type, "process.failed");
    let terminal = terminal_event
        .semantics
        .terminal
        .as_ref()
        .expect("filtered on terminal semantics");
    assert_eq!(terminal.status, lash_core::ProcessStatus::Failed);
    let lash_core::ProcessAwaitOutput::Settled { output } = &terminal.outcome else {
        panic!("the child settled with a terminal outcome: {terminal_event:?}");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("the child's terminal event must record a failure: {terminal_event:?}");
    };
    assert_eq!(failure.message, REASON);
    assert_eq!(failure.code, "process_session_turn_tool_error");
    // The encoded `submit_error` call stays available for diagnosis without
    // displacing the one field a parent model reads.
    let raw = failure
        .raw
        .as_ref()
        .map(lash_core::ToolValue::to_json_value)
        .expect("bounded diagnostics ride `raw`");
    assert!(
        raw["stop"]
            .as_str()
            .is_some_and(|stop| stop.contains("subagent_submit_error") && stop.contains(REASON)),
        "the child's original submit_error call must survive in `raw`: {raw}"
    );
}

#[tokio::test]
async fn rlm_spawn_is_visible_through_parent_session_process_observer() {
    let probe = run_seed_probe_inner_dispatch(
        typescript_block(
            r#"
const result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(result);"#,
        ),
        TurnInput::text("spawn one observable subagent process"),
        None,
    )
    .await;
    probe.assert_process_visibility("subagent", "spawn").await;
    assert_eq!(
        probe.outcome,
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
        r#"<typescript>
const spawnChild = async () => {
  const result = await agents.spawn({
    capability: "default",
    task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: { len: "int" }
  });
  return result;
};
const handle = await processes.start({ definition: spawnChild });
finish(await handle);
</typescript>"#,
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
async fn rlm_spawn_links_subagent_process_from_lashlang_graph() {
    let graph_store = Arc::new(TraceLashlangGraphStore::default());
    let (outcome, _) = run_seed_probe_with_graph_store(
        r#"<typescript>
const result = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(result);
</typescript>"#,
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
            graph.scope.session_id.as_deref() == Some("root")
                && matches!(&graph.subject, TraceRuntimeSubject::Effect { .. })
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
    assert!(child.child_graph_key.is_none());
    assert!(
        child
            .child_process_id
            .as_str()
            .starts_with("process:subagent:")
    );
    assert!(child.child_incarnation > 0);
    assert_eq!(child.child_attempt, None);
}

#[tokio::test]
async fn rlm_spawn_defaults_single_capability_when_omitted() {
    let (outcome, prompt) = run_seed_probe(
        r#"<typescript>
const result = await agents.spawn({
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(result);
</typescript>"#,
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
    let is_child = request.scope.session_id != "root";
    if is_child {
        state.child_execution_count.fetch_add(1, Ordering::SeqCst);
        *state.captured_child_prompt.lock_recover() = Some(prompt);
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: state.child_response.clone(),
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

async fn run_seed_probe_with_graph_store(
    parent_response: &'static str,
    input: TurnInput,
    graph_store: Option<Arc<TraceLashlangGraphStore>>,
) -> (lash_core::facade_support::TurnOutcome, String) {
    let probe =
        run_seed_probe_inner_dispatch(parent_response.to_string(), input, graph_store).await;
    let child_prompt = probe.child_prompt().to_string();
    (probe.outcome, child_prompt)
}

async fn run_seed_probe_inner_dispatch(
    parent_response: String,
    input: TurnInput,
    graph_store: Option<Arc<TraceLashlangGraphStore>>,
) -> SeedProbe {
    run_seed_probe_inner_dispatch_with_options(
        parent_response,
        typescript_block("finish({ len: chunk.length });"),
        input,
        graph_store,
        Arc::new(StaticCapability::new("default", SessionSpec::inherit())),
    )
    .await
}

async fn run_seed_probe_inner_dispatch_with(
    parent_response: String,
    child_response: String,
    input: TurnInput,
    capability: Arc<dyn Capability>,
) -> SeedProbe {
    run_seed_probe_inner_dispatch_with_options(
        parent_response,
        child_response,
        input,
        None,
        capability,
    )
    .await
}

async fn run_seed_probe_inner_dispatch_with_options(
    parent_response: String,
    child_response: String,
    input: TurnInput,
    graph_store: Option<Arc<TraceLashlangGraphStore>>,
    capability: Arc<dyn Capability>,
) -> SeedProbe {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("subagent-seed-probe".to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(move || {
            let test = Box::pin(run_seed_probe_inner(
                parent_response,
                child_response,
                input,
                graph_store,
                capability,
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
    child_response: String,
    input: TurnInput,
    graph_store: Option<Arc<TraceLashlangGraphStore>>,
    capability: Arc<dyn Capability>,
) -> SeedProbe {
    let captured_child_prompt: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let child_execution_count = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(SeedProbeState {
        parent_response,
        child_response,
        child_execution_count: Arc::clone(&child_execution_count),
        captured_child_prompt: Arc::clone(&captured_child_prompt),
    });
    let provider = seed_probe_provider(Arc::clone(&state)).into_handle();
    let execution_sink: Option<Arc<dyn lash_core::facade_support::TraceSink>> = graph_store
        .as_ref()
        .map(|store| Arc::clone(store) as Arc<dyn lash_core::facade_support::TraceSink>);
    let trace_context = lash_core::TraceContext::default();
    let language_features = LashlangLanguageFeatures::default().with_label_annotations();
    // One SQLite memory backend (ADR 0102) holds every port of the probe; the
    // handle lives for the whole probe, and with it the databases.
    let backend: Arc<dyn lash_core::Backend> = Arc::new(
        lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open a SQLite memory backend"),
    );
    // The RLM protocol plugin (which compiles + stores the parent turn's process
    // artifacts) and the process engine that the worker runs those artifacts
    // through must share ONE artifact store; otherwise the worker cannot load the
    // module the parent wrote. Both take it from one memory backend.
    let artifact_backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open the artifact backend");
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&artifact_backend);

    let factories: Vec<Arc<dyn PluginFactory>> = vec![
        Arc::new(
            lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build()
                    .with_lashlang_language_features(language_features),
                &artifact_backend,
            )
            .with_lashlang_execution_trace(execution_sink.clone(), trace_context.clone())
            // This harness assembles the plugin host and process engine by hand
            // (no core install step records lifecycle availability), so declare
            // it explicitly: the worker below runs real processes.
            .with_process_lifecycle(true),
        ),
        Arc::new(SubagentsPluginFactory::new(Arc::new(
            CapabilityRegistry::new().with(capability),
        ))),
        // The `processes` module is catalogue presence, not an ability bit
        // (ADR 0095): the seeded programs author `processes.start`, so the
        // surface only exists if this factory is in the session's factories.
        Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()),
    ];
    let registry = backend.process_registry();
    let host_plugins = PluginHost::new(factories.clone());
    let process_abilities = LashlangAbilities::default().with_sleep();
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
        LashlangProcessEngine::new(artifact_store.clone(), process_surface)
            .with_execution_trace(execution_sink, trace_context),
    );
    let plugins = host_plugins
        .with_extensions(extensions)
        .build_session("root")
        .expect("plugin session");
    let embedded = lash_core::facade_support::EmbeddedRuntimeHost::new({
        let mut config = RuntimeHostConfig::new(
            Arc::clone(&backend),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        config.providers.provider_resolver = Arc::new(
            lash_core::facade_support::SingleProviderResolver::new(provider.clone()),
        );
        config.with_process_engine_registration(lash_core::ProcessEngineRegistration::accepting(
            process_engine.clone(),
        ))
    });
    let policy = SessionPolicy {
        provider_id: provider.kind().to_string(),
        model: model_spec("seed-probe-model", None, 64_000),
        turn_budget: lash_core::TurnBudget::bounded(4),
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    // `agents.spawn(...)` starts a SessionTurn (subagent) process that the
    // lease-protected worker executes — not directly. A SINGLE native runner over
    // the same registry + an explicit in-memory store factory runs it (and
    // provider re-supply reaches the child). One runner suffices even for the
    // nested case here (`handle = start spawn_child` then `await handle`) because
    // the worker runs each process on its own task, so the parent's await never
    // parks the runner away from the child.
    let watched = lash_core::facade_support::watch_process_registry(Arc::clone(&registry));
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::from_plugin_factories(
            factories,
            {
                let mut config = RuntimeHostConfig::new(
                    Arc::clone(&backend),
                    lash_core::CommitBudget::bounded(1024 * 1024, 512),
                    lash_core::QueuedWorkBatchingConfig::new(1),
                );
                config.providers.provider_resolver = Arc::new(
                    lash_core::facade_support::SingleProviderResolver::new(provider.clone()),
                );
                config.with_process_engine_registration(
                    lash_core::ProcessEngineRegistration::accepting(process_engine),
                )
            },
            lash_core_worker::WorkerProcessWork::SelfNative(watched.clone()),
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
    let runtime_host = host;
    let runtime_services = RuntimeServices::new(
        plugins,
        std::sync::Arc::clone(&runtime_host.embedded().core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.embedded().core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_background_state(
        policy.clone(),
        runtime_host,
        runtime_services,
        RuntimeSessionState {
            session_id: SessionId::from("root"),
            policy,
            protocol_turn_options: lash_core::ProtocolTurnOptions::default(),
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

    let scoped_effect_controller = backend
        .effect_host()
        .scoped_static(lash_core::AdmittedScope::turn("root", "subagent-test-turn"))
        .expect("test execution scope")
        .expect("the backend host lends a static controller");
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
        child_execution_count,
        process_registry: registry,
        _backend: backend,
    }
}

/// What one probe run observed: the turn's outcome plus the prompt the child was
/// actually served.
struct SeedProbe {
    outcome: lash_core::facade_support::TurnOutcome,
    child_prompt: Option<String>,
    child_execution_count: Arc<AtomicUsize>,
    process_registry: Arc<dyn lash_core::ProcessRegistry>,
    /// Holds the probe's memory backend, whose databases the registry reads.
    _backend: Arc<dyn lash_core::Backend>,
}

impl SeedProbe {
    fn child_execution_count(&self) -> usize {
        self.child_execution_count.load(Ordering::SeqCst)
    }

    fn child_prompt(&self) -> &str {
        self.child_prompt
            .as_deref()
            .unwrap_or_else(|| panic!("child prompt was not captured; outcome={:?}", self.outcome))
    }

    /// The one subagent process this probe's parent spawned, as a polling
    /// host observes it.
    async fn observed_subagent_process(&self) -> lash_core::facade_support::ObservedProcess {
        let observer =
            lash_core::facade_support::ProcessWorkObserver::new(Arc::clone(&self.process_registry));
        let observed = observer
            .list(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("list observed processes");
        let mut subagents = observed
            .into_iter()
            .filter(|process| process.kind() == "subagent")
            .collect::<Vec<_>>();
        assert_eq!(
            subagents.len(),
            1,
            "expected exactly one subagent process: {subagents:?}"
        );
        subagents.remove(0)
    }

    async fn assert_process_visibility(&self, kind: &str, label: &str) {
        let observed_processes = lash_core::ProcessObserverRegistry::list_observed_by(
            self.process_registry.as_ref(),
            &SessionId::from("root"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list processes through the parent session observer");
        let observed_identities = observed_processes
            .iter()
            .map(|process| (&process.id, &process.identity, &process.status))
            .collect::<Vec<_>>();
        let matching = observed_processes
            .iter()
            .filter(|process| {
                process.identity.kind == kind && process.identity.label.as_deref() == Some(label)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "the parent session observer must expose one {kind}/{label} process record; observed={observed_identities:?}"
        );
        let process = matching[0];
        assert_eq!(
            process.lifecycle.on_parent_end,
            lash_core::OnParentEnd::Abandon
        );
        assert!(
            matches!(&process.lifecycle.parent, lash_core::ParentScope::Owned(lash_core::EffectOpener::Turn { session_id, .. }) if session_id.as_str() == "root"),
            "spawn_agent retains its originating turn scope"
        );
        let observers = lash_core::ProcessObserverRegistry::observers_for_process(
            self.process_registry.as_ref(),
            &process.id,
        )
        .await
        .expect("load process observer edges");
        assert!(
            observers.iter().any(|observer| observer == "root"),
            "the spawning session must retain the observer edge for {}; observers={observers:?}",
            process.id
        );

        let events = lash_core::ProcessEventLogTestSupport::full_event_window(
            self.process_registry.as_ref(),
            &process.id,
            0,
        )
        .await
        .expect("load observer-visible process lifecycle events");
        let first_started = events
            .iter()
            .position(|event| event.event_type == "process.first_started")
            .unwrap_or_else(|| {
                panic!(
                    "missing process.first_started for {}: {events:?}",
                    process.id
                )
            });
        let completed = events
            .iter()
            .position(|event| event.event_type == "process.completed")
            .unwrap_or_else(|| panic!("missing process.completed for {}: {events:?}", process.id));
        assert!(
            first_started < completed,
            "process.first_started must precede process.completed for {}: {events:?}",
            process.id
        );
        assert_eq!(process.status, lash_core::ProcessStatus::Completed);
    }
}

fn request_text(request: &LlmRequest) -> String {
    let mut out = String::new();
    if let Some(instructions) = &request.instructions {
        out.push_str("instructions\n");
        out.push_str(instructions);
        out.push('\n');
    }
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
                LlmContentBlock::ToolResult { content, .. } => {
                    out.push_str(&lash_core::facade_support::tool_result_text(content));
                }
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
        session_id: SessionId::from("parent"),
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
        .map(|example| lash_protocol_rlm::render_tool_example_for_typescript_catalog(example))
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
