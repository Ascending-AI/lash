//! The declaring process-control leaf tools: `start`, `signal`, `emit` and
//! `get`.
//!
//! Each one is an ordinary leaf tool. None of them performs its durable act in
//! the attempt body: every one declares a [`lash_core::ToolIntent`] and lets
//! the shared realization router run it behind the attempt's own commit. That
//! is what makes them redrive-safe — a body that started a child, appended an
//! event or installed a subscription before its attempt committed would leave
//! that effect behind when the attempt failed.
//!
//! A start cannot answer its process id directly: the registrar mints it when
//! the declaration is realized, after the attempt commits. The attempt answers
//! the start's result slot instead, and the realization replaces the slot with
//! the handle of the process its start key registered, so a crash redrive of
//! the attempt realizes the same start and exposes the same process (FIG-2994,
//! ADR 0107).

use serde_json::Value;

use lash_core::{
    AttemptContext, ToolAttemptOutcome, ToolDefinition, ToolIntent, ToolIntents, ToolOutcome,
    ToolOutcomeDone,
};
use lash_tool_support::{ToolBinding, ToolDefinitionBindingExt};

use crate::done_without_intents;

fn definition_property(description: &str) -> Value {
    serde_json::json!({ "type": "object", "description": description, "properties": { "id": definition_id_schema(), "signature": {"oneOf": [{"type": "object", "properties": {"signature": {"const": "unknown"}}, "required": ["signature"], "additionalProperties": false}, {"type": "object", "properties": {"signature": {"const": "known"}, "encoding": {}}, "required": ["signature", "encoding"], "additionalProperties": false}]} }, "required": ["id", "signature"], "additionalProperties": false })
}

pub fn definition_id_schema() -> Value {
    serde_json::json!({ "type": "object", "properties": { "$lash_definition_id": { "type": "string", "pattern": "^lash\\.definition:sha256:[0-9a-f]{64}$" } }, "required": ["$lash_definition_id"], "additionalProperties": false })
}

pub fn process_start_tool_definition() -> ToolDefinition {
    ToolDefinition::raw("tool:start_process", "start_process", "Start a durable process by immutable definition or tagged definition ID and return its handle.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "definition": definition_property("The immutable definition returned by create or get."),
                "definition_id": definition_id_schema(),
                "args": { "type": "object" },
                "label": { "type": "string" }
            },
            "oneOf": [{"required": ["definition"], "not": {"required": ["definition_id"]}}, {"required": ["definition_id"], "not": {"required": ["definition"]}}],
            "additionalProperties": false
        }),
        serde_json::json!({"x-lash": {"kind": "process_unknown"}}))
        .with_tool_binding(ToolBinding::new(["processes"], "start"))
}

pub fn process_get_tool_definition() -> ToolDefinition {
    ToolDefinition::raw("tool:get_process_definition", "get_process_definition", "Resolve a tagged definition ID and retain its definition in this execution.",
        serde_json::json!({"type": "object", "properties": {"definition_id": definition_id_schema()}, "required": ["definition_id"], "additionalProperties": false}),
        serde_json::json!({"type": "object", "properties": {"id": definition_id_schema(), "signature": {}}, "required": ["id", "signature"], "additionalProperties": false}))
        .with_tool_binding(ToolBinding::new(["processes"], "get"))
}

pub fn execute_process_get_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
) -> ToolAttemptOutcome {
    let Ok(map) = exact_fields(args, &["definition_id"]) else {
        return refuse("get requires only definition_id");
    };
    let Some(value) = map.get("definition_id") else {
        return refuse("get requires definition_id");
    };
    let id = match lash_core::ProcessDefinitionId::from_tagged_json(value) {
        Ok(id) => id,
        Err(error) => return refuse(error),
    };
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(lash_sansio::handle::definition_slot_json(0)),
        ToolIntents::v3(vec![ToolIntent::GetDefinition(
            lash_core::GetDefinitionIntent {
                owner: context.owner().runtime_owner(),
                definition_id: id,
            },
        )]),
    )
}

fn exact_fields<'a>(
    value: &'a Value,
    fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let map = value
        .as_object()
        .ok_or_else(|| "arguments must be an object".to_owned())?;
    if let Some(key) = map.keys().find(|key| !fields.contains(&key.as_str())) {
        return Err(format!("unknown field `{key}`"));
    }
    Ok(map)
}

