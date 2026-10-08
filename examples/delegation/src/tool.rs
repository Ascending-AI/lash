//! The `spawn_agent` and `submit_error` tools.
//!
//! Examples are written in TypeScript, the RLM language (ADR 0096).

use std::sync::Arc;

use lash::persistence::FleetFormat;
use lash::plugins::PluginOptions;
use lash::process::{
    CausalRef, DeclaredProcessIdentity, Lifetime, ProcessInput, ProcessOriginator,
    ProcessStartDeclaration, SessionTurnOutcome,
};
use lash::rlm::{RLM_PROTOCOL_PLUGIN_ID, RlmCreateExtras, RlmSeed, RlmTermination};
use lash::schema::JsonSchema;
use lash::tools::{
    AttemptContext, CancelHint, DeclaredStart, ExecutionOwner, ExecutionPolicy, PendingCompletion,
    PendingToolCall, PreparedToolCall, StartProcessIntent, StaticToolExecute, StaticToolProvider,
    ToolArgumentProjectionPolicy, ToolAttemptOutcome, ToolBinding, ToolCall, ToolCallOutput,
    ToolControl, ToolDeclaration, ToolDefinition, ToolDefinitionBindingExt, ToolFailure,
    ToolFailureCause, ToolFailureClass, ToolId, ToolIntentKind, ToolOutcome, ToolPrepareContext,
    ToolProvider, ToolValue,
};
use lash::{RuntimeOwner, SessionCreateRequest, SessionId, SessionStartPoint};
use serde_json::{Value, json};

use crate::{ChildConfig, DELEGATION_PLUGIN_ID, DelegatedChild};

/// The definition key of a child's `SessionTurn` process.
pub const SESSION_TURN_DEFINITION: &str = "lash-subagent-session-turn";

/// A spawn from a host-originated process: no session parents the child.
pub const SPAWN_HOST_ORIGINATED_PROCESS: &str = "subagent_spawn_host_originated_process";

/// How many times a `spawn_agent` call's body may run: once, and again
/// after each of two crashes.
const SPAWN_ATTEMPTS: std::num::NonZeroU32 = std::num::NonZeroU32::MIN.saturating_add(2);

pub(crate) fn spawn_provider(child: Arc<ChildConfig>) -> Arc<dyn ToolProvider> {
    Arc::new(StaticToolProvider::new(
        vec![spawn_agent_tool_definition()],
        SpawnAgent { child },
    ))
}

pub(crate) fn submit_error_provider() -> Arc<dyn ToolProvider> {
    Arc::new(StaticToolProvider::new(
        vec![submit_error_tool_definition()],
        SubmitError,
    ))
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool's schemas, which admission checks"
)]
fn tool_definition(
    name: &str,
    description: impl Into<String>,
    input_schema: Value,
    output_schema: Value,
) -> ToolDefinition {
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        input_schema,
        output_schema,
    )
    .expect("valid declared tool schemas")
    // The body only declares its child's start and parks: a short bound.
    .with_execution(std::time::Duration::from_secs(30))
}

