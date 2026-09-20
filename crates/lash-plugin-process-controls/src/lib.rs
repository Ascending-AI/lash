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
use lash_core::{ProcessId, SessionId, ToolCall, ToolDefinition, ToolOutcome, ToolProvider};
use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolBinding, ToolDefinitionBindingExt,
};

mod declarations;

pub use declarations::{
    DEFAULT_PROCESS_ENGINE_KIND, execute_process_emit_tool_call,
    execute_process_register_tool_call, execute_process_signal_tool_call,
    execute_process_start_tool_call, process_emit_tool_definition,
    process_register_tool_definition, process_signal_tool_definition,
    process_start_tool_definition,
};

/// Plugin factory for process-control tools.
///
/// Declares its provider through a [`PluginSpec`] driven by
/// [`StaticPluginFactory`], so it does not hand-roll the `SessionPlugin` +
/// `register` ceremony.
pub struct SessionProcessAdminPluginFactory {
    inner: StaticPluginFactory,
}

impl SessionProcessAdminPluginFactory {
    pub fn new() -> Self {
        Self::with_cancel_process(true)
    }

    pub fn without_cancel_process() -> Self {
        Self::with_cancel_process(false)
    }

    fn with_cancel_process(include_cancel_process: bool) -> Self {
        let provider = StaticToolProvider::new(
            processes_tool_definitions(include_cancel_process),
            SessionProcessAdminTools {
                include_cancel_process,
            },
        );
        let spec =
            PluginSpec::new().with_tool_provider(Arc::new(provider) as Arc<dyn ToolProvider>);
        Self {
            inner: StaticPluginFactory::new("processes", spec),
        }
    }
}

impl Default for SessionProcessAdminPluginFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginFactory for SessionProcessAdminPluginFactory {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        self.inner.build(ctx)
    }
}

struct SessionProcessAdminTools {
    include_cancel_process: bool,
}

#[async_trait::async_trait]
impl StaticToolExecute for SessionProcessAdminTools {
    /// `await_process` parks, so the runtime pre-derives the completion key its
    /// recorded attempt reads. Nothing else in this plugin defers.
    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id.as_str() == "tool:await_process"
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "await_process" {
            return execute_process_await_tool_call(call.context, call.args);
        }
        if call.name() == "start_process" {
            return execute_process_start_tool_call(call.context, call.args).await;
        }
        if call.name() == "signal_process" {
            return execute_process_signal_tool_call(call.context, call.args);
        }
        if call.name() == "emit_process_event" {
            return execute_process_emit_tool_call(call.context, call.args);
        }
        if call.name() == "register_process" {
            return execute_process_register_tool_call(call.context, call.args);
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
        let Some(process_id) = cancel_target(call.args) else {
            return done_without_intents(ToolOutcome::err_fmt(
                "cancel_process requires `handle` or `process_id`",
            ));
        };
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({
                "process_id": process_id,
                "status": "cancelled",
            })),
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::CancelProcess(
                lash_core::CancelProcessIntent {
                    session_id: SessionId::from(call.context.session_id()),
                    process_id: ProcessId::from(process_id),
                },
            )]),
        )
    }
}

/// The process a cancel names, from either accepted spelling.
///
/// A handle is the shape every other process tool takes, so `cancel` accepts
/// it too and reads it through the one handle parser. The bare `process_id`
/// stays for a host that holds an id and never held a handle — a `shell.start`
/// run reported by id, for instance.
fn cancel_target(args: &Value) -> Option<String> {
    if let Some(handle) = args.get("handle")
        && let Ok(process_ref) = lash_core::ProcessRef::from_handle_json(handle)
    {
        return Some(process_ref.process_id.to_string());
    }
    args.get("process_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn done_without_intents(result: ToolOutcome) -> lash_core::ToolAttemptOutcome {
    match result {
        ToolOutcome::Done(output) => lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(*output),
        ),
        ToolOutcome::Pending(pending) => lash_core::ToolAttemptOutcome::pending(*pending),
    }
}

