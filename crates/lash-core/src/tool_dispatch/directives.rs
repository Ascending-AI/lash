use crate::plugin::{PluginDirective, PluginOwned, emit_plugin_runtime_events};
use crate::{ToolFailure, ToolFailureClass, ToolResult};

use super::context::ToolDispatchContext;

pub(super) struct BeforeToolDirectiveOutcome {
    pub args: serde_json::Value,
    pub short_circuit: Option<ToolResult>,
}

pub(super) async fn apply_before_tool_directives(
    context: &ToolDispatchContext<'_>,
    mut args: serde_json::Value,
    directives: Vec<PluginOwned<PluginDirective>>,
) -> BeforeToolDirectiveOutcome {
    let mut short_circuit = None;
    for emitted in directives {
        let plugin_id = emitted.plugin_id;
        match emitted.value {
            PluginDirective::CreateSession { request } => {
                if let Err(err) = context.session_lifecycle.create_session(*request).await {
                    short_circuit = Some(ToolResult::err_fmt(err.to_string()));
                    break;
                }
            }
            PluginDirective::ReplaceToolArgs { args: replacement } => {
                args = replacement;
            }
            PluginDirective::ShortCircuitTool { output } => {
                short_circuit = Some(ToolResult::from_output(output));
            }
            PluginDirective::AbortTurn { message, .. } => {
                short_circuit = Some(ToolResult::err_fmt(message));
            }
            PluginDirective::EmitRuntimeEvents { events } => {
                emit_plugin_runtime_events(&context.event_tx, &plugin_id, events).await;
            }
            PluginDirective::EmitTrace {
                name,
                payload,
                context: trace_context,
            } => {
                if let Err(err) =
                    emit_trace(context, &plugin_id, name, payload, *trace_context).await
                {
                    short_circuit = Some(ToolResult::err_fmt(err));
                    break;
                }
            }
            PluginDirective::EnqueueMessages { .. } => {
                short_circuit = Some(ToolResult::err_fmt(
                    "before_tool_call does not support message injection",
                ));
            }
        }
    }

    BeforeToolDirectiveOutcome {
        args,
        short_circuit,
    }
}

pub(super) async fn apply_after_tool_directives(
    context: &ToolDispatchContext<'_>,
    mut result: ToolResult,
    directives: Vec<PluginOwned<PluginDirective>>,
) -> ToolResult {
    for emitted in directives {
        let plugin_id = emitted.plugin_id;
        match emitted.value {
            PluginDirective::CreateSession { request } => {
                if let Err(err) = context.session_lifecycle.create_session(*request).await {
                    result = ToolResult::failure(ToolFailure::runtime(
                        ToolFailureClass::Internal,
                        "plugin_session_create_failed",
                        err.to_string(),
                    ));
                    break;
                }
            }
            PluginDirective::ShortCircuitTool { output } => {
                result = ToolResult::from_output(output);
            }
            PluginDirective::AbortTurn { message, .. } => {
                result = ToolResult::err_fmt(message);
            }
            PluginDirective::EmitRuntimeEvents { events } => {
                emit_plugin_runtime_events(&context.event_tx, &plugin_id, events).await;
            }
            PluginDirective::EmitTrace {
                name,
                payload,
                context: trace_context,
            } => {
                if let Err(err) =
                    emit_trace(context, &plugin_id, name, payload, *trace_context).await
                {
                    result = ToolResult::err_fmt(err);
                    break;
                }
            }
            PluginDirective::EnqueueMessages { messages } => {
                context.checkpoint_messages.enqueue(messages);
            }
            PluginDirective::ReplaceToolArgs { .. } => {
                result = ToolResult::err_fmt(
                    "after_tool_call only supports abort, short-circuit, session creation, events, and message injection",
                );
            }
        }
    }
    result
}

async fn emit_trace(
    context: &ToolDispatchContext<'_>,
    plugin_id: &str,
    name: String,
    payload: serde_json::Value,
    trace_context: lash_trace::TraceContext,
) -> Result<(), String> {
    context
        .session_graph
        .emit_trace_event(
            trace_context,
            lash_trace::TraceEvent::Custom {
                name: format!("plugin.{plugin_id}.{name}"),
                payload,
            },
        )
        .await
        .map_err(|err| err.to_string())
}
