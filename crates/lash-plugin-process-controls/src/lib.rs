//! Protocol-stack runtime-control tools (`processes.list`,
//! `processes.cancel`, `processes.await`).
//!
//! Dedicated plugins register these tools into the normal tool-provider
//! surface, so protocol crates do not own or duplicate runtime control behavior.

use std::sync::Arc;

use serde_json::Value;

use lash_core::plugin::{
    PluginError, PluginFactory, PluginSessionContext, PluginSpec, SessionPlugin,
    StaticPluginFactory,
};
use lash_core::{ProcessId, ToolCall, ToolDefinition, ToolOutcome, ToolProvider};
use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolBinding, ToolDefinitionBindingExt,
};

mod declarations;

pub use declarations::{
    execute_process_emit_tool_call, execute_process_get_tool_call,
    execute_process_signal_tool_call, execute_process_start_tool_call,
    process_emit_tool_definition, process_get_tool_definition, process_signal_tool_definition,
    process_start_tool_definition,
};

/// Plugin factory for process-control tools.
///
/// Declares its provider through a [`PluginSpec`] executed by
/// [`StaticPluginFactory`], so it does not hand-roll the `SessionPlugin` +
/// `register` ceremony.
///
/// The [`LifetimePolicy`](lash_core::LifetimePolicy) is required and has no
/// default: it chooses the lifetime of every process the model's
/// `start_process` declares, against the start's admitted
/// [`StartCx`](lash_core::StartCx). The model never picks one (FIG-3607).
pub struct SessionProcessAdminPluginFactory {
    inner: StaticPluginFactory,
}

impl SessionProcessAdminPluginFactory {
    /// Process controls whose starts take `lifetime`, for example
    /// [`lash_core::lifetime::session_or_starter`].
    pub fn new(
        lifetime: impl Fn(&lash_core::StartCx) -> lash_core::Lifetime + Send + Sync + 'static,
    ) -> Self {
        Self::with_cancel_process(Arc::new(lifetime), true)
    }

    /// The same controls without `cancel_process`.
    pub fn without_cancel_process(
        lifetime: impl Fn(&lash_core::StartCx) -> lash_core::Lifetime + Send + Sync + 'static,
    ) -> Self {
        Self::with_cancel_process(Arc::new(lifetime), false)
    }

    fn with_cancel_process(
        lifetime: lash_core::LifetimePolicy,
        include_cancel_process: bool,
    ) -> Self {
        let provider = StaticToolProvider::new(
            processes_tool_definitions(include_cancel_process),
            SessionProcessAdminTools {
                include_cancel_process,
                lifetime,
            },
        );
        let spec =
            PluginSpec::new().with_tool_provider(Arc::new(provider) as Arc<dyn ToolProvider>);
        Self {
            inner: StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("processes"),
                spec,
            ),
        }
    }
}

impl PluginFactory for SessionProcessAdminPluginFactory {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        self.inner.declaration()
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        self.inner.build(ctx)
    }
}

struct SessionProcessAdminTools {
    include_cancel_process: bool,
    lifetime: lash_core::LifetimePolicy,
}

#[async_trait::async_trait]
impl StaticToolExecute for SessionProcessAdminTools {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "await_process" {
            return execute_process_await_tool_call(call.context, call.args);
        }
        if call.name() == "start_process" {
            return execute_process_start_tool_call(call.context, call.args, &self.lifetime).await;
        }
        if call.name() == "signal_process" {
            return execute_process_signal_tool_call(call.context, call.args);
        }
        if call.name() == "emit_process_event" {
            return execute_process_emit_tool_call(call.context, call.args);
        }
        if call.name() == "get_process_definition" {
            return execute_process_get_tool_call(call.context, call.args);
        }
        if call.name() == "list_process_handles" {
            return done_without_intents(
                execute_process_list_tool_call(call.context, call.args).await,
            );
        }
        if call.name() != "cancel_process" || !self.include_cancel_process {
            return done_without_intents(ToolOutcome::err_fmt(format_args!(
                "Unknown leaf process tool: {}",
                call.name()
            )));
        }
        let process_id = match cancel_target(call.args) {
            Ok(process_id) => process_id,
            Err(refusal) => return done_without_intents(ToolOutcome::err_fmt(refusal)),
        };
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({
                "process_id": process_id,
                "status": "cancelled",
            })),
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::CancelProcess(
                lash_core::CancelProcessIntent {
                    owner: call.context.owner().runtime_owner(),
                    process_id,
                },
            )]),
        )
    }
}

