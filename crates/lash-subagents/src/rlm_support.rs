//! Private helpers for the RLM subagent tool catalog.

use lash_core::{
    CausalRef, InputItem, SessionCreateRequest, SessionSnapshot, SessionToolAccess,
    SubagentSessionContext, ToolDefinition, ToolResult, TurnInput, facade_support::AssembledTurn,
    facade_support::SessionSpec, facade_support::TurnFinish, facade_support::TurnOutcome,
    facade_support::TurnStop,
};
use lash_lashlang_runtime::ToolDefinitionLashlangExt;
use serde_json::{Value, json};

use crate::capability::{CapabilityRegistry, SubagentSpawnContext};

#[cfg(test)]
pub(crate) fn build_session_policy(
    registry: &CapabilityRegistry,
    current_policy: &lash_core::SessionPolicy,
    capability_name: &str,
) -> Result<lash_core::SessionPolicy, String> {
    let current_snapshot = SessionSnapshot {
        policy: current_policy.clone(),
        ..SessionSnapshot::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let session_spec = SessionSpec::inherit();
    let tool_access = SessionToolAccess::default();
    let request = build_spawn_create_request(SpawnCreateRequestInput {
        registry,
        parent_session_id: "root",
        current_snapshot,
        session_spec: &session_spec,
        tool_access: &tool_access,
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        capability_name,
        output_schema: None,
        seed: Default::default(),
        parent_subagent: None,
        caused_by: None,
    })?;
    request
        .policy
        .ok_or_else(|| "capability did not resolve a child policy".to_string())
}

pub(crate) struct SpawnCreateRequestInput<'a> {
    pub(crate) registry: &'a CapabilityRegistry,
    pub(crate) parent_session_id: &'a str,
    pub(crate) current_snapshot: SessionSnapshot,
    pub(crate) session_spec: &'a SessionSpec,
    pub(crate) tool_access: &'a SessionToolAccess,
    pub(crate) final_answer_format: lash_rlm_types::RlmFinalAnswerFormat,
    pub(crate) capability_name: &'a str,
    pub(crate) output_schema: Option<Value>,
    pub(crate) seed: lash_protocol_rlm::RlmSeed,
    pub(crate) parent_subagent: Option<&'a SubagentSessionContext>,
    pub(crate) caused_by: Option<CausalRef>,
}

pub(crate) fn build_spawn_create_request(
    input: SpawnCreateRequestInput<'_>,
) -> Result<SessionCreateRequest, String> {
    let SpawnCreateRequestInput {
        registry,
        parent_session_id,
        current_snapshot,
        session_spec,
        tool_access,
        final_answer_format,
        capability_name,
        output_schema,
        seed,
        parent_subagent,
        caused_by,
    } = input;
    let capability = registry
        .get(capability_name)
        .ok_or_else(|| unknown_capability_message(capability_name, registry))?;
    capability.build_session_request(SubagentSpawnContext {
        parent_session_id,
        parent_snapshot: &current_snapshot,
        session_spec,
        base_tool_access: tool_access,
        final_answer_format,
        output_schema,
        seed,
        parent_subagent,
        caused_by,
    })
}

