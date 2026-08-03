//! RLM protocol subagent spawning surface.
//!
//! Examples are written in Lashlang module syntax. Prompt prose is tuned for
//! schema-first results and binding subagent output.

use std::sync::Arc;

use async_trait::async_trait;
use lash_core::{
    PreparedToolCall, SessionToolAccess, SubagentSessionContext, ToolArgumentProjectionPolicy,
    ToolCall, ToolContext, ToolDefinition, ToolPrepareContext, ToolResult,
    facade_support::SessionSpec, sansio::PendingToolCall,
};
use lash_lashlang_runtime::ToolDefinitionLashlangExt;
use lash_tool_support::{StaticToolExecute, StaticToolProvider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::CapabilityRegistry;
use crate::rlm_support::{
    self, SpawnCreateRequestInput, build_spawn_create_request, capability_list_for_description,
    example_capability_name, finalise_tool_result, render_task_prompt, required_string,
    spawn_agent_input_schema, task_result_value, tool_definition, turn_input_for_task,
    unknown_capability_message,
};

pub(crate) struct RlmSubagentToolsProvider {
    pub(crate) registry: Arc<CapabilityRegistry>,
    pub(crate) session_spec: SessionSpec,
    pub(crate) tool_access: SessionToolAccess,
    pub(crate) final_answer_format: lash_rlm_types::RlmFinalAnswerFormat,
    pub(crate) parent_subagent: Option<SubagentSessionContext>,
    pub(crate) include_submit_error: bool,
}

impl RlmSubagentToolsProvider {
    /// A subagent spawn *is* a process that runs a child lash session: decode the
    /// journaled create request + task, emit one generic
    /// `ProcessInput::SessionTurn`, and await its handle. Going through the
    /// process worker (rather than any bespoke session-lifecycle code in this
    /// crate) is what re-supplies the live parent provider, gives the child
    /// durability, and makes it recoverable — the same generic path every other
    /// background session turn takes.
    async fn spawn_agent(&self, _args: &Value, context: &ToolContext<'_>) -> Result<Value, String> {
        let prepared: PreparedSpawnAgent = context
            .decode_prepared_payload()
            .map_err(|err| format!("spawn_agent was not prepared correctly: {err}"))?;

        if context
            .sessions()
            .tool_catalog()
            .await
            .ok()
            .is_some_and(|catalog| {
                catalog.iter().all(|tool| {
                    tool.get("name").and_then(serde_json::Value::as_str) != Some("spawn_agent")
                })
            })
        {
            return Err("subagent spawning is unavailable in this session".to_string());
        }

        let request = lash_core::ProcessStartRequest::new(
            prepared.process_id.clone(),
            lash_core::ProcessInput::SessionTurn {
                create_request: prepared.create_request,
                turn_input: Box::new(prepared.turn_input),
                output_contract: lash_core::ToolOutputContract::Static,
            },
            // Subagent session-turn rows are journaled child sessions, idempotent
            // by process id, so recovery may re-execute them (ADR 0019).
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessOriginator::host(),
        )
        .with_identity(
            lash_core::ProcessIdentity::new("subagent").with_label(Some("spawn".to_string())),
        );
        context
            .processes()
            .start(request)
            .await
            .map_err(|err| format!("failed to start subagent process: {err}"))?;
        context
            .emit_child_process_started(prepared.process_id.clone(), Some("subagent".to_string()));
        let output = context
            .processes()
            .await_process(&prepared.process_id)
            .await
            .map_err(|err| format!("subagent failed while executing its task: {err}"))?;
        child_task_result(output)
    }

    async fn prepare_spawn_agent(
        &self,
        args: &Value,
        context: &lash_core::ToolPrepareContext,
        tool_id: lash_core::ToolId,
        call: lash_core::sansio::PendingToolCall,
    ) -> Result<PreparedToolCall, ToolResult> {
        let task =
            required_string(args, "task").map_err(|err| ToolResult::err(serde_json::json!(err)))?;
        let capability_name = capability_name_from_args(args, &self.registry)
            .map_err(|err| ToolResult::err(serde_json::json!(err)))?;
        if self.registry.get(&capability_name).is_none() {
            return Err(ToolResult::err(serde_json::json!(
                unknown_capability_message(&capability_name, &self.registry)
            )));
        }
        let output_schema = lash_lashlang_runtime::parse_output_schema(args.get("output"))
            .map_err(|err| ToolResult::err(serde_json::json!(err)))?;
        let seed = lash_protocol_rlm::RlmSeed::from_tool_args(args)
            .map_err(|err| ToolResult::err(serde_json::json!(err)))?;
        let current_snapshot = context
            .session_snapshot()
            .await
            .map_err(|err| ToolResult::err(serde_json::json!(err.to_string())))?;
        let create_request = Box::new(
            build_spawn_create_request(SpawnCreateRequestInput {
                registry: &self.registry,
                parent_session_id: context.session_id(),
                current_snapshot,
                session_spec: &self.session_spec,
                tool_access: &self.tool_access,
                final_answer_format: self.final_answer_format.clone(),
                capability_name: &capability_name,
                output_schema: output_schema.clone(),
                seed,
                parent_subagent: self.parent_subagent.as_ref(),
                caused_by: context
                    .tool_call_id()
                    .map(|call_id| lash_core::CausalRef::ToolCall {
                        session_id: context.session_id().to_string(),
                        call_id: call_id.to_string(),
                    }),
            })
            .map_err(|err| ToolResult::err(serde_json::json!(err)))?,
        );
        let turn_input = turn_input_for_task(render_task_prompt(&task, output_schema.as_ref()));
        // Mint the child's process identity here, in the prepared (journaled)
        // payload, so it is stable across replay — the durable layer keys the
        // child session turn by this persisted `process_id` end-to-end. The
        // parent tool-call id is unique per call and always non-empty.
        let process_id = format!("process:subagent:{}", call.call_id);
        let payload = serde_json::to_value(PreparedSpawnAgent {
            process_id,
            create_request,
            turn_input,
        })
        .map_err(|err| ToolResult::err(serde_json::json!(err.to_string())))?;
        Ok(PreparedToolCall::from_parts(
            call.call_id,
            tool_id,
            call.tool_name,
            call.args,
            call.replay,
            payload,
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PreparedSpawnAgent {
    process_id: String,
    create_request: Box<lash_core::SessionCreateRequest>,
    turn_input: lash_core::TurnInput,
}

/// Project the awaited subagent process output back onto the spawn tool's
/// result. The generic `SessionTurn` runner wraps the child's terminal
/// `AssembledTurn` in its success value; recover it and apply the existing
/// `task_result_value` mapping so the spawn surface is unchanged. A child that
/// terminated via `submit_error` (or otherwise failed) surfaces as a tool error
/// carrying its reason.
fn child_task_result(output: lash_core::ProcessAwaitOutput) -> Result<Value, String> {
    match output {
        lash_core::ProcessAwaitOutput::Success { value, .. } => {
            let turn: lash_core::facade_support::AssembledTurn = value
                .get("turn")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|err| format!("subagent process output was malformed: {err}"))?
                .ok_or_else(|| "subagent process output was missing its turn".to_string())?;
            Ok(task_result_value(&turn))
        }
        lash_core::ProcessAwaitOutput::Failure { message, .. } => Err(message),
        lash_core::ProcessAwaitOutput::Cancelled { message, .. } => Err(message),
        lash_core::ProcessAwaitOutput::Abandoned { .. } => {
            Err("subagent process was abandoned before recording an outcome".to_string())
        }
        lash_core::ProcessAwaitOutput::NoLongerRetained { .. } => {
            Err("subagent process outcome is no longer retained".to_string())
        }
    }
}

#[async_trait]
impl StaticToolExecute for RlmSubagentToolsProvider {
    async fn prepare_tool_call(
        &self,
        tool_id: &lash_core::ToolId,
        pending: PendingToolCall,
        context: &ToolPrepareContext,
    ) -> Result<PreparedToolCall, ToolResult> {
        if pending.tool_name == "spawn_agent" {
            let args = pending.args.clone();
            self.prepare_spawn_agent(&args, context, tool_id.clone(), pending)
                .await
        } else {
            Ok(PreparedToolCall::identity(tool_id.clone(), pending))
        }
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        let result = match call.name {
            "spawn_agent" => self.spawn_agent(call.args, call.context).await,
            "submit_error" => return rlm_support::submit_error_tool_result(call.args),
            other => Err(format!("Unknown tool: {other}")),
        };
        finalise_tool_result(result)
    }
}

impl RlmSubagentToolsProvider {
    /// Build the cached subagent tool provider. The served definitions are
    /// fixed once the provider is constructed (they depend only on the
    /// registered capability names and whether `submit_error` is exposed).
    pub(crate) fn into_provider(self) -> StaticToolProvider<Self> {
        let definitions = self.tool_definitions();
        StaticToolProvider::new(definitions, self)
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = rlm_subagent_tool_definitions(&self.registry.names());
        if self.include_submit_error {
            definitions.push(rlm_support::submit_error_tool_definition());
        }
        definitions
    }
}

pub(crate) fn rlm_subagent_tool_definitions(capability_names: &[String]) -> Vec<ToolDefinition> {
    vec![spawn_agent_tool_definition(capability_names)]
}

pub fn spawn_agent_tool_definition(capability_names: &[String]) -> ToolDefinition {
    let example_capability = example_capability_name(capability_names);
    let capability_arg = capability_example_arg(capability_names, &example_capability);
    spawn_agent_definition(
        capability_names,
        vec![
            // Parallel subagent fan-out: start process handles first, then join.
            format!(
                r#"process research(agents: Agents, task: str) {{
  result = await agents.spawn({{ task: task{capability_arg}, output: {{ summary: "str" }} }})?
  finish result
}}
handles = {{
  first: start research(agents: agents, task: "Research the first topic"),
  second: start research(agents: agents, task: "Research the second topic")
}}
results = await handles
finish {{ first: results.first?, second: results.second? }}"#
            ),
            // Schema-first: the highest-leverage shape — bind a typed result.
            format!(
                r#"typed = await agents.spawn({{ task: "Find the longest line in src/main.rs"{capability_arg}, output: {{ line: "str", length: "int" }} }})?"#
            ),
            // Record shorthand uses string descriptors for every field,
            // including list fields.
            format!(
                r#"queries = await agents.spawn({{ task: "Generate two focused web search queries"{capability_arg}, output: {{ queries: "list[str]" }} }})?"#
            ),
            // Reusable Type literal for richer shapes.
            r#"Shape = Type { name: str, tags: list[str], status: enum["ok", "err"] }"#.into(),
            format!(
                r#"signed = await agents.spawn({{ task: "Parse the book listing in data/books.json"{capability_arg}, output: Shape }})?"#
            ),
            // seed: pass projected source through to the child as a projected
            // binding; pass plain values as RLM globals on the child.
            format!(
                r#"answer = await agents.spawn({{ task: "Solve sub-problem 3 using the bound problem text and the running findings."{capability_arg}, seed: {{ problem: input.prompt, findings: findings }}, output: {{ value: "int" }} }})?"#
            ),
            // Untyped is fine for free-form prose results.
            format!(
                r#"prose = await agents.spawn({{ task: "Skim the routes in api/ and flag any missing auth checks"{capability_arg} }})?"#
            ),
        ],
    )
}

fn spawn_agent_definition(capability_names: &[String], examples: Vec<String>) -> ToolDefinition {
    let cap_list = capability_list_for_description(capability_names);
    let capability_detail = capability_detail_for_tool_description(capability_names);
    let description = format!(
        "Run one subagent through the `agents.spawn` module operation and return its final result. A direct `await agents.spawn(...)` call blocks until that child finishes, so multiple direct awaits are serial. For parallel subagent fan-out, declare a named process that accepts `agents: Agents`, call `await agents.spawn({{ ... }})?` inside it, start every branch process first with `agents: agents`, then join the handles with `results = await handles`. {capability_detail} `output` defines the typed return shape. Available capabilities: {cap_list}. \
        In record shorthand, each `output` field value is a string type descriptor such as `\"str\"`, `\"int\"`, or `\"list[str]\"`; pass a Lashlang `Type {{ ... }}` literal for nested shapes. \
        \n\nThe child starts with **no** inherited state — globals, projected bindings, message history are all blank. Hand it specific data via `seed: {{ name: value, ... }}`. Each entry's kind is preserved automatically: if `value`'s lashlang source root is a host-projected binding (e.g. `seed: {{ problem: input.prompt }}`) the child receives `problem` as a read-only projected binding, identical to how it appeared on the parent. Otherwise it lands as a regular RLM global. Computed expressions default to global. Projected seed entries require an RLM child; passing one to a non-RLM capability is an error.\
        \n\nA child can fail terminally with `await task.fail({{ reason: \"...\" }})?`; this tool returns an error with that reason."
    );
    tool_definition(
        "spawn_agent",
        description,
        spawn_agent_input_schema(capability_names),
        examples,
    )
    .with_argument_projection(
        ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
    )
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["agents"],
        "spawn",
    ))
    .with_output_from_input_schema("output", None)
}

fn capability_detail_for_tool_description(capability_names: &[String]) -> String {
    if capability_names.len() == 1 {
        return "Only one capability is available in this session, so omit `capability` unless you need to be explicit.".to_string();
    }
    "Pick `capability` from the available list; unavailable capability names are rejected."
        .to_string()
}

fn capability_example_arg(capability_names: &[String], example_capability: &str) -> String {
    if capability_names.len() == 1 {
        String::new()
    } else {
        format!(r#", capability: "{example_capability}""#)
    }
}

fn capability_name_from_args(
    args: &Value,
    registry: &CapabilityRegistry,
) -> Result<String, String> {
    match args.get("capability") {
        Some(Value::String(capability)) => Ok(capability.clone()),
        Some(_) => Err("field `capability` must be a string".to_string()),
        None => {
            let names = registry.names();
            match names.as_slice() {
                [only] => Ok(only.clone()),
                [] => Err(
                    "field `capability` is required: no default capability is registered"
                        .to_string(),
                ),
                _ => Err(format!(
                    "field `capability` is required when multiple capabilities are available: {}",
                    capability_list_for_description(&names)
                )),
            }
        }
    }
}