/// The process a cancel names, from either accepted spelling.
///
/// A handle is the shape every other process tool takes, so `cancel` accepts
/// it too and reads it through the one handle parser. The bare `process_id`
/// stays for a host that holds an id and never held a handle — an
/// Externally-Owned run the host launched and reported by id, for instance.
///
/// A value that is present but names no process is refused with the reason,
/// never passed over for the other spelling: a retired handle or an id no
/// registrar minted says so.
fn cancel_target(args: &Value) -> Result<ProcessId, String> {
    if let Some(handle) = args.get("handle") {
        return lash_core::process_id_from_handle_json(handle)
            .map_err(|refusal| format!("cancel_process `handle`: {refusal}"));
    }
    match args.get("process_id") {
        Some(Value::String(value)) => ProcessId::parse(value.trim())
            .map_err(|refusal| format!("cancel_process `process_id`: {refusal}")),
        Some(_) => Err("cancel_process `process_id` must be a string".to_string()),
        None => Err("cancel_process requires `handle` or `process_id`".to_string()),
    }
}

pub(crate) fn done_without_intents(result: ToolOutcome) -> lash_core::ToolAttemptOutcome {
    match result {
        ToolOutcome::Done(output) => lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(*output),
        ),
        ToolOutcome::Pending(pending) => lash_core::ToolAttemptOutcome::pending(*pending),
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub fn process_list_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:list_process_handles",
        "list_process_handles",
        "List process runs visible to this session, including host-launched runs, with process id, descriptor, optional definition ID, and lifecycle status. Filters are optional; the default returns running runs. Empty arguments select running runs; `definition_id` selects runs of a definition and `status: \"any\"` includes visible run history.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "anyOf": [
                        { "const": "any" },
                        { "type": "object", "properties": { "in": { "type": "array", "items": { "enum": ["running", "waiting", "completed", "failed", "cancelled", "abandoned", "caller_departed"] } } }, "required": ["in"], "additionalProperties": false }
                    ],
                    "description": "Any-of lifecycle status set. Absence selects running runs; `any` includes every status."
                },
                // Deliberately untyped: the value is whatever definition
                // encoding the engine that started the run stores, and a
                // Lashlang cell passes the process itself (`on_button`), whose
                // `Process<...>` type is not assignable to a record.
                "definition_id": declarations::definition_id_schema()
            },
            "additionalProperties": false
        }),
        process_list_output_schema(),
    ).expect("valid declared tool schemas")
    .with_examples(vec![
        "await processes.list({})?".into(),
        r#"await processes.list({ status: "any" })?"#.into(),
        "await processes.list({ definition_id: on_button.id })?".into(),
    ])
    .with_tool_binding(ToolBinding::new(["processes"], "list"))
}

fn processes_tool_definitions(include_cancel_process: bool) -> Vec<ToolDefinition> {
    let mut definitions = vec![
        process_start_tool_definition(),
        process_list_tool_definition(),
        process_await_tool_definition(),
        process_signal_tool_definition(),
        process_emit_tool_definition(),
        process_get_tool_definition(),
    ];
    if include_cancel_process {
        definitions.push(process_cancel_tool_definition());
    }
    definitions
}

