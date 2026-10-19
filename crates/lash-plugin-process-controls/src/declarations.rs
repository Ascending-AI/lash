//! The declaring process-control leaf tools: `start` and
//! `get`.
//!
//! Each one is an ordinary leaf tool. None of them performs its durable act in
//! the attempt body: every one declares a [`lash_execution_contracts::tool_intent::ToolIntent`] and lets
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

use lash_core::AttemptContext;
use lash_core::ToolAttemptOutcome;
use lash_core::ToolDeclaration;
use lash_core::ToolDefinition;
use lash_core::ToolIntentKind;
use lash_core::ToolOutcome;
use lash_core::ToolOutcomeDone;
use lash_execution_contracts::tool_intent::ToolIntent;
use lash_execution_contracts::tool_intent::ToolIntents;
use lash_tool_support::{ToolBinding, ToolDefinitionBindingExt};

use crate::done_without_intents;

/// The slot a program passes a function in: the `x-lash` function marker,
/// the one JSON-schema form that admits a function, which crosses as a
/// function value.
pub(crate) fn function_slot_schema() -> Value {
    serde_json::json!({
        "x-lash": { "kind": "function" },
        "description": "An async function passed directly; the runtime copies its code and captures inline."
    })
}

pub fn definition_id_schema() -> Value {
    serde_json::json!({ "type": "object", "properties": { "$lash_definition_id": { "type": "string", "pattern": "^lash\\.definition:sha256:[0-9a-f]{64}$" } }, "required": ["$lash_definition_id"], "additionalProperties": false })
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub fn process_start_tool_definition() -> ToolDefinition {
    ToolDefinition::raw("tool:start_process", "start_process", "Start a durable process from an inline function or a published workflow ID and return its handle, including definition_id. Pass exactly one of `definition` or `definition_id`. Function captures are copied when passed to the tool; args key by parameter name.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "definition": function_slot_schema(),
                "definition_id": definition_id_schema(),
                "args": { "type": "object" },
                "label": { "type": "string" }
            },
            "additionalProperties": false
        }),
        serde_json::json!({"x-lash": {"kind": "process_unknown"}})).expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(30))
        .with_declaration(ToolDeclaration::default().with_intents([ToolIntentKind::StartProcess]), None)
        .expect("a valid tool declaration")
        .with_tool_binding(ToolBinding::new(["processes"], "start"))
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub fn process_get_tool_definition() -> ToolDefinition {
    ToolDefinition::raw("tool:get_process_definition", "get_process_definition", "Resolve a tagged definition ID and retain its definition in this execution.",
        serde_json::json!({"type": "object", "properties": {"definition_id": definition_id_schema()}, "required": ["definition_id"], "additionalProperties": false}),
        serde_json::json!({"type": "object", "properties": {"id": definition_id_schema(), "signature": {}}, "required": ["id", "signature"], "additionalProperties": false})).expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(30))
        .with_declaration(ToolDeclaration::default().with_intents([ToolIntentKind::GetDefinition]), None)
        .expect("a valid tool declaration")
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
        ToolOutcomeDone::ok(lash_protocol_types::handle::definition_slot_json(0)),
        ToolIntents::v3(vec![ToolIntent::GetDefinition(
            lash_execution_contracts::tool_intent::GetDefinitionIntent {
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

/// A retryable attempt fault: it took no effect, so the attempt runs again,
/// and it is never recorded as the call's answer or a refusal (D9). A
/// terminal cause stays the call's failure.
fn host_failed(error: lash_execution_contracts::plugin_error::PluginError) -> ToolAttemptOutcome {
    ToolAttemptOutcome::host_failed(
        lash_core::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation(),
    )
}

/// Declare the start the model asked for (FIG-6034 §3.6): resolve its
/// definition, take the host's lifetime and environment policies against the
/// attempt's recorded facts, hold the chosen launch under the attempt's
/// execution claim, and seal it into the declaration. The declaration's
/// launch is journaled with the outcome; realization never runs a policy.
pub async fn execute_process_start_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
    lifetime: &lash_execution_contracts::process::LifetimePolicy,
    environment: &dyn lash_core::ProcessEnvPolicy,
) -> ToolAttemptOutcome {
    let fields = match exact_fields(args, &["definition", "definition_id", "args", "label"]) {
        Ok(fields) => fields,
        Err(error) => return refuse(error),
    };
    // The documented `label` argument: a host-facing name for this run, never
    // part of the process's identity (FIG-3122). Declaring it here is the only
    // way it reaches the row — an engine derives its own label from the
    // payload, and for Lash VM that is the lift digest, so a run the author
    // named `probe` lists as `__process_<hash>` unless the start declares the
    // name. A start that passes no label keeps the engine's derived one.
    let label = match start_label(args) {
        Ok(label) => label,
        Err(message) => return refuse(message),
    };
    let run_args = match fields.get("args") {
        None => serde_json::Map::new(),
        Some(Value::Object(args)) => args.clone(),
        Some(_) => return refuse("args must be an object"),
    };
    let (resolved, inline) = match (fields.get("definition"), fields.get("definition_id")) {
        (Some(value), None) => match context
            .resolve_inline_start(
                lash_protocol_types::LASH_VM_ENGINE_KIND,
                value.clone(),
                run_args,
            )
            .await
        {
            Ok(inline) => (inline.target.clone(), Some(inline)),
            Err(lash_core::ResolveTargetError::Refused(message)) => return refuse(message),
            Err(lash_core::ResolveTargetError::Fault(error)) => return host_failed(error),
        },
        (None, Some(value)) => {
            let id = match lash_core::ProcessDefinitionId::from_tagged_json(value) {
                Ok(id) => id,
                Err(error) => return refuse(error),
            };
            match context
                .resolve_start_target(
                    &lash_execution_contracts::process::ProcessDefinitionTarget::DefinitionId(id),
                    run_args,
                )
                .await
            {
                Ok(resolved) => (resolved, None),
                Err(lash_core::ResolveTargetError::Refused(message)) => return refuse(message),
                Err(lash_core::ResolveTargetError::Fault(error)) => return host_failed(error),
            }
        }
        _ => return refuse("exactly one of definition or definition_id is required"),
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
    // Children inherit their chain's originator rather than the ephemeral
    // session that executes their parent.
    let spawn = context.process_spawn_provenance().cloned();
    let originator = match (spawn, context.owner()) {
        (Some(spawn), _) => spawn.originator,
        (
            None,
            lash_core::ExecutionOwner::SessionFrame {
                session_id,
                agent_frame_id,
            },
        ) => lash_execution_contracts::process::ProcessOriginator::Session {
            session_id: session_id.clone(),
            agent_frame_id: Some(agent_frame_id.clone()),
        },
        (None, lash_core::ExecutionOwner::Process { process_id }) => {
            return refuse(format!(
                "process `{process_id}` carries no spawn provenance for the child it starts"
            ));
        }
    };
    // The host's environment policy states the child's environment and
    // engine settings from the attempt's recorded run, never live config. A
    // refusal is the tool's error; a fault is retried.
    let choice = match environment
        .choose(&context.start_policy_cx(&resolved, label.as_deref()))
        .await
    {
        Ok(choice) => choice,
        Err(lash_core::ProcessEnvPolicyError::Refused(refusal)) => return refuse(refusal),
        Err(lash_core::ProcessEnvPolicyError::Fault(message)) => {
            return host_failed(
                lash_execution_contracts::plugin_error::PluginError::attempt_fault(message),
            );
        }
    };
    let declaration = match inline {
        Some(inline) => {
            let launch = match context.declare_inline_launch(&choice, &inline).await {
                Ok(launch) => launch,
                Err(lash_core::HoldEnvError::Refused(refusal)) => return refuse(refusal),
                Err(lash_core::HoldEnvError::Fault(error)) => return host_failed(error),
            };
            lash_execution_contracts::process::ProcessStartDeclaration::inline(
                resolved.args.clone(),
                launch,
                originator,
                lifetime,
            )
        }
        None => {
            let launch = match context.declare_launch(&choice, &resolved).await {
                Ok(launch) => launch,
                Err(lash_core::HoldEnvError::Refused(refusal)) => return refuse(refusal),
                Err(lash_core::HoldEnvError::Fault(error)) => return host_failed(error),
            };
            lash_execution_contracts::process::ProcessStartDeclaration::definition(
                Some(resolved.signature.clone()),
                resolved.args.clone(),
                launch,
                originator,
                lifetime,
            )
        }
    };
    let declaration = match label {
        None => declaration,
        Some(label) => declaration.with_declared_identity(
            lash_execution_contracts::process::DeclaredProcessIdentity::labelled(
                "definition",
                Some(label),
            ),
        ),
    };
    // The process id is minted when the declared start is realized, after
    // this attempt seals its output, so the attempt answers the start's result
    // slot and the realization replaces it with the handle of the process the
    // start registered (ADR 0107).
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(lash_protocol_types::handle::process_start_slot_json(
            identity.intent_index,
        )),
        ToolIntents::v3(vec![ToolIntent::StartProcess(Box::new(
            lash_execution_contracts::tool_intent::StartProcessIntent { owner, declaration },
        ))]),
    )
}

#[cfg(test)]
#[path = "declarations_tests.rs"]
mod declarations_tests;