/// `processes.signal(handle, name, payload)` — deliver a named signal.
pub fn process_signal_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:signal_process",
        "signal_process",
        "Deliver a named signal to a running durable process. The signal is durable and is delivered once, even if the signalling turn restarts.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "x-lash": { "kind": "process_unknown" },
                    "description": "Process handle to signal, as returned by `processes.list(...)`.",
                },
                "name": {
                    "type": "string",
                    "description": "Signal name the target process waits on.",
                },
                "payload": {
                    "description": "Signal payload, validated against the target's event schema.",
                },
            },
            "required": ["handle", "name"],
            "additionalProperties": false
        }),
        serde_json::json!({ "description": "The recorded signal event." }),
    )
    .with_examples(vec![
        r#"await processes.signal({ handle: h, name: "approved", payload: { by: "sam" } })?"#.into(),
    ])
    .with_tool_binding(ToolBinding::new(["processes"], "signal"))
}

/// `processes.emit(value)` — append progress to the *enclosing* process.
///
/// Run-only by construction: the event it appends belongs to the process the
/// caller is running inside, and a cell has no such process. That refusal is
/// the tool's contract, not a missing capability, so it is refused with a typed
/// reason rather than silently appending nowhere.
pub fn process_emit_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:emit_process_event",
        "emit_process_event",
        "Append a progress value to the event journal of the process this call is running inside. Only available inside a durable process; a call from a cell is refused.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": {
                    "description": "Progress value to append.",
                },
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "description": "The appended process event." }),
    )
    .with_examples(vec![
        r#"await processes.emit({ value: { stage: "approved" } })?"#.into(),
    ])
    .with_tool_binding(ToolBinding::new(["processes"], "emit"))
}

fn required_object_field<'a>(args: &'a Value, field: &str) -> Result<&'a Value, String> {
    args.get(field)
        .ok_or_else(|| format!("`{field}` is required"))
}

/// The host-facing label a start declares, when it declares one.
///
/// A present-but-unusable label is refused rather than dropped: the argument is
/// documented, so silently ignoring a non-string or blank value would reproduce
/// the defect this plumbing fixes.
fn start_label(args: &Value) -> Result<Option<String>, String> {
    match args.get("label") {
        None => Ok(None),
        Some(Value::String(label)) => {
            let label = label.trim();
            if label.is_empty() {
                Err("`label` must be a non-empty string".to_string())
            } else {
                Ok(Some(label.to_string()))
            }
        }
        Some(_) => Err("`label` must be a string".to_string()),
    }
}

fn refuse(message: impl std::fmt::Display) -> ToolAttemptOutcome {
    done_without_intents(ToolOutcome::err_fmt(format_args!("{message}")))
}

