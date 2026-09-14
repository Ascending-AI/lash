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
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::err_fmt(format_args!(
            "process tool `{}` requires the leaf AttemptContext signature",
            call.name
        ))
    }

    /// `await_process` parks, so the runtime pre-derives the completion key its
    /// recorded attempt reads. Nothing else in this plugin defers.
    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id.as_str() == "tool:await_process"
    }

    async fn execute_attempt(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name == "await_process" {
            return execute_process_await_tool_call(call.context, call.args);
        }
        if call.name == "list_process_handles" {
            return done_without_intents(
                execute_process_list_tool_call(call.context, call.args).await,
            );
        }
        if call.name != "cancel_process" || !self.include_cancel_process {
            return done_without_intents(ToolOutcome::err_fmt(format_args!(
                "Unknown leaf process tool: {}",
                call.name
            )));
        }
        let Some(process_id) = call
            .args
            .get("process_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            return done_without_intents(ToolOutcome::err_fmt(
                "cancel_process requires `process_id`",
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

fn done_without_intents(result: ToolOutcome) -> lash_core::ToolAttemptOutcome {
    match result {
        ToolOutcome::Done(output) => lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(*output),
        ),
        ToolOutcome::Pending(pending) => lash_core::ToolAttemptOutcome::pending(pending),
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
        process_list_tool_definition(),
        process_await_tool_definition(),
    ];
    if include_cancel_process {
        definitions.push(process_cancel_tool_definition());
    }
    definitions
}

/// `processes.await(handle)` — park until the process behind `handle` reaches
/// its terminal, and answer with that terminal.
///
/// The argument is typed as a handle through `x-lash` rather than as a record:
/// a cell passes the process handle value itself, whose nominal type is not
/// assignable to a record, so a `{"type":"object"}` parameter would refuse the
/// call in the type checker before the handler ever ran.
pub fn process_await_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:await_process",
        "await_process",
        "Wait for a durable process to finish and return its terminal outcome. Pass the handle a process start or `processes.list(...)` returned. The wait is durable: it survives a restart of the waiting turn, and cancelling the turn drops the wait without cancelling the process.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "x-lash": { "kind": "handle" },
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
        "Request cancellation for a durable process, including a running `shell.start` process, by `process_id`.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "Process id returned by a process handle or `processes.list(...)`."
                }
            },
            "required": ["process_id"],
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

fn process_list_output_schema() -> Value {
    serde_json::json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "__handle__": {
                    "type": "string",
                    "enum": ["process"],
                    "description": "Handle marker; pass the whole record where a process handle is needed."
                },
                "id": {
                    "type": "string",
                    "description": "Process handle id."
                },
                "process_id": {
                    "type": "string",
                    "description": "Same process id, repeated for tools that ask for process_id."
                },
                "descriptor": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string" },
                        "label": { "type": "string" }
                    },
                    "additionalProperties": false
                },
                "definition": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                },
                "status": {
                    "type": "string",
                    "enum": ["running", "completed", "failed", "cancelled"]
                }
            },
            "required": ["__handle__", "id", "process_id", "descriptor", "status"],
            "additionalProperties": false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_expose_processes_tools() {
        let definitions = processes_tool_definitions(true);
        let names = definitions
            .iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["list_process_handles", "cancel_process"]);
        #[cfg(not(feature = "lashlang"))]
        for definition in &definitions {
            assert_eq!(
                definition
                    .manifest
                    .bindings
                    .contains_key(lash_tool_support::LASHLANG_TOOL_BINDING_KEY),
                lash_tool_support::LASHLANG_BINDINGS_ENABLED,
                "{} had Lashlang binding state inconsistent with lash-tool-support",
                definition.name()
            );
            if !lash_tool_support::LASHLANG_BINDINGS_ENABLED {
                assert!(
                    definition.manifest.bindings.is_empty(),
                    "{} unexpectedly had bindings: {:?}",
                    definition.name(),
                    definition.manifest.bindings
                );
            }
        }
        #[cfg(feature = "lashlang")]
        assert!(definitions.iter().all(|tool| {
            tool.manifest
                .bindings
                .contains_key(lash_tool_support::LASHLANG_TOOL_BINDING_KEY)
        }));
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
            .execute_attempt(ToolCall {
                name: "cancel_process",
                args: &serde_json::json!({"process_id": "literal-process"}),
                context: &context,
            })
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
