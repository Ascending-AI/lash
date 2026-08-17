//! Protocol-stack runtime-control tools (`processes.list`,
//! `processes.cancel`).
//!
//! Dedicated plugins register these tools into the normal tool-provider
//! surface, so protocol crates do not own or duplicate runtime control behavior.

use std::sync::Arc;

use serde_json::Value;

use lash_core::plugin::{
    PluginError, PluginFactory, PluginSessionContext, PluginSpec, SessionPlugin,
    StaticPluginFactory,
};
use lash_core::{AttemptToolCall, ToolCall, ToolDefinition, ToolProvider, ToolResult};
use lash_tool_support::{
    LashlangToolBinding, StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt,
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
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        ToolResult::err_fmt(format_args!(
            "process tool `{}` requires the leaf AttemptContext signature",
            call.name
        ))
    }

    fn supports_attempt_context(&self, tool_id: &lash_core::ToolId) -> bool {
        matches!(
            tool_id.as_str(),
            "tool:list_process_handles" | "tool:cancel_process"
        )
    }

    async fn execute_attempt(&self, call: AttemptToolCall<'_>) -> lash_core::ToolAttemptResult {
        if call.name == "list_process_handles" {
            return done_without_intents(
                execute_process_list_tool_call(call.context, call.args).await,
            );
        }
        if call.name != "cancel_process" || !self.include_cancel_process {
            return done_without_intents(ToolResult::err_fmt(format_args!(
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
            return done_without_intents(ToolResult::err_fmt(
                "cancel_process requires `process_id`",
            ));
        };
        lash_core::ToolAttemptResult::done(
            lash_core::ToolResultDone::ok(serde_json::json!({
                "process_id": process_id,
                "status": "cancelled",
            })),
            lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::CancelProcess(
                lash_core::CancelProcessIntent {
                    session_id: call.context.session_id().to_string(),
                    process_id,
                    reason: Some("cancelled by processes.cancel".to_string()),
                },
            )]),
        )
    }
}

fn done_without_intents(result: ToolResult) -> lash_core::ToolAttemptResult {
    match result {
        ToolResult::Done(output) => lash_core::ToolAttemptResult::done_without_intents(
            lash_core::ToolResultDone::from_output(*output),
        ),
        ToolResult::Pending(pending) => lash_core::ToolAttemptResult::pending(pending),
    }
}

pub fn process_list_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:list_process_handles",
        "list_process_handles",
        "List process runs visible to this session, including `shell.start` runs, with process id, descriptor, optional definition name, and lifecycle status. Filters are optional; the default returns running runs.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["running", "completed", "failed", "cancelled", "any"],
                    "description": "Lifecycle status to list. The default is `running`; `any` includes historical runs."
                },
                "definition": {
                    "type": "object",
                    "description": "A process definition value, for example `on_button`."
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
    .with_lashlang_binding(LashlangToolBinding::new(["processes"], "list"))
}

fn processes_tool_definitions(include_cancel_process: bool) -> Vec<ToolDefinition> {
    let mut definitions = vec![process_list_tool_definition()];
    if include_cancel_process {
        definitions.push(process_cancel_tool_definition());
    }
    definitions
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
    .with_lashlang_binding(LashlangToolBinding::new(["processes"], "cancel"))
}

pub async fn execute_process_list_tool_call(
    context: &lash_core::AttemptContext<'_>,
    args: &Value,
) -> ToolResult {
    let filter = match lash_core::ProcessListFilter::decode(args) {
        Ok(filter) => filter,
        Err(err) => return ToolResult::err_fmt(err),
    };
    let processes = context.processes();
    let result = processes.list_handles_filtered(&filter).await;
    match result {
        Ok(entries) => ToolResult::ok(serde_json::json!(entries)),
        Err(err) => ToolResult::err_fmt(err.to_string()),
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
        assert!(
            definitions
                .iter()
                .all(|tool| tool.manifest.bindings.is_empty())
        );
        #[cfg(feature = "lashlang")]
        assert!(definitions.iter().all(|tool| {
            tool.manifest
                .bindings
                .contains_key(lash_lashlang_runtime::LASHLANG_TOOL_BINDING_KEY)
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
            .execute_attempt(AttemptToolCall {
                name: "cancel_process",
                args: &serde_json::json!({"process_id": "literal-process"}),
                context: &context,
            })
            .await;
        let lash_core::ToolAttemptResult::Done { result, intents } = result else {
            panic!("processes.cancel must complete with an intent")
        };
        assert_eq!(
            result.into_output().value_for_projection(),
            serde_json::json!({
                "process_id": "literal-process",
                "status": "cancelled",
            })
        );
        assert_eq!(intents.protocol_version, lash_core::TOOL_INTENT_PROTOCOL_V1);
        assert_eq!(intents.intents.len(), 1);
        let lash_core::ToolIntent::CancelProcess(intent) = &intents.intents[0] else {
            panic!("processes.cancel must declare CancelProcess")
        };
        assert_eq!(intent.session_id, "test-session");
        assert_eq!(intent.process_id, "literal-process");
        assert_eq!(
            intent.reason.as_deref(),
            Some("cancelled by processes.cancel")
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
        assert!(rendered.contains("status?: enum["), "{rendered}");
        assert!(rendered.contains("definition?: record"), "{rendered}");
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
        .build_session("standard", None)
        .expect("standard session");
        let standard_names = standard_session
            .resolved_tool_catalog("standard")
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
        .build_session("rlm", None)
        .expect("rlm session");
        let rlm_names = rlm_session
            .resolved_tool_catalog("rlm")
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
