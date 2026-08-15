use crate::plugin::{PluginDirective, PluginOwned, emit_plugin_runtime_events};
use crate::{ToolFailure, ToolFailureClass, ToolResult};

use super::context::ToolDispatchContext;

pub(super) struct BeforeToolDirectiveOutcome {
    pub args: serde_json::Value,
    pub short_circuit: Option<ToolResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum BeforeToolTerminalKind {
    SuccessfulShortCircuit,
    DeniedShortCircuit,
    AbortTurn,
}

impl BeforeToolTerminalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::SuccessfulShortCircuit => "successful_short_circuit",
            Self::DeniedShortCircuit => "denied_short_circuit",
            Self::AbortTurn => "abort_turn",
        }
    }
}

struct BeforeToolTerminal {
    plugin_id: String,
    kind: BeforeToolTerminalKind,
    result: ToolResult,
}

pub(super) struct BeforeToolDirectiveFold {
    args: serde_json::Value,
    terminal: Option<BeforeToolTerminal>,
}

impl BeforeToolDirectiveFold {
    pub(super) fn new(args: serde_json::Value) -> Self {
        Self {
            args,
            terminal: None,
        }
    }

    pub(super) async fn apply(
        &mut self,
        context: &ToolDispatchContext<'_>,
        directives: Vec<PluginOwned<PluginDirective>>,
    ) {
        for emitted in directives {
            let plugin_id = emitted.plugin_id;
            match emitted.value {
                PluginDirective::CreateSession { request } => {
                    if let Err(err) = context.session_lifecycle.create_session(*request).await {
                        self.fold_terminal(
                            context,
                            BeforeToolTerminal {
                                plugin_id,
                                kind: BeforeToolTerminalKind::DeniedShortCircuit,
                                result: ToolResult::err_fmt(err.to_string()),
                            },
                        )
                        .await;
                    }
                }
                PluginDirective::ReplaceToolArgs { args: replacement } => {
                    self.args = replacement;
                }
                PluginDirective::ShortCircuitTool { output } => {
                    let kind = if output.is_success() {
                        BeforeToolTerminalKind::SuccessfulShortCircuit
                    } else {
                        BeforeToolTerminalKind::DeniedShortCircuit
                    };
                    self.fold_terminal(
                        context,
                        BeforeToolTerminal {
                            plugin_id,
                            kind,
                            result: ToolResult::from_output(output),
                        },
                    )
                    .await;
                }
                PluginDirective::AbortTurn { message, .. } => {
                    self.fold_terminal(
                        context,
                        BeforeToolTerminal {
                            plugin_id,
                            kind: BeforeToolTerminalKind::AbortTurn,
                            result: ToolResult::err_fmt(message),
                        },
                    )
                    .await;
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
                        self.fold_terminal(
                            context,
                            BeforeToolTerminal {
                                plugin_id,
                                kind: BeforeToolTerminalKind::DeniedShortCircuit,
                                result: ToolResult::err_fmt(err),
                            },
                        )
                        .await;
                    }
                }
                PluginDirective::EnqueueMessages { .. } => {
                    self.fold_terminal(
                        context,
                        BeforeToolTerminal {
                            plugin_id,
                            kind: BeforeToolTerminalKind::DeniedShortCircuit,
                            result: ToolResult::err_fmt(
                                "before_tool_call does not support message injection",
                            ),
                        },
                    )
                    .await;
                }
            }
        }
    }

    async fn fold_terminal(
        &mut self,
        context: &ToolDispatchContext<'_>,
        candidate: BeforeToolTerminal,
    ) {
        let Some(current) = self.terminal.take() else {
            self.terminal = Some(candidate);
            return;
        };
        let later_plugin_id = candidate.plugin_id.clone();
        let candidate_wins = candidate.kind > current.kind
            || (candidate.kind == current.kind && candidate.plugin_id < current.plugin_id);
        let displaced_denial = (current.kind == BeforeToolTerminalKind::DeniedShortCircuit
            && candidate.kind == BeforeToolTerminalKind::AbortTurn)
            .then(|| current.result.clone());
        let (mut winner, ignored) = if candidate_wins {
            (candidate, current)
        } else {
            (current, candidate)
        };
        emit_terminal_conflict(context, &later_plugin_id, &winner, &ignored).await;
        if let Some(denial) = displaced_denial {
            winner.result = denial;
        }
        self.terminal = Some(winner);
    }

    pub(super) fn finish(self) -> BeforeToolDirectiveOutcome {
        BeforeToolDirectiveOutcome {
            args: self.args,
            short_circuit: self.terminal.map(|terminal| terminal.result),
        }
    }
}

pub(super) async fn apply_before_tool_directives(
    context: &ToolDispatchContext<'_>,
    args: serde_json::Value,
    directives: Vec<PluginOwned<PluginDirective>>,
) -> BeforeToolDirectiveOutcome {
    let mut fold = BeforeToolDirectiveFold::new(args);
    fold.apply(context, directives).await;
    fold.finish()
}

async fn emit_terminal_conflict(
    context: &ToolDispatchContext<'_>,
    later_plugin_id: &str,
    winner: &BeforeToolTerminal,
    ignored: &BeforeToolTerminal,
) {
    let payload = serde_json::json!({
        "winner_plugin_id": winner.plugin_id,
        "winner_directive": winner.kind.as_str(),
        "ignored_plugin_id": ignored.plugin_id,
        "ignored_directive": ignored.kind.as_str(),
    });
    if let Err(err) = emit_trace(
        context,
        later_plugin_id,
        "before_tool_call.directive_conflict".to_string(),
        payload.clone(),
        lash_trace::TraceContext::default().for_session(context.session_id.clone()),
    )
    .await
    {
        tracing::error!(
            target: "lash::plugin_composition",
            later_plugin_id,
            error = %err,
            "failed to emit before_tool_call directive conflict trace"
        );
    }
    let _ = context
        .event_tx
        .try_send(crate::SessionStreamEvent::PluginEvent {
            plugin_id: later_plugin_id.to_string(),
            event: crate::PluginRuntimeEvent::Custom {
                name: "before_tool_call.directive_conflict".to_string(),
                payload,
            },
        });
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