pub fn process_list_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:list_process_handles",
        "list_process_handles",
        "List process runs visible to this session, including `shell.start` runs, with process id, descriptor, optional definition name, and lifecycle status. Filters are optional; the default returns running runs. Empty arguments select running runs; `definition` selects runs of a definition and `status: \"any\"` includes visible run history.",
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
                "definition": {
                    "description": "A process definition value, for example `on_button`: pass the process itself and rows started from it match."
                }
            },
            "additionalProperties": false
        }),
        process_list_output_schema(),
    )
    .with_examples(vec![
        "await processes.list({})?".into(),
        r#"await processes.list({ status: "any" })?"#.into(),
        "await processes.list({ definition: on_button })?".into(),
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
        process_register_tool_definition(),
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
    )
    .with_examples(vec!["await processes.await({ handle: h })?".into()])
    .with_tool_binding(ToolBinding::new(["processes"], "await"))
}

pub fn process_cancel_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:cancel_process",
        "cancel_process",
        "Request cancellation for a durable process, including a running `shell.start` process. Pass the handle a process start or `processes.list(...)` returned, or the bare `process_id`.",
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
    )
    .with_examples(vec![
        "await processes.cancel({ handle: h })?".into(),
        r#"await processes.cancel({ process_id: "tool:call-01JZK7G4QP9Q4J7W3Q2E1H6M9C" })?"#.into(),
        r#"await processes.cancel({ process_id: "subagent:session-01JZK7G4QP9Q4J7W3Q2E1H6M9C" })?"#.into(),
    ])
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
    let process_ref = match lash_core::ProcessRef::from_handle_json(handle) {
        Ok(process_ref) => process_ref,
        Err(err) => return done_without_intents(ToolOutcome::err_fmt(err)),
    };
    if let Err(err) = context.completion_key() {
        return done_without_intents(ToolOutcome::err_fmt(err));
    }
    lash_core::ToolAttemptOutcome::pending(
        lash_core::PendingCompletion::new().resolved_by_process_terminal(process_ref),
    )
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
/// carried, and omitted the `incarnation` a process handle needs to be
/// awaitable.
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
            "incarnation": {
                "type": "integer",
                "description": "Registration incarnation this handle pins, so the handle cannot rebind to a later run of the same id."
            },
            "kind": {
                "type": "string",
                "description": "Engine kind that owns the run."
            },
            "label": {
                "type": "string",
                "description": "Host-facing label, absent when the run has none."
            },
            "definition": {
                "type": "object",
                "properties": {
                    "engine_kind": { "type": "string" },
                    "definition": { "description": "Engine-owned definition value." },
                    "signature": { "description": "Signature the engine resolved for the definition." }
                },
                "required": ["engine_kind", "definition", "signature"],
                "additionalProperties": false,
                "description": "The definition reference this run pins, absent for a run that names none."
            },
            "status": {
                "type": "string",
                "enum": ["running", "waiting", "completed", "failed", "cancelled", "abandoned", "caller_departed"]
            }
        },
        "required": ["__handle__", "id", "process_id", "incarnation", "kind", "status"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_for(_tools: &SessionProcessAdminTools, name: &str) -> lash_core::ToolManifest {
        processes_tool_definitions(true)
            .into_iter()
            .find(|definition| definition.name() == name)
            .expect("process-controls manifest resolves")
            .manifest()
    }

    #[test]
    fn tool_definitions_expose_processes_tools() {
        let definitions = processes_tool_definitions(true);
        let names = definitions
            .iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "start_process",
                "list_process_handles",
                "await_process",
                "signal_process",
                "emit_process_event",
                "register_process",
                "cancel_process"
            ]
        );
        assert!(definitions.iter().all(|tool| {
            tool.manifest
                .bindings
                .contains_key(lash_tool_support::TYPESCRIPT_TOOL_BINDING_KEY)
        }));
    }

    #[test]
    fn list_output_contract_matches_the_handle_view() {
        // The contract a cell type-checks against and the record it receives
        // must be the same shape. Comparing the schema's property set against a
        // real `ProcessHandleView` serialization is what catches the drift the
        // previous hand-written schema had already accumulated.
        let view = lash_core::ProcessHandleView::new(
            "process-1",
            lash_core::ProcessIncarnation::from_registration_sequence(4),
            lash_core::ProcessIdentity::for_definition(
                lash_core::ProcessDefinitionRef::unclaimed(
                    "lashlang",
                    serde_json::json!({ "process_name": "on_button" }),
                ),
                Some("on_button"),
            ),
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

    #[tokio::test]
    async fn cancel_process_accepts_the_handle_shape_every_other_process_tool_takes() {
        let tools = SessionProcessAdminTools {
            include_cancel_process: true,
        };
        let tool_context = lash_core::testing::mock_tool_context();
        let context = lash_core::AttemptContext::__for_testing(
            &tool_context,
            "process-controls-intent-scope",
        );
        let result = tools
            .execute(ToolCall::new(
                &manifest_for(&tools, "cancel_process"),
                &serde_json::json!({ "handle": handle_json("handle-process", 2) }),
                &context,
            ))
            .await;
        let lash_core::ToolAttemptOutcome::Done { intents, .. } = result else {
            panic!("cancel is not a deferring tool");
        };
        let [lash_core::ToolIntent::CancelProcess(intent)] = intents.intents.as_slice() else {
            panic!("expected one cancel declaration, got {intents:?}");
        };
        assert_eq!(intent.process_id.as_str(), "handle-process");
    }

    #[test]
    fn cancel_process_definition_renders_contract() {
        let definition = process_cancel_tool_definition();
        let rendered = definition.compact_contract().render_signature();
        assert!(rendered.contains("status: enum["), "{rendered}");
        assert!(!rendered.contains("terminal:"), "{rendered}");
    }

    #[tokio::test]
    async fn cancel_process_declares_literal_v1_cancel_intent() {
        let tools = SessionProcessAdminTools {
            include_cancel_process: true,
        };
        let tool_context = lash_core::testing::mock_tool_context();
        let context = lash_core::AttemptContext::__for_testing(
            &tool_context,
            "process-controls-intent-scope",
        );
        let result = tools
            .execute(ToolCall::new(
                &manifest_for(&tools, "cancel_process"),
                &serde_json::json!({"process_id": "literal-process"}),
                &context,
            ))
            .await;
        let lash_core::ToolAttemptOutcome::Done { result, intents } = result else {
            panic!("processes.cancel must complete with an intent")
        };
        assert_eq!(
            result.into_output().value_for_projection(),
            serde_json::json!({
                "process_id": "literal-process",
                "status": "cancelled",
            })
        );
        assert_eq!(intents.protocol_version, lash_core::TOOL_INTENT_PROTOCOL_V3);
        assert_eq!(intents.intents.len(), 1);
        let lash_core::ToolIntent::CancelProcess(intent) = &intents.intents[0] else {
            panic!("processes.cancel must declare CancelProcess")
        };
        assert_eq!(intent.session_id, "test-session");
        assert_eq!(intent.process_id, "literal-process");
    }

    fn parked_attempt_context<'run>(
        tool_context: &lash_core::ToolContext<'run>,
    ) -> lash_core::AttemptContext<'run> {
        lash_core::testing::mock_attempt_context_with_completion_key(
            tool_context,
            lash_core::AwaitEventKey {
                scope: lash_core::ExecutionScope::turn("test-session", "test-turn"),
                wait: lash_core::AwaitEventWaitIdentity::ToolCompletion {
                    tool_call_id: "await-process-call".to_string(),
                },
                key_id: "await-process-key".to_string(),
                signature: "await-process-signature".to_string(),
            },
        )
    }

    /// The process-handle record a cell actually holds.
    ///
    /// Minted rather than spelled out: a copy of the record drifts from the
    /// mint the moment the mint changes, which is exactly what happened when
    /// ADR 0095 folded the incarnation into the id.
    fn handle_json(id: &str, incarnation: u64) -> serde_json::Value {
        lash_core::RuntimeExecutionContext::process_handle_json(&lash_core::ProcessRef::new(
            id,
            lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
        ))
    }

    #[tokio::test]
    async fn await_process_parks_naming_the_process_terminal_as_its_resolver() {
        let tools = SessionProcessAdminTools {
            include_cancel_process: true,
        };
        let tool_context = lash_core::testing::mock_tool_context();
        let context = parked_attempt_context(&tool_context);
        let outcome = tools
            .execute(ToolCall::new(
                &manifest_for(&tools, "await_process"),
                &serde_json::json!({ "handle": handle_json("proc-1", 3) }),
                &context,
            ))
            .await;
        let lash_core::ToolAttemptOutcome::Pending(pending) = outcome else {
            panic!("processes.await must park instead of answering inline")
        };
        let Some(lash_core::PendingResolver::ProcessTerminal { process_ref }) = pending.resolved_by
        else {
            panic!("a parked processes.await must name the process terminal as its resolver")
        };
        assert_eq!(process_ref.process_id, "proc-1");
        assert_eq!(
            process_ref.incarnation.registration_sequence(),
            3,
            "the arming must pin the incarnation the caller held, not the id alone"
        );
    }

    /// The park is what makes the wait durable, so a call that cannot be parked
    /// must fail loudly rather than answer inline: an inline answer would be a
    /// silent downgrade to a non-durable await.
    #[tokio::test]
    async fn await_process_refuses_a_value_that_is_not_a_process_handle() {
        let tools = SessionProcessAdminTools {
            include_cancel_process: true,
        };
        let tool_context = lash_core::testing::mock_tool_context();
        let context = parked_attempt_context(&tool_context);
        for (label, args) in [
            ("missing handle", serde_json::json!({})),
            (
                "a record that is not a handle at all",
                serde_json::json!({ "handle": { "id": "x", "incarnation": 1 } }),
            ),
            (
                "a handle with no incarnation",
                serde_json::json!({ "handle": { "__handle__": "process", "id": "x" } }),
            ),
        ] {
            let outcome = tools
                .execute(ToolCall::new(
                    &manifest_for(&tools, "await_process"),
                    &args,
                    &context,
                ))
                .await;
            assert!(
                matches!(outcome, lash_core::ToolAttemptOutcome::Done { .. }),
                "{label} must be refused, not parked"
            );
        }
    }

    #[test]
    fn await_process_declares_a_deferring_attempt_and_a_handle_typed_argument() {
        let definition = process_await_tool_definition();
        let tools = SessionProcessAdminTools {
            include_cancel_process: true,
        };
        assert!(
            StaticToolExecute::attempt_may_defer(&tools, definition.id()),
            "the runtime only pre-derives a completion key for a tool that declares it defers"
        );
        // A `{"type":"object"}` parameter would refuse a nominally typed cell
        // handle in the type checker before the handler ran (FIG-2989), which
        // is exactly what the `x-lash` keyword exists to avoid.
        assert_eq!(
            definition.contract.input_schema.canonical["properties"]["handle"]["x-lash"],
            serde_json::json!({ "kind": "process_unknown" })
        );
    }

    #[test]
    fn list_process_contract_returns_handle_array() {
        let definition = process_list_tool_definition();

        assert_eq!(
            definition.contract.output_schema.canonical["type"],
            serde_json::json!("array")
        );
        let rendered = definition.compact_contract().render_signature();
        assert!(rendered.contains("-> list[record{"), "{rendered}");
        assert!(rendered.contains("__handle__"), "{rendered}");
        assert!(rendered.contains("process_id"), "{rendered}");
        assert!(rendered.contains("definition"), "{rendered}");
        assert!(rendered.contains("status: enum["), "{rendered}");
        assert!(
            rendered.contains("status?: any | record{in: list[enum["),
            "{rendered}"
        );
        // A Lashlang cell passes the process itself, whose `Process<...>` type
        // is not assignable to a record, so the filter parameter is untyped.
        assert!(rendered.contains("definition?: any"), "{rendered}");
        assert!(!rendered.contains("history"), "{rendered}");
        assert!(!rendered.contains("terminal:"), "{rendered}");
    }

    #[test]
    fn plugin_registers_cancel_when_configured_and_omits_it_otherwise() {
        let standard_session = lash_core::facade_support::PluginHost::new(
            std::iter::once(
                Arc::new(SessionProcessAdminPluginFactory::new()) as Arc<dyn PluginFactory>
            )
            .chain(lash_core::testing::test_standard_protocol_factories())
            .collect(),
        )
        .build_session("standard")
        .expect("standard session");
        let standard_names = standard_session
            .resolved_tool_catalog(&SessionId::from("standard"))
            .expect("standard tool catalog")
            .tool_names()
            .as_ref()
            .clone();

        let rlm_session = lash_core::facade_support::PluginHost::new(
            std::iter::once(
                Arc::new(SessionProcessAdminPluginFactory::without_cancel_process())
                    as Arc<dyn PluginFactory>,
            )
            .chain(lash_core::testing::test_code_protocol_factories())
            .collect(),
        )
        .build_session("rlm")
        .expect("rlm session");
        let rlm_names = rlm_session
            .resolved_tool_catalog(&SessionId::from("rlm"))
            .expect("rlm tool catalog")
            .tool_names()
            .as_ref()
            .clone();

        assert!(standard_names.contains(&"list_process_handles".to_string()));
        assert!(standard_names.contains(&"cancel_process".to_string()));
        assert!(rlm_names.contains(&"list_process_handles".to_string()));
        assert!(!rlm_names.contains(&"cancel_process".to_string()));
    }
}