/// `processes.await(handle)` — park until the process behind `handle` reaches
/// its terminal, and answer with that terminal.
///
/// The argument is typed through `x-lash` rather than as a record: a cell
/// passes the process handle value itself, whose nominal type is not assignable
/// to a record, so a `{"type":"object"}` parameter would refuse the call in the
/// type checker before the handler ever ran.
///
/// The kind is `process_unknown` — a process the host can only describe as
/// callable — because the host has no authoritative call signature for an
/// arbitrary awaited process. `handle` is the *trigger* handle kind and carries
/// the payload its trigger delivers, which is a different type and would refuse
/// a process value here.
#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub fn process_await_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:await_process",
        "await_process",
        "Wait for a durable process to finish and return its terminal outcome. Pass the handle a process start or `processes.list(...)` returned. The wait is durable: it survives a restart of the waiting turn, and cancelling the turn drops the wait without cancelling the process.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "x-lash": { "kind": "process_unknown" },
                    "description": "Process handle to wait on, as returned by a process start or `processes.list(...)`."
                }
            },
            "required": ["handle"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "description": "The process's terminal outcome."
        }),
    ).expect("valid declared tool schemas")
    .with_examples(vec!["await processes.await({ handle: h })?".into()])
    // `await_process` parks, so admission records that it may defer and the
    // runtime pre-derives the completion key its recorded attempt reads.
    .with_declaration(lash_core::ToolDeclaration::deferring())
    .with_tool_binding(ToolBinding::new(["processes"], "await"))
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub fn process_cancel_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:cancel_process",
        "cancel_process",
        "Request cancellation for a durable process, including a running host-launched process. Pass the handle a process start or `processes.list(...)` returned, or the bare `process_id`.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "x-lash": { "kind": "process_unknown" },
                    "description": "Process handle to cancel, as returned by a process start or `processes.list(...)`."
                },
                "process_id": {
                    "type": "string",
                    "description": "Process id, for a caller that holds the bare id rather than a handle."
                }
            },
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string" },
                "status": {
                    "type": "string",
                    "enum": ["running", "completed", "failed", "cancelled"]
                }
            },
            "required": ["process_id", "status"],
            "additionalProperties": false
        }),
    ).expect("valid declared tool schemas")
    .with_examples(vec![
        "await processes.cancel({ handle: h })?".into(),
        r#"await processes.cancel({ process_id: "tool:call-01JZK7G4QP9Q4J7W3Q2E1H6M9C" })?"#.into(),
        r#"await processes.cancel({ process_id: "subagent:session-01JZK7G4QP9Q4J7W3Q2E1H6M9C" })?"#.into(),
    ])
    .with_declaration(
        lash_core::ToolDeclaration::default()
            .with_intents([lash_core::ToolIntentKind::CancelProcess]),
    )
    .with_tool_binding(ToolBinding::new(["processes"], "cancel"))
}

/// Parks the call on the terminal of the handle's process.
///
/// It takes the completion key first and names the process terminal as the
/// resolver, so the runtime — not this tool — is responsible for arming the
/// wait, both now and on every redrive of the parked turn. That is what makes
/// the wait durable: nothing here holds a future, a task, or a watcher that a
/// crash could lose.
///
/// Deliberately intent-free. A parking attempt cannot carry tool intents by
/// construction, and it needs none: the durable act is the journaled arming the
/// runtime performs from the declaration below, not a side effect this body
/// requests.
pub fn execute_process_await_tool_call(
    context: &lash_core::AttemptContext<'_>,
    args: &Value,
) -> lash_core::ToolAttemptOutcome {
    let Some(handle) = args.get("handle") else {
        return done_without_intents(ToolOutcome::err_fmt("await_process requires `handle`"));
    };
    let process_id = match lash_core::process_id_from_handle_json(handle) {
        Ok(process_id) => process_id,
        Err(err) => return done_without_intents(ToolOutcome::err_fmt(err)),
    };
    if let Err(err) = context.completion_key() {
        return done_without_intents(ToolOutcome::err_fmt(err));
    }
    let mut pending = lash_core::PendingCompletion::new().resolved_by_process_terminal(process_id);
    pending.on_cancel = lash_core::CancelHint::Ignore;
    lash_core::ToolAttemptOutcome::pending(pending)
}

pub async fn execute_process_list_tool_call(
    context: &lash_core::AttemptContext<'_>,
    args: &Value,
) -> ToolOutcome {
    let filter = match lash_core::ProcessListFilter::decode(args) {
        Ok(filter) => filter,
        Err(err) => return ToolOutcome::err_fmt(err),
    };
    let processes = context.processes();
    let result = processes.list_handles_filtered(&filter).await;
    match result {
        Ok(entries) => ToolOutcome::ok(serde_json::json!(entries)),
        Err(err) => ToolOutcome::err_fmt(err.to_string()),
    }
}

/// The one handle shape `processes.list` answers with.
///
/// Written from [`lash_core::ProcessHandleView`]'s own serde shape rather than
/// from a reading of what a caller might want: the contract a cell type-checks
/// against and the record it actually receives are the same shape, and
/// `list_output_contract_matches_the_handle_view` fails if they drift. The
/// previous hand-written schema had already drifted — it advertised a
/// `descriptor` object and a `{name}` definition that no handle view has ever
/// carried.
fn process_list_output_schema() -> Value {
    serde_json::json!({
        "type": "array",
        "items": process_handle_view_schema()
    })
}