/// `spawn_agent`: run one child session on a task and answer its final
/// value.
pub fn spawn_agent_tool_definition() -> ToolDefinition {
    tool_definition(
        "spawn_agent",
        "Run one subagent on a task and return its final result. Spawns awaited together, with `Promise.all` or in one batch, run at once.\
        \n\nThe child inherits no state. Pass required context through `seed`; projected roots remain read-only projections, while computed values become writable globals.\
        \n\nA child can fail terminally through `task.fail`; this operation returns that reason as an error.",
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" },
                "output": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Optional typed result shape. Use string descriptors for record fields, e.g. `{ queries: \"list[str]\" }`."
                },
                "seed": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Optional record of state to seed into an RLM child. A host-projected root (e.g. `seed: { problem: input.prompt }`) arrives as a read-only projected binding; any other value as a regular global. The child receives nothing else from the parent."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
        json!({ "type": "object", "additionalProperties": true }),
    )
    .with_examples(vec![
        r#"typed = await agents.spawn({ task: "Find the longest line in src/main.rs", output: { line: "str", length: "int" } })?"#.to_string(),
        r#"answer = await agents.spawn({ task: "Solve sub-problem 3 using the bound problem text.", seed: { problem: input.prompt }, output: { value: "int" } })?"#.to_string(),
        // A reusable shape binding, spelled as the same descriptor shorthand
        // `output` accepts.
        r#"Shape = { name: "str", tags: "list[str]", status: "str" }"#.to_string(),
        r#"signed = await agents.spawn({ task: "Parse the book listing in data/books.json", output: Shape })?"#.to_string(),
        r#"prose = await agents.spawn({ task: "Skim the routes in api/ and flag any missing auth checks" })?"#.to_string(),
    ])
    .with_argument_projection(ToolArgumentProjectionPolicy::preserve_projected_refs_in_field(
        "seed",
    ))
    .with_tool_binding(ToolBinding::new(["agents"], "spawn"))
    .with_output_from_input_schema("output", None)
    // The child runs as a declared process start the call parks on.
    .with_declaration(ToolDeclaration::deferring().with_intents([ToolIntentKind::StartProcess]))
    // The call waits for its child, however long the child's own bounds let
    // it run, or until the delegating turn ends.
    .with_park(lash::tools::ParkBound::UntilScopeEnd)
    // The body's one effect is that start, under a key derived from the
    // call: a rerun after a crash gets the child it registered back, so the
    // call is rerun rather than settled as interrupted.
    .with_execution_policy(ExecutionPolicy::repeatable(SPAWN_ATTEMPTS, 0, 0))
}

/// `submit_error`: a delegated child ends its task as a terminal failure.
pub fn submit_error_tool_definition() -> ToolDefinition {
    let reason = json!({
        "type": "object",
        "properties": { "reason": { "type": "string" } },
        "required": ["reason"],
        "additionalProperties": false
    });
    tool_definition(
        "submit_error",
        "End the current subagent task as a terminal failure with a concise reason. Use this when the child cannot produce a valid result.",
        reason.clone(),
        reason,
    )
    .with_tool_binding(ToolBinding::new(["task"], "fail"))
}

/// What a spawn's preparation records for its body: the child's whole
/// create request, its first turn's input and the declared output schema.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PreparedSpawn {
    pub(crate) create_request: Box<SessionCreateRequest>,
    pub(crate) turn_input: lash::TurnInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_schema: Option<JsonSchema>,
}

pub(crate) struct SpawnAgent {
    pub(crate) child: Arc<ChildConfig>,
}

fn invalid(message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::err(json!(message.into()))
}

fn refusal(code: &str, message: String) -> ToolOutcome {
    ToolOutcome::from_output(ToolCallOutput::failure(ToolFailure::runtime(
        ToolFailureClass::InvalidRequest,
        code,
        message,
    )))
}

/// The session a child parents under, and what caused the spawn. A session
/// parents its own spawn; a spawn inside a process parents under the
/// session that originated the process chain. A host-originated chain names
/// no session, so its spawn is refused. The parent is a link only: nothing
/// of its state is read.
fn spawn_parent(context: &ToolPrepareContext) -> Result<(SessionId, CausalRef), ToolOutcome> {
    match context.owner() {
        RuntimeOwner::Session(session_id) => Ok((
            session_id.clone(),
            CausalRef::ToolCall {
                session_id: session_id.clone(),
                call_id: context.call_id().clone(),
            },
        )),
        RuntimeOwner::Process(process_id) => match context.process_originator() {
            Some(ProcessOriginator::Session { session_id, .. }) => Ok((
                session_id.clone(),
                CausalRef::Process {
                    process_id: process_id.clone(),
                },
            )),
            Some(ProcessOriginator::Host { .. }) | None => Err(refusal(
                SPAWN_HOST_ORIGINATED_PROCESS,
                format!(
                    "spawn_agent: process `{process_id}` was originated by the host, \
                     so no session can parent its child"
                ),
            )),
        },
    }
}

