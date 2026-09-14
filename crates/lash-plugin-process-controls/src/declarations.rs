//! The declaring process-control leaf tools: `start`, `signal`, `emit` and
//! `register`.
//!
//! Each one is an ordinary leaf tool. None of them performs its durable act in
//! the attempt body: every one declares a [`lash_core::ToolIntent`] and lets
//! the shared realization router run it behind the attempt's own commit. That
//! is what makes them redrive-safe — a body that started a child, appended an
//! event or installed a subscription before its attempt committed would leave
//! that effect behind when the attempt failed.
//!
//! The one thing an attempt *may* answer with directly is the process id, which
//! [`lash_core::ProcessId::from_intent_identity`] derives from the declaring
//! attempt's own intent identity. The executor derives the same id from the
//! committed declaration, so the id this tool returns before commit and the id
//! the registry holds after a crash redrive are the same value (FIG-2994).

use serde_json::Value;

use lash_core::{
    AttemptContext, ProcessId, SessionId, ToolAttemptOutcome, ToolDefinition, ToolIntent,
    ToolIntents, ToolOutcome, ToolOutcomeDone,
};
use lash_tool_support::{ToolBinding, ToolDefinitionBindingExt};

use crate::done_without_intents;

/// The engine a start or registration names when the caller does not say.
///
/// A definition value is engine-owned bytes: nothing in it says which engine
/// owns it, so the engine kind is an argument rather than something this plugin
/// can infer. The default is the one engine a stock runtime configures; a
/// third-party plugin contributing its own engine passes its own kind, and an
/// engine this host never registered is refused at realization by the engine
/// registry rather than guessed at here.
pub const DEFAULT_PROCESS_ENGINE_KIND: &str = "lashlang";

/// The process value a definition argument carries, as `x-lash` says it.
///
/// `process_unknown` rather than `handle`: `handle` is the *trigger* handle
/// kind and requires the payload its trigger delivers, which is a different
/// type and would refuse a process value. The host has no authoritative call
/// signature for an arbitrary caller-supplied definition, so it describes the
/// argument as a process it can only say is callable. The engine that owns the
/// definition supplies the authority at admission.
fn definition_property(description: &str) -> Value {
    serde_json::json!({
        "x-lash": { "kind": "process_unknown" },
        "description": description,
    })
}

fn engine_property() -> Value {
    serde_json::json!({
        "type": "string",
        "description": "Engine that owns the definition value. Defaults to the stock engine; pass the kind a third-party engine registered under.",
    })
}

/// `processes.start(definition, args)` — declare a durable child start and
/// answer with the id it will have.
pub fn process_start_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:start_process",
        "start_process",
        "Start a durable process from a process definition value and return its id. The start is durable: it survives a restart of the starting turn, and the id returned here is the id the process registry holds afterwards.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "definition": definition_property(
                    "The process to start, for example `on_button`: pass the process itself.",
                ),
                "args": {
                    "type": "object",
                    "description": "Arguments for the process's declared parameters. Defaults to no arguments.",
                },
                "engine": engine_property(),
                "label": {
                    "type": "string",
                    "description": "Host-facing label for the started run. Never part of the process's identity.",
                },
            },
            "required": ["definition"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the started process." },
                "process_id": { "type": "string", "description": "Same id, repeated for tools that ask for `process_id`." }
            },
            "required": ["id", "process_id"],
            "additionalProperties": false
        }),
    )
    .with_examples(vec![
        "await processes.start({ definition: on_button, args: { request: r } })?".into(),
    ])
    .with_tool_binding(ToolBinding::new(["processes"], "start"))
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

/// `processes.register(name, definition)` — install a name for a definition.
pub fn process_register_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:register_process",
        "register_process",
        "Register a process definition under a name so later starts and trigger registrations can select it by that name.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name to register the definition under, unique within the owning scope.",
                },
                "definition": definition_property(
                    "The process to register, for example `on_button`: pass the process itself.",
                ),
                "engine": engine_property(),
            },
            "required": ["name", "definition"],
            "additionalProperties": false
        }),
        serde_json::json!({ "description": "The registered definition." }),
    )
    .with_examples(vec![
        r#"await processes.register({ name: "approval", definition: on_button })?"#.into(),
    ])
    .with_tool_binding(ToolBinding::new(["processes"], "register"))
}

/// The engine start payload a definition value and its arguments make.
///
/// A definition value is the engine's own encoding of "which definition", and
/// an engine start payload is that same encoding plus the arguments for this
/// run. Building it here by adding `args` keeps the plugin engine-agnostic:
/// nothing in this crate knows the shape of any engine's definition value, and
/// the engine's own admission is what reads the result and refuses a payload it
/// does not recognise.
fn engine_start_payload(definition: &Value, args: Option<&Value>) -> Result<Value, String> {
    let Value::Object(fields) = definition else {
        return Err("`definition` must be a process definition value".to_string());
    };
    let mut payload = fields.clone();
    let args = match args {
        None => serde_json::Map::new(),
        Some(Value::Object(args)) => args.clone(),
        Some(_) => return Err("`args` must be an object".to_string()),
    };
    payload.insert("args".to_string(), Value::Object(args));
    Ok(Value::Object(payload))
}