/// The schema of one [`lash_core::ProcessHandleView`].
pub fn process_handle_view_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "__handle__": {
                "type": "string",
                "enum": ["lash"],
                "description": "Handle marker; pass the whole record where a process handle is needed."
            },
            "id": {
                "type": "string",
                "description": "Opaque handle id. Carry it and hand it back; do not read its parts or build one."
            },
            "process_id": {
                "type": "string",
                "description": "The process this handle names, for tools that ask for a process_id."
            },
            "kind": {
                "type": "string",
                "description": "Engine kind that owns the run."
            },
            "label": {
                "type": "string",
                "description": "Host-facing label, absent when the run has none."
            },
            "definition_id": declarations::definition_id_schema(),
            "status": {
                "type": "string",
                "enum": ["running", "waiting", "completed", "failed", "cancelled", "abandoned", "caller_departed"]
            }
        },
        "required": ["__handle__", "id", "process_id", "kind", "status"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_output_contract_matches_the_handle_view() {
        // The contract a cell type-checks against and the record it receives
        // must be the same shape. Comparing the schema's property set against a
        // real `ProcessHandleView` serialization is what catches the drift the
        // previous hand-written schema had already accumulated.
        let view = lash_core::ProcessHandleView::new(
            lash_core::ProcessId::fixture("process-1"),
            {
                let mut identity =
                    lash_core::ProcessIdentity::labelled("lashlang", Some("on_button"));
                identity.definition_id =
                    Some(lash_core::ProcessDefinitionId::from_sha256_digest([1; 32]));
                identity
            },
            lash_core::ProcessStatus::Running,
        );
        let serialized = serde_json::to_value(&view).expect("a handle view serializes");
        let serialized = serialized.as_object().expect("a handle view is an object");
        let schema = process_handle_view_schema();
        let properties = schema["properties"]
            .as_object()
            .expect("the schema declares properties");

        for name in serialized.keys() {
            assert!(
                properties.contains_key(name),
                "the handle view carries `{name}`, which the contract does not declare"
            );
        }
        for name in properties.keys() {
            assert!(
                serialized.contains_key(name),
                "the contract declares `{name}`, which no handle view carries"
            );
        }
        for name in schema["required"]
            .as_array()
            .expect("the schema names required properties")
        {
            let name = name.as_str().expect("a required property is a name");
            assert!(
                serialized.contains_key(name),
                "the contract requires `{name}`, which this handle view omits"
            );
        }
    }

    #[test]
    fn await_process_declares_a_deferring_attempt_and_a_handle_typed_argument() {
        let definition = process_await_tool_definition();
        assert!(
            definition.manifest.declaration.may_defer,
            "the runtime only pre-derives a completion key for a tool that declares it defers"
        );
        // A `{"type":"object"}` parameter would refuse a nominally typed cell
        // handle in the type checker before the handler ran (FIG-2989), which
        // is exactly what the `x-lash` keyword exists to avoid.
        assert_eq!(
            definition.contract.input_schema.canonical.as_value()["properties"]["handle"]["x-lash"],
            serde_json::json!({ "kind": "process_unknown" })
        );
    }

    /// A handle or id that is present but names no process is refused with
    /// its reason, never reported as a missing argument.
    #[test]
    fn a_cancel_target_that_names_no_process_is_refused_with_its_reason() {
        let retired = serde_json::json!({
            "handle": { "__handle__": "lash", "id": "p.1.old-name" },
            "process_id": lash_core::process_id_for_test("fallback").as_str(),
        });
        let refusal = cancel_target(&retired).expect_err("a retired handle names no process");
        assert!(refusal.contains("retired spelling"), "{refusal}");

        let unminted = serde_json::json!({ "process_id": "host-chosen-name" });
        let refusal = cancel_target(&unminted).expect_err("a host name is no minted id");
        assert!(
            refusal.starts_with("cancel_process `process_id`"),
            "{refusal}"
        );

        let refusal = cancel_target(&serde_json::json!({})).expect_err("nothing named");
        assert_eq!(refusal, "cancel_process requires `handle` or `process_id`");

        let minted = lash_core::process_id_for_test("minted");
        let named = serde_json::json!({ "process_id": minted.as_str() });
        assert_eq!(cancel_target(&named), Ok(minted));
    }
}