pub async fn execute_process_start_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
    lifetime: &lash_core::LifetimePolicy,
) -> ToolAttemptOutcome {
    let fields = match exact_fields(args, &["definition", "definition_id", "args", "label"]) {
        Ok(fields) => fields,
        Err(error) => return refuse(error),
    };
    let target = match (fields.get("definition"), fields.get("definition_id")) {
        (Some(value), None) => {
            match serde_json::from_value::<lash_core::ProcessDefinition>(value.clone()) {
                Ok(definition) => lash_core::ProcessDefinitionTarget::Definition(definition),
                Err(error) => return refuse(error),
            }
        }
        (None, Some(value)) => match lash_core::ProcessDefinitionId::from_tagged_json(value) {
            Ok(id) => lash_core::ProcessDefinitionTarget::DefinitionId(id),
            Err(error) => return refuse(error),
        },
        _ => return refuse("exactly one of definition or definition_id is required"),
    };
    let run_args = match fields.get("args") {
        None => serde_json::Map::new(),
        Some(Value::Object(args)) => args.clone(),
        Some(_) => return refuse("args must be an object"),
    };
    let identity = context.intent_identity(0);
    // The lifetime is the host's policy resolved against this attempt's
    // admitted start context, never the model's choice, and the declaration
    // journals the decision so realization never re-runs the policy
    // (FIG-3607 R4b).
    let cx = match context.start_cx() {
        Ok(cx) => cx,
        Err(error) => return refuse(error),
    };
    let lifetime = lifetime(&cx);
    let owner = context.owner().runtime_owner();
    // A child started from inside a running process belongs to the chain that
    // started that process, not to the ephemeral session the run executes in:
    // it inherits the chain's originator and its wake target, and the execution
    // scope never reaches a record. The in-attempt start path has always read
    // this off the runtime execution context; since ADR 0095 a start is a leaf
    // tool, so the declaration is where the inheritance has to be stamped —
    // without it a process's children are owned by (and observed from) a
    // session that disappears when the run ends.
    // A start that is *not* inside a chain is a session start: the session that
    // authored the call is both its originator and its wake target, exactly as
    // the in-session start path stamps it
    // (`lash_core::runtime::session_manager::process_runners::control`). Leaving
    // the wake target unset here registers a process whose declared wakes are
    // materialized and then dropped for want of a delivery target, so a
    // `processes.emit` from that process never becomes queued work on the
    // session waiting for it.
    let spawn = context.process_spawn_provenance().cloned();
    let (originator, wake_session_id) = match (spawn, context.owner()) {
        (Some(spawn), _) => (spawn.originator, spawn.wake_session_id),
        (
            None,
            lash_core::ExecutionOwner::SessionFrame {
                session_id,
                agent_frame_id,
            },
        ) => (
            lash_core::ProcessOriginator::Session {
                session_id: session_id.clone(),
                agent_frame_id: Some(agent_frame_id.clone()),
            },
            Some(session_id.clone()),
        ),
        (None, lash_core::ExecutionOwner::Process { process_id }) => {
            return refuse(format!(
                "process `{process_id}` carries no spawn provenance for the child it starts"
            ));
        }
    };
    let declaration = lash_core::ProcessStartDeclaration::new(
        lash_core::ProcessInput::Definition {
            definition_id: target.definition_id().clone(), args: run_args,
            signature_claim: Some(target.signature_claim().clone()),
        },
        originator,
        lifetime,
    )
    .with_wake_session_id(wake_session_id)
    // An engine start is admitted against the execution env its own record
    // carries, never against the live session env, so the declaration captures
    // the attempt's environment digest here. The coordinator stores the bytes
    // before journaling, and realization loads those exact bytes (FIG-2999).
    .with_env_ref(match context.process_execution_env_ref() {
        Ok(env_ref) => env_ref,
        Err(error) => return refuse(error),
    });
    // The documented `label` argument: a host-facing name for this run, never
    // part of the process's identity (FIG-3122). Declaring it here is the only
    // way it reaches the row — an engine derives its own label from the
    // payload, and for Lashlang that is the lift digest, so a run the author
    // named `probe` lists as `__process_<hash>` unless the start declares the
    // name. A start that passes no label keeps the engine's derived one.
    let declaration = match start_label(args) {
        Ok(None) => declaration,
        Ok(Some(label)) => declaration.with_declared_identity(
            lash_core::DeclaredProcessIdentity::labelled("definition", Some(label)),
        ),
        Err(message) => return refuse(message),
    };
    // The process id is minted when the declared start is realized, after
    // this attempt seals its output, so the attempt answers the start's result
    // slot and the realization replaces it with the handle of the process the
    // start registered (ADR 0107).
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(lash_sansio::handle::process_start_slot_json(
            identity.intent_index,
        )),
        ToolIntents::v3(vec![ToolIntent::StartProcess(Box::new(
            lash_core::StartProcessIntent { owner, declaration },
        ))]),
    )
}

pub fn execute_process_signal_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
) -> ToolAttemptOutcome {
    let handle = match required_object_field(args, "handle") {
        Ok(value) => value,
        Err(message) => return refuse(message),
    };
    let process_id = match lash_core::process_id_from_handle_json(handle) {
        Ok(process_id) => process_id,
        Err(message) => return refuse(message),
    };
    let Some(signal_name) = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return refuse("signal_process requires a non-empty `name`");
    };
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(serde_json::json!({
            "process_id": process_id,
            "signal": signal_name,
        })),
        ToolIntents::v3(vec![ToolIntent::SignalProcess(
            lash_core::SignalProcessIntent {
                owner: context.owner().runtime_owner(),
                process_id,
                signal_name: signal_name.to_string(),
                payload: args.get("payload").cloned().unwrap_or(Value::Null),
            },
        )]),
    )
}

/// The event type a progress emission appends under.
const PROCESS_PROGRESS_EVENT_TYPE: &str = "process.yield";

pub fn execute_process_emit_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
) -> ToolAttemptOutcome {
    let Some(process_id) = context.enclosing_process() else {
        return refuse(
            "emit_process_event appends to the process it runs inside, and this call is not \
             running inside a durable process",
        );
    };
    let value = match required_object_field(args, "value") {
        Ok(value) => value.clone(),
        Err(message) => return refuse(message),
    };
    let process_id = process_id.clone();
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(serde_json::json!({
            "process_id": process_id,
            "event_type": PROCESS_PROGRESS_EVENT_TYPE,
        })),
        ToolIntents::v3(vec![ToolIntent::EmitProcessEvent(
            lash_core::EmitProcessEventIntent {
                owner: context.owner().runtime_owner(),
                process_id,
                event_type: PROCESS_PROGRESS_EVENT_TYPE.to_string(),
                payload: value,
            },
        )]),
    )
}

#[cfg(test)]
#[path = "declarations_tests.rs"]
mod declarations_tests;
