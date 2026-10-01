//! RLM protocol subagent spawning surface.
//!
//! Examples are written in TypeScript, the sole RLM language (ADR 0096).
//! Prompt prose is tuned for schema-first results and binding subagent output.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lash_core::{
    PreparedToolCall, SessionToolAccess, SubagentSessionContext, ToolArgumentProjectionPolicy,
    ToolCall, ToolDefinition, ToolOutcome, ToolPrepareContext, facade_support::SessionSpec,
    sansio::PendingToolCall,
};
use lash_lashlang_runtime::ToolDefinitionBindingExt;
use lash_tool_support::{StaticToolExecute, StaticToolProvider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::{CapabilityRegistry, ChildPluginSource};
use crate::rlm_support::{
    self, SpawnCreateRequestInput, build_spawn_create_request, capability_list_for_description,
    example_capability_name, finalise_tool_result, render_task_prompt, required_string,
    spawn_agent_input_schema, tool_definition, turn_input_for_task, unknown_capability_message,
};

pub(crate) struct RlmSubagentToolsProvider {
    pub(crate) registry: Arc<CapabilityRegistry>,
    pub(crate) session_spec: SessionSpec,
    pub(crate) tool_access: SessionToolAccess,
    pub(crate) final_answer_format: lash_rlm_types::RlmFinalAnswerFormat,
    /// The host's policy for a spawned child's lifetime (FIG-3607).
    pub(crate) lifetime: lash_core::LifetimePolicy,
    pub(crate) parent_subagent: Option<SubagentSessionContext>,
    pub(crate) include_submit_error: bool,
    /// How long a spawn waits for its child. A spawn that times out is an
    /// error result, and the runtime cancels the child.
    pub(crate) timeout: Option<Duration>,
}

impl RlmSubagentToolsProvider {
    pub(crate) fn into_provider(self) -> StaticToolProvider<Self> {
        let definitions = self.tool_definitions();
        StaticToolProvider::new(definitions, self)
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = vec![spawn_agent_tool_definition(&self.registry.names())];
        if self.include_submit_error {
            definitions.push(rlm_support::submit_error_tool_definition());
        }
        definitions
    }

    async fn prepare_spawn_agent(
        &self,
        tool_id: &lash_core::ToolId,
        call: PendingToolCall,
        context: &ToolPrepareContext,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        let args = &call.args;
        let task = required_string(args, "task")
            .map_err(|err| ToolOutcome::err(serde_json::json!(err)))?;
        let capability_name = capability_name_from_args(args, &self.registry)
            .map_err(|err| ToolOutcome::err(serde_json::json!(err)))?;
        let capability = self.registry.get(&capability_name).ok_or_else(|| {
            ToolOutcome::err(serde_json::json!(unknown_capability_message(
                &capability_name,
                &self.registry
            )))
        })?;
        let plugin_source = capability.plugin_source();
        let output_schema = lash_sansio::schema_contract::parse_output_schema(args.get("output"))
            .map_err(|err| ToolOutcome::err(serde_json::json!(err.to_string())))?;
        let output_schema = output_schema
            .map(lash_sansio::JsonSchema::admit)
            .transpose()
            .map_err(|source| {
                ToolOutcome::failure(
                    lash_core::ToolFailure::invalid_request(
                        "unusable_output_schema",
                        source.to_string(),
                    )
                    .with_cause(lash_core::ToolFailureCause::SchemaAdmission { source }),
                )
            })?;
        let seed = lash_protocol_rlm::RlmSeed::from_tool_args(args)
            .map_err(|err| ToolOutcome::err(serde_json::json!(err)))?;
        let parent = spawn_parent(context)?;
        let current_snapshot = context
            .snapshot_session(&parent.session_id)
            .await
            .map_err(|err| ToolOutcome::err(serde_json::json!(err.to_string())))?;
        let parent_session_id = parent.session_id;
        let mut create_request = build_spawn_create_request(SpawnCreateRequestInput {
            registry: &self.registry,
            parent_session_id: &parent_session_id,
            current_snapshot,
            session_spec: &self.session_spec,
            tool_access: &self.tool_access,
            final_answer_format: self.final_answer_format.clone(),
            capability_name: &capability_name,
            output_schema: output_schema.clone(),
            seed,
            parent_subagent: self.parent_subagent.as_ref(),
            caused_by: Some(parent.caused_by),
        })
        .map_err(|err| ToolOutcome::err(serde_json::json!(err)))?;
        // A `ParentFork` peer initializes from this spawn-time capture alone;
        // it is journaled inside the durable creation request so a worker
        // restart rebuilds the child identically without reading the parent.
        create_request.plugin_source = match plugin_source {
            ChildPluginSource::CurrentHostFresh => lash_core::SessionPluginSource::CurrentHostFresh,
            ChildPluginSource::ParentFork => {
                if let lash_core::RuntimeOwner::Process(process_id) = context.owner() {
                    return Err(spawn_refusal(
                        SPAWN_PARENT_FORK_IN_PROCESS,
                        format!(
                            "spawn_agent: a `ParentFork` capability forks its parent's conversation, \
                         and process `{process_id}` has none to fork"
                        ),
                    ));
                }
                let plugin_init = context
                    .session_plugin_init()
                    .await
                    .map_err(|err| ToolOutcome::err(serde_json::json!(err.to_string())))?;
                lash_core::SessionPluginSource::ParentFork(plugin_init)
            }
        };
        // The child session is the process's own, derived from the id its
        // start mints (ADR 0107), so the request names none.
        create_request.session_id = None;
        let turn_input = turn_input_for_task(render_task_prompt(
            &task,
            output_schema
                .as_ref()
                .map(lash_sansio::JsonSchema::as_value),
        ));
        let payload = serde_json::to_value(PreparedSpawnAgent {
            create_request: Box::new(create_request),
            turn_input,
            output_schema,
        })
        .map_err(|err| ToolOutcome::err(serde_json::json!(err.to_string())))?;
        Ok(PreparedToolCall::identity(tool_id.clone(), call).with_prepared_payload(payload))
    }

    /// A subagent spawn *is* a process that runs a child lash session. The
    /// body declares that one `ProcessInput::SessionTurn` start and parks on
    /// it; the runtime launches the start, and the child's final value — the
    /// SessionTurn runner's projection under `SessionTurnOutcome::FinalValue` —
    /// resolves the call. Going through the process worker gives the child
    /// durability and makes it recoverable, under the environment its start
    /// captured, the same generic path every other session turn takes.
    fn execute_spawn_agent(
        &self,
        context: &lash_core::AttemptContext<'_>,
    ) -> Result<lash_core::ToolAttemptOutcome, String> {
        let prepared: PreparedSpawnAgent = context
            .decode_prepared_payload()
            .map_err(|err| format!("spawn_agent was not prepared correctly: {err}"))?;
        // The host's policy, resolved against the spawn's admitted start
        // context. The decision rides the declaration; a redrive presents the
        // same key and gets the retained child back.
        let lifetime = (self.lifetime)(&context.start_cx().map_err(|err| err.to_string())?);
        let owner = context.owner().runtime_owner();
        // A child spawned from inside a running process belongs to the chain
        // that started the process; any other spawn belongs to the session
        // that authored the call, which then observes it.
        let originator = match (context.process_spawn_provenance(), context.owner()) {
            (Some(spawn), _) => spawn.originator.clone(),
            (
                None,
                lash_core::ExecutionOwner::SessionFrame {
                    session_id,
                    agent_frame_id,
                },
            ) => lash_core::ProcessOriginator::Session {
                session_id: session_id.clone(),
                agent_frame_id: Some(agent_frame_id.clone()),
            },
            (None, lash_core::ExecutionOwner::Process { process_id }) => {
                return Err(format!(
                    "spawn_agent: process `{process_id}` carries no spawn provenance for its child"
                ));
            }
        };
        let declaration = lash_core::ProcessStartDeclaration::new(
            lash_core::ProcessInput::SessionTurn {
                definition_key: SUBAGENT_SESSION_TURN_DEFINITION.to_string(),
                create_request: prepared.create_request,
                turn_input: Box::new(prepared.turn_input),
                result: lash_core::SessionTurnOutcome::FinalValue {
                    schema: prepared.output_schema,
                },
            },
            originator,
            lifetime,
        )
        .with_declared_identity(lash_core::DeclaredProcessIdentity::labelled(
            "subagent",
            Some("spawn".to_string()),
        ))
        // The child runs under its parent's recorded facts: the start captures
        // the attempt's environment — the parent's recorded policy and plugin
        // config — and the worker builds the child's process runtime from it
        // (FIG-4396).
        .with_env_ref(
            context
                .process_execution_env_ref()
                .map_err(|err| format!("spawn_agent could not capture its environment: {err}"))?,
        );
        let start = lash_core::DeclaredStart::new(
            context,
            lash_core::StartProcessIntent { owner, declaration },
        )
        .map_err(|err| format!("spawn_agent could not declare its child: {err}"))?;
        let mut pending = lash_core::PendingCompletion::new();
        if let Some(timeout) = self.timeout {
            pending = pending.with_deadline(timeout);
        }
        Ok(lash_core::ToolAttemptOutcome::pending(
            pending.resolved_by_declared_start(start),
        ))
    }
}

/// The session a spawned child parents under, and what caused the spawn.
struct SpawnParent {
    session_id: lash_core::SessionId,
    caused_by: lash_core::CausalRef,
}

/// A spawn from a session parents its child under that session. A spawn
/// from inside a durable process parents it under the session that
/// originated the process chain: the process has no session of its own and
/// never stands one in. A chain a host originated names no session, so its
/// spawn is refused.
fn spawn_parent(context: &ToolPrepareContext) -> Result<SpawnParent, ToolOutcome> {
    match context.owner() {
        lash_core::RuntimeOwner::Session(session_id) => Ok(SpawnParent {
            session_id: session_id.clone(),
            caused_by: lash_core::CausalRef::ToolCall {
                session_id: session_id.clone(),
                call_id: context.call_id().clone(),
            },
        }),
        lash_core::RuntimeOwner::Process(process_id) => match context.process_originator() {
            Some(lash_core::ProcessOriginator::Session { session_id, .. }) => Ok(SpawnParent {
                session_id: session_id.clone(),
                caused_by: lash_core::CausalRef::Process {
                    process_id: process_id.clone(),
                },
            }),
            Some(lash_core::ProcessOriginator::Host { .. }) | None => Err(spawn_refusal(
                SPAWN_HOST_ORIGINATED_PROCESS,
                format!(
                    "spawn_agent: process `{process_id}` was originated by the host, \
                     so no session can parent its subagent"
                ),
            )),
        },
    }
}

/// A spawn from a host-originated process: no session parents the child.
pub const SPAWN_HOST_ORIGINATED_PROCESS: &str = "subagent_spawn_host_originated_process";
/// A `ParentFork` spawn from a process: there is no conversation to fork.
pub const SPAWN_PARENT_FORK_IN_PROCESS: &str = "subagent_spawn_parent_fork_in_process";

fn spawn_refusal(code: &str, message: String) -> ToolOutcome {
    ToolOutcome::from_output(lash_core::ToolCallOutput::failure(
        lash_core::ToolFailure::runtime(lash_core::ToolFailureClass::InvalidRequest, code, message),
    ))
}

/// The definition key of a spawned child's SessionTurn process.
const SUBAGENT_SESSION_TURN_DEFINITION: &str = "lash-subagent-session-turn";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PreparedSpawnAgent {
    create_request: Box<lash_core::SessionCreateRequest>,
    turn_input: lash_core::TurnInput,
    /// The caller's declared output schema, parsed once at prepare. The
    /// SessionTurn runner checks the child's final value against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_schema: Option<lash_sansio::JsonSchema>,
}

#[async_trait]
impl StaticToolExecute for RlmSubagentToolsProvider {
    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id.as_str() == SPAWN_AGENT_TOOL_ID
    }

    async fn prepare_tool_call(
        &self,
        tool_id: &lash_core::ToolId,
        pending: PendingToolCall,
        context: &ToolPrepareContext,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        match pending.tool_name.as_str() {
            "spawn_agent" => self.prepare_spawn_agent(tool_id, pending, context).await,
            _ => Ok(PreparedToolCall::identity(tool_id.clone(), pending)),
        }
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let result = match call.name() {
            "spawn_agent" => match self.execute_spawn_agent(call.context) {
                Ok(outcome) => return outcome,
                Err(err) => Err(err),
            },
            "submit_error" => return rlm_support::submit_error_tool_result(call.args).into(),
            other => Err(format!("Unknown tool: {other}")),
        };
        finalise_tool_result(result).into()
    }
}