fn required_object_field<'a>(args: &'a Value, field: &str) -> Result<&'a Value, String> {
    args.get(field)
        .ok_or_else(|| format!("`{field}` is required"))
}

fn engine_kind(args: &Value) -> String {
    args.get("engine")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .unwrap_or(DEFAULT_PROCESS_ENGINE_KIND)
        .to_string()
}

fn refuse(message: impl std::fmt::Display) -> ToolAttemptOutcome {
    done_without_intents(ToolOutcome::err_fmt(format_args!("{message}")))
}

/// Declares the start and answers with the id the declaration derives.
pub async fn execute_process_start_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
) -> ToolAttemptOutcome {
    let definition = match required_object_field(args, "definition") {
        Ok(value) => value,
        Err(message) => return refuse(message),
    };
    let payload = match engine_start_payload(definition, args.get("args")) {
        Ok(payload) => payload,
        Err(message) => return refuse(message),
    };
    let identity = match context.intent_identity(0) {
        Ok(identity) => identity,
        Err(reason) => {
            return refuse(format!(
                "start_process cannot declare an intent: {reason:?}"
            ));
        }
    };
    let parent = match context.child_process_parent_scope().await {
        Ok(parent) => parent,
        Err(error) => return refuse(error),
    };
    let session_id = SessionId::from(context.session_id());
    let declaration = lash_core::ProcessStartDeclaration::new(
        lash_core::ProcessInput::Engine {
            kind: engine_kind(args),
            payload,
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessOriginator::Session {
            session_id: session_id.clone(),
            agent_frame_id: Some(context.agent_frame_id().clone()),
        },
        lash_core::ProcessLifecyclePolicy::new(parent, lash_core::OnParentEnd::Abandon),
    );
    let process_id = ProcessId::from_intent_identity(&identity);
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(serde_json::json!({
            "id": process_id,
            "process_id": process_id,
        })),
        ToolIntents::v3(vec![ToolIntent::StartProcess(Box::new(
            lash_core::StartProcessIntent {
                session_id,
                declaration,
            },
        ))]),
    )
}

/// Declares the signal against the process the handle names.
pub fn execute_process_signal_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
) -> ToolAttemptOutcome {
    let handle = match required_object_field(args, "handle") {
        Ok(value) => value,
        Err(message) => return refuse(message),
    };
    let process_ref = match lash_core::ProcessRef::from_handle_json(handle) {
        Ok(process_ref) => process_ref,
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
            "process_id": process_ref.process_id,
            "signal": signal_name,
        })),
        ToolIntents::v3(vec![ToolIntent::SignalProcess(
            lash_core::SignalProcessIntent {
                session_id: SessionId::from(context.session_id()),
                process_id: process_ref.process_id,
                signal_name: signal_name.to_string(),
                payload: args.get("payload").cloned().unwrap_or(Value::Null),
            },
        )]),
    )
}

/// The event type a progress emission appends under.
const PROCESS_PROGRESS_EVENT_TYPE: &str = "process.yield";

/// Declares the append against the process this call runs inside.
pub fn execute_process_emit_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
) -> ToolAttemptOutcome {
    let Some(process_id) = context.runtime_process_id() else {
        return refuse(
            "emit_process_event appends to the process it runs inside, and this call is not \
             running inside a durable process",
        );
    };
    let value = match required_object_field(args, "value") {
        Ok(value) => value.clone(),
        Err(message) => return refuse(message),
    };
    let process_id = ProcessId::from(process_id.to_string());
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(serde_json::json!({
            "process_id": process_id,
            "event_type": PROCESS_PROGRESS_EVENT_TYPE,
        })),
        ToolIntents::v3(vec![ToolIntent::EmitProcessEvent(
            lash_core::EmitProcessEventIntent {
                session_id: SessionId::from(context.session_id()),
                process_id,
                event_type: PROCESS_PROGRESS_EVENT_TYPE.to_string(),
                payload: value,
            },
        )]),
    )
}

/// Declares the registration.
///
/// The definition-name registry is a separate child of the arc (FIG-2995). The
/// declaration is admitted, identified and journaled like any other intent
/// today, and its realization refuses with the shared typed
/// `process_definition_registry_unavailable` reason rather than reporting a
/// registration that never happened. The tool exists ahead of its table because
/// the declaration shape is what it binds against.
pub fn execute_process_register_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
) -> ToolAttemptOutcome {
    let Some(name) = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return refuse("register_process requires a non-empty `name`");
    };
    let definition = match required_object_field(args, "definition") {
        Ok(value) => value.clone(),
        Err(message) => return refuse(message),
    };
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(serde_json::json!({ "name": name })),
        ToolIntents::v3(vec![ToolIntent::RegisterProcessDefinition(Box::new(
            lash_core::RegisterProcessDefinitionIntent {
                session_id: SessionId::from(context.session_id()),
                engine_kind: engine_kind(args),
                definition,
                env_spec: None,
                label: Some(name.to_string()),
            },
        ))]),
    )
}

#[cfg(test)]
#[path = "declarations_tests.rs"]
mod declarations_tests;