/// The first turn's input: the task, and the result contract when the call
/// states an output shape.
fn task_input(task: &str, output_schema: Option<&Value>) -> lash::TurnInput {
    let mut text = task.to_string();
    if let Some(schema) = output_schema {
        let pretty = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
        text.push_str(&format!(
            "\n\n## Required output\n\nWhen done, end the task with `finish(value)`. The value MUST match this JSON Schema exactly:\n\n```json\n{pretty}\n```"
        ));
    }
    lash::TurnInput::text(text)
}

impl SpawnAgent {
    /// The child's create request: exactly what the host configured and the
    /// call stated, recorded as a child of `parent` caused by `caused_by`.
    pub(crate) fn create_request(
        &self,
        parent: SessionId,
        caused_by: CausalRef,
        task: &str,
        output_schema: Option<JsonSchema>,
        seed: RlmSeed,
        fleet: FleetFormat,
    ) -> Result<SessionCreateRequest, ToolOutcome> {
        let mut options = PluginOptions::typed(
            DELEGATION_PLUGIN_ID,
            DelegatedChild {
                task: task.to_string(),
            },
        )
        .map_err(|error| invalid(error.to_string()))?;
        let mut initial_nodes = Vec::new();
        if self.child.rlm {
            options
                .insert_typed(
                    RLM_PROTOCOL_PLUGIN_ID,
                    RlmCreateExtras {
                        termination: Some(RlmTermination::FinishRequired {
                            schema: output_schema,
                        }),
                        render: None,
                    },
                )
                .map_err(|error| invalid(error.to_string()))?;
            initial_nodes = lash::rlm::rlm_seed_initial_nodes(seed, fleet);
        } else if !seed.is_empty() {
            return Err(invalid("spawn_agent: `seed` needs an RLM child"));
        }
        let mut request = SessionCreateRequest::child_session(
            parent,
            SessionStartPoint::Empty,
            Default::default(),
        )
        .with_spec(&self.child.spec)
        .map_err(|error| invalid(format!("the host's child spec does not resolve: {error}")))?;
        request.plugin_options = options.over(request.plugin_options);
        // The child session is the process's own, derived from the id its
        // start mints (ADR 0107), so the request names none.
        request.session_id = None;
        if let Some(plan) = self.child.prompt_plan.clone() {
            request = request.with_prompt_plan(plan);
        }
        Ok(request
            .with_tool_access(self.child.tool_access.clone())
            .with_initial_nodes(initial_nodes)
            .with_caused_by(caused_by))
    }

    async fn prepare_spawn(
        &self,
        tool_id: &ToolId,
        call: PendingToolCall,
        context: &ToolPrepareContext,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        let task = call
            .args
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or_else(|| invalid("missing required parameter: task"))?
            .to_string();
        let output_schema = lash::schema::parse_output_schema(call.args.get("output"))
            .map_err(|error| invalid(error.to_string()))?
            .map(JsonSchema::admit)
            .transpose()
            .map_err(|source| {
                ToolOutcome::failure(
                    ToolFailure::invalid_request("unusable_output_schema", source.to_string())
                        .with_cause(ToolFailureCause::SchemaAdmission { source }),
                )
            })?;
        let seed = RlmSeed::from_tool_args(&call.args).map_err(invalid)?;
        let (parent, caused_by) = spawn_parent(context)?;
        let create_request = self.create_request(
            parent,
            caused_by,
            &task,
            output_schema.clone(),
            seed,
            context.fleet_format(),
        )?;
        let payload = serde_json::to_value(PreparedSpawn {
            create_request: Box::new(create_request),
            turn_input: task_input(&task, output_schema.as_ref().map(JsonSchema::as_value)),
            output_schema,
        })
        .map_err(|error| invalid(error.to_string()))?;
        Ok(PreparedToolCall::identity(tool_id.clone(), call).with_prepared_payload(payload))
    }

