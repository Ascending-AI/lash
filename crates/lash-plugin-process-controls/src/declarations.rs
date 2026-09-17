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
/// answer with the handle it will be held by.
pub fn process_start_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:start_process",
        "start_process",
        "Start a durable process from a process definition value and return a handle to it. The start is durable: it survives a restart of the starting turn, and the handle returned here names the process the registry holds afterwards.",
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
        // The answer *is* the handle, so it is typed as the one process type
        // rather than as the record that carries it: a start whose result read
        // as a plain object could not be handed back to `await`, `signal` or
        // `cancel`, which is the whole point of holding it. The record's own
        // fields are `__handle__` and the opaque `id`, plus the `process_id`
        // the process tools take; the id is opaque to the cell and is never
        // parsed or built by guest code (ADR 0095).
        serde_json::json!({
            "x-lash": { "kind": "process_unknown" },
            "description": "Handle to the started process: await it for the result, or pass it to `processes.signal`, `processes.cancel` or `processes.await`.",
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

/// The host-facing label a start declares, when it declares one.
///
/// A present-but-unusable label is refused rather than dropped: the argument is
/// documented, so silently ignoring a non-string or blank value would reproduce
/// the defect this plumbing fixes.
fn start_label(args: &Value) -> Result<Option<String>, String> {
    match args.get("label") {
        None | Some(Value::Null) => Ok(None),
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
    let (originator, wake_session_id) = match spawn {
        Some(spawn) => (spawn.originator, spawn.wake_session_id),
        None => (
            lash_core::ProcessOriginator::Session {
                session_id: session_id.clone(),
                agent_frame_id: Some(context.agent_frame_id().clone()),
            },
            Some(session_id.clone()),
        ),
    };
    let declaration = lash_core::ProcessStartDeclaration::new(
        lash_core::ProcessInput::Engine {
            kind: engine_kind(args),
            payload,
        },
        lash_core::RecoveryContract::Rerunnable,
        originator,
        lash_core::ProcessLifecyclePolicy::new(parent, lash_core::OnParentEnd::Abandon),
    )
    .with_wake_session_id(wake_session_id)
    // The attempt bound this host stamps onto a child. It lives on the runtime
    // execution context, which only the in-attempt start path could read before
    // FIG-2999; a leaf start that could not reach it registered its child with
    // no bound, and a child failing the same way every attempt retried forever.
    // A redrive that re-registers the same deterministic id still takes the
    // bound recorded on the row, not this one.
    .with_max_attempts(context.engine_child_max_attempts().map(|bound| bound.get()))
    // An engine start is admitted against the execution env its own record
    // carries, never against the live session env, so the declaration captures
    // the attempt's env spec here rather than leaving realization to substitute
    // one (FIG-2999).
    .with_env_spec(context.process_execution_env_spec());
    // The documented `label` argument: a host-facing name for this run, never
    // part of the process's identity (FIG-3122). Declaring it here is the only
    // way it reaches the row — an engine derives its own label from the
    // payload, and for Lashlang that is the lift digest, so a run the author
    // named `probe` lists as `__process_<hash>` unless the start declares the
    // name. A start that passes no label keeps the engine's derived one.
    let declaration = match start_label(args) {
        Ok(None) => declaration,
        Ok(Some(label)) => declaration.with_declared_identity(
            lash_core::DeclaredProcessIdentity::labelled(engine_kind(args), Some(label)),
        ),
        Err(message) => return refuse(message),
    };
    let process_id = ProcessId::from_intent_identity(&identity);
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(unrealized_start_handle(&process_id)),
        ToolIntents::v3(vec![ToolIntent::StartProcess(Box::new(
            lash_core::StartProcessIntent {
                session_id,
                declaration,
            },
        ))]),
    )
}

/// The handle a start answers with before its declaration is realized.
///
/// A handle names a process *and* the incarnation it was taken against, and the
/// incarnation is a sequence the registry allocates when the row lands — after
/// this attempt has sealed its output. So the declaration answers the one
/// handle kind with the incarnation it does not yet have, and the realization
/// projects the handle the registry actually minted over it
/// (`project_recorded_intent_outcomes`). A cell that somehow held this record
/// without the projection holds a handle that names no incarnation, which every
/// reader already refuses (`ProcessRef::from_handle_json`) — it can never be
/// mistaken for a live one.
const UNREALIZED_INCARNATION: u64 = 0;

fn unrealized_start_handle(process_id: &ProcessId) -> Value {
    let mut handle = lash_sansio::handle::handle_record_json(
        &lash_sansio::handle::HandleId::process(process_id.as_str(), UNREALIZED_INCARNATION),
    );
    handle["process_id"] = serde_json::json!(process_id);
    handle
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
/// The declaration carries the claimed name to the registry (FIG-2995):
/// realization resolves the definition once, pins the returned reference into
/// the durable row, and refuses with the shared typed
/// `process_definition_registry_unavailable` reason on a runtime that has no
/// registry. No `expected_revision` rides this tool, so the registration is a
/// fresh-slot claim and a take-over of a registered name refuses with the
/// typed conflict instead of silently rewriting it.
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
                name: Some(name.to_string()),
                expected_revision: None,
            },
        ))]),
    )
}

#[cfg(test)]
#[path = "declarations_tests.rs"]
mod declarations_tests;