/// The manifest id of `spawn_agent`.
const SPAWN_AGENT_TOOL_ID: &str = "tool:spawn_agent";

#[cfg(test)]
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
                r#"research = async (task: string) => {{
  result = await agents.spawn({{ task: task{capability_arg}, output: {{ summary: "str" }} }})?
  return result.summary
}}
first = await processes.start({{ definition: research, args: {{ task: "Research the first topic" }} }})?
second = await processes.start({{ definition: research, args: {{ task: "Research the second topic" }} }})?
finish {{ first: await first, second: await second }}"#
            ),
            // Schema-first: the highest-leverage shape — bind a typed result.
            format!(
                r#"typed = await agents.spawn({{ task: "Find the longest line in src/main.rs"{capability_arg}, output: {{ line: "str", length: "int" }} }})?"#
            ),
            format!(
                r#"queries = await agents.spawn({{ task: "Generate two focused web search queries"{capability_arg}, output: {{ queries: "list[str]" }} }})?"#
            ),
            // A reusable shape binding, spelled as the same descriptor
            // shorthand `output` accepts.
            r#"Shape = { name: "str", tags: "list[str]", status: "str" }"#.into(),
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
        "Run one subagent and return its final result. Spawns awaited together, with `Promise.all` or in one batch, run at once. {capability_detail} Available capabilities: {cap_list}. \
        \n\nThe child inherits no state. Pass required context through `seed`; projected roots remain read-only projections, while computed values become writable globals. Projected seeds require an RLM child.\
        \n\nA child can fail terminally through `task.fail`; this operation returns that reason as an error."
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
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["agents"], "spawn"))
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

#[cfg(test)]
#[path = "outcome_tests.rs"]
mod outcome_tests;
#[cfg(test)]
#[path = "spawn_parent_tests.rs"]
mod spawn_parent_tests;