pub(crate) fn unknown_capability_message(name: &str, registry: &CapabilityRegistry) -> String {
    let known = registry.names();
    if known.is_empty() {
        format!("unknown capability `{name}`: no capabilities registered")
    } else {
        format!(
            "unknown capability `{name}`: expected one of {}",
            known
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

pub(crate) fn render_task_prompt(task: &str, output_schema: Option<&Value>) -> String {
    let mut sections = vec![task.to_string()];
    if let Some(schema) = output_schema {
        let schema_pretty =
            serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
        sections.push(format!(
            "## Required output\n\nWhen done, end the task with `finish <value>`. The value MUST match this JSON Schema exactly:\n\n```json\n{schema_pretty}\n```"
        ));
    }
    sections.join("\n\n")
}

pub(crate) fn required_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing required parameter: {key}"))
}

pub(crate) fn turn_input_for_task(text: String) -> TurnInput {
    TurnInput {
        items: vec![InputItem::Text { text }],
        protocol_turn_options: None,
        trace_turn_id: None,
        protocol_extension: None,
        turn_context: lash_core::TurnContext::default(),
    }
}

pub(crate) fn capability_list_for_description(capability_names: &[String]) -> String {
    if capability_names.is_empty() {
        return "(no capabilities registered)".to_string();
    }
    let quoted: Vec<String> = capability_names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect();
    match quoted.len() {
        1 => quoted.into_iter().next().expect("len 1"),
        2 => format!("{} or {}", quoted[0], quoted[1]),
        _ => {
            let last = quoted.last().expect("non-empty").clone();
            let head = quoted[..quoted.len() - 1].join(", ");
            format!("{head}, or {last}")
        }
    }
}

pub(crate) fn example_capability_name(capability_names: &[String]) -> String {
    capability_names
        .iter()
        .find(|name| name.as_str() == "explore")
        .or_else(|| capability_names.first())
        .cloned()
        .unwrap_or_else(|| "explore".to_string())
}

pub(crate) fn tool_definition(
    name: &str,
    description: impl Into<String>,
    input_schema: Value,
    examples: Vec<String>,
) -> ToolDefinition {
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        input_schema,
        json!({ "type": "object", "additionalProperties": true }),
    )
    .with_examples(examples)
}

pub(crate) fn spawn_agent_input_schema(capability_names: &[String]) -> Value {
    let enum_values: Vec<Value> = capability_names
        .iter()
        .map(|name| Value::String(name.clone()))
        .collect();
    let mut required = vec!["task"];
    if capability_names.len() != 1 {
        required.push("capability");
    }
    json!({
        "type": "object",
        "properties": {
            "task": { "type": "string" },
            "capability": { "type": "string", "enum": enum_values },
            "output": {
                "type": "object",
                "additionalProperties": true,
                "description": "Optional typed result shape. Use string descriptors for record fields, e.g. `{ queries: \"list[str]\" }`, or pass a Lashlang `Type { ... }` literal for nested shapes."
            },
            "seed": {
                "type": "object",
                "additionalProperties": true,
                "description": "Optional record of state to seed into the child. Each entry's kind is preserved automatically: if its lashlang source root is a host-projected binding (e.g. `seed: { problem: input.prompt }`), the child receives it as a read-only projected binding; otherwise it lands as a regular RLM global. Computed values default to global. Children inherit nothing else from the parent — pass everything they need explicitly."
            }
        },
        "required": required,
        "additionalProperties": false
    })
}

pub(crate) fn submit_error_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:submit_error",
        "submit_error",
        "End the current subagent task as a terminal failure with a concise reason. Use this when the child cannot produce a valid result.",
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Failure reason returned to the parent agents.spawn call."
                }
            },
            "required": ["reason"],
            "additionalProperties": false
        }),
        submit_error_output_schema(),
    )
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["task"],
        "fail",
    ))
}

fn submit_error_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "reason": { "type": "string" }
        },
        "required": ["reason"],
        "additionalProperties": false
    })
}

pub(crate) fn submit_error_tool_result(args: &Value) -> ToolResult {
    ToolResult::ok(args.clone()).with_control(lash_core::ToolControl::Fail {
        failure: lash_core::ToolFailure::tool(
            lash_core::ToolFailureClass::Execution,
            "subagent_submit_error",
            args.to_string(),
        ),
    })
}

/// Render the spawned subagent's tool authority as a prompt note. Under the
/// flat catalog, tool-list notes are ordinary prompt contributions authored by
/// the host, not a catalog property.
pub(crate) fn subagent_capability_note(authority: &SubagentSessionContext) -> String {
    format!(
        "Subagent capability: {}. Depth: {}/{}.",
        authority.capability, authority.depth, authority.max_depth
    )
}

pub(crate) fn task_result_value(turn: &AssembledTurn) -> Value {
    match &turn.outcome {
        TurnOutcome::Finished(TurnFinish::FinalValue { value }) => return value.clone(),
        TurnOutcome::Finished(TurnFinish::ToolValue { value, .. }) => return value.clone(),
        TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) => {
            if !text.trim().is_empty() {
                return json!(text.trim().to_string());
            }
        }
        TurnOutcome::Stopped(TurnStop::SubmittedError { value }) => return value.clone(),
        TurnOutcome::Stopped(TurnStop::ToolError { value, .. }) => return value.clone(),
        TurnOutcome::AgentFrameSwitch {
            frame_key, task, ..
        } => {
            return json!({ "frame_key": frame_key, "task": task });
        }
        TurnOutcome::Stopped(_) => {}
    }
    if !turn.assistant_output.safe_text.trim().is_empty() {
        return json!(turn.assistant_output.safe_text.trim().to_string());
    }
    json!(turn.assistant_output.raw_text.trim().to_string())
}

/// Wrap an `Ok`/`Err` result as a `ToolResult`. Used by both providers'
/// `execute` so error encoding stays identical.
pub(crate) fn finalise_tool_result(result: Result<Value, String>) -> ToolResult {
    match result {
        Ok(value) => ToolResult::ok(value),
        Err(err) => ToolResult::err(json!(err)),
    }
}