    /// The body declares one `SessionTurn` start and parks on it. The child's
    /// final value, checked against the declared schema, resolves the call.
    fn execute_spawn(&self, context: &AttemptContext<'_>) -> Result<ToolAttemptOutcome, String> {
        let prepared: PreparedSpawn = context
            .decode_prepared_payload()
            .map_err(|error| format!("spawn_agent was not prepared correctly: {error}"))?;
        // The host's policy, resolved against the spawn's admitted start
        // context. The decision rides the declaration; a redrive presents the
        // same key and gets the retained child back.
        let start_cx = context.start_cx().map_err(|error| error.to_string())?;
        let lifetime = (self.child.lifetime)(&start_cx);
        let on_cancel = match &lifetime {
            Lifetime::Until(scope) if scope.id() == start_cx.starter().id() => {
                CancelHint::CancelExternalWork
            }
            _ => CancelHint::Ignore,
        };
        // A child spawned inside a running process belongs to the chain that
        // started the process; any other spawn belongs to the session that
        // authored the call, which then observes it.
        let originator = match (context.process_spawn_provenance(), context.owner()) {
            (Some(spawn), _) => spawn.originator.clone(),
            (
                None,
                ExecutionOwner::SessionFrame {
                    session_id,
                    agent_frame_id,
                },
            ) => ProcessOriginator::Session {
                session_id: session_id.clone(),
                agent_frame_id: Some(agent_frame_id.clone()),
            },
            (None, ExecutionOwner::Process { process_id }) => {
                return Err(format!(
                    "spawn_agent: process `{process_id}` carries no spawn provenance for its child"
                ));
            }
        };
        let declaration = ProcessStartDeclaration::new(
            ProcessInput::SessionTurn {
                definition_key: SESSION_TURN_DEFINITION.to_string(),
                create_request: prepared.create_request,
                turn_input: Box::new(prepared.turn_input),
                result: SessionTurnOutcome::FinalValue {
                    schema: prepared.output_schema,
                },
            },
            originator,
            lifetime,
        )
        .with_declared_identity(DeclaredProcessIdentity::labelled(
            "subagent",
            Some("spawn".to_string()),
        ))
        // The process that runs the child is built from the attempt's
        // execution environment. The child session itself records only its
        // create request.
        .with_env_ref(
            context
                .process_execution_env_ref()
                .map_err(|error| format!("spawn_agent could not capture its environment: {error}"))?,
        );
        let start = DeclaredStart::new(
            context,
            StartProcessIntent {
                owner: context.owner().runtime_owner(),
                declaration,
            },
        )
        .map_err(|error| format!("spawn_agent could not declare its child: {error}"))?;
        let mut pending = PendingCompletion::new();
        pending.on_cancel = on_cancel;
        Ok(ToolAttemptOutcome::pending(
            pending.resolved_by_declared_start(start),
        ))
    }
}

#[lash::async_trait]
impl StaticToolExecute for SpawnAgent {
    async fn prepare_tool_call(
        &self,
        tool_id: &ToolId,
        pending: PendingToolCall,
        context: &ToolPrepareContext,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        self.prepare_spawn(tool_id, pending, context).await
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        match self.execute_spawn(call.context) {
            Ok(outcome) => outcome,
            Err(error) => invalid(error).into(),
        }
    }
}

struct SubmitError;

#[lash::async_trait]
impl StaticToolExecute for SubmitError {
    /// The child's own reason is the failure's message, so the parent's
    /// spawn result and the child's process record read the child's words.
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let args = call.args.clone();
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map_or_else(
                || "the child ended its task with submit_error and no reason".to_string(),
                ToOwned::to_owned,
            );
        let mut failure =
            ToolFailure::tool(ToolFailureClass::Execution, "subagent_submit_error", reason);
        failure.raw = Some(ToolValue::untrusted_json(args.clone()));
        ToolOutcome::ok(args)
            .with_control(ToolControl::Fail { failure })
            .into()
    }
}
