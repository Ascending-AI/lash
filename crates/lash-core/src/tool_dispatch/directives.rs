use crate::plugin::{PluginDirective, PluginOwned, emit_plugin_runtime_events};
use crate::{ToolFailure, ToolFailureClass, ToolResult};

use super::context::ToolDispatchContext;

pub(super) struct BeforeToolDirectiveOutcome {
    pub args: serde_json::Value,
    pub short_circuit: Option<ToolResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ToolTerminalKind {
    SuccessfulShortCircuit,
    DeniedShortCircuit,
    AbortTurn,
}

impl ToolTerminalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::SuccessfulShortCircuit => "successful_short_circuit",
            Self::DeniedShortCircuit => "denied_short_circuit",
            Self::AbortTurn => "abort_turn",
        }
    }
}

struct ToolTerminal {
    plugin_id: String,
    kind: ToolTerminalKind,
    result: ToolResult,
}

#[derive(Clone, Copy)]
enum EqualStrengthResolution {
    PluginId,
    FirstEmitted,
}

pub(super) struct BeforeToolDirectiveFold {
    args: serde_json::Value,
    terminal: Option<ToolTerminal>,
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
                            ToolTerminal {
                                plugin_id,
                                kind: ToolTerminalKind::DeniedShortCircuit,
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
                        ToolTerminalKind::SuccessfulShortCircuit
                    } else {
                        ToolTerminalKind::DeniedShortCircuit
                    };
                    self.fold_terminal(
                        context,
                        ToolTerminal {
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
                        ToolTerminal {
                            plugin_id,
                            kind: ToolTerminalKind::AbortTurn,
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
                            ToolTerminal {
                                plugin_id,
                                kind: ToolTerminalKind::DeniedShortCircuit,
                                result: ToolResult::err_fmt(err),
                            },
                        )
                        .await;
                    }
                }
                PluginDirective::EnqueueMessages { .. } => {
                    self.fold_terminal(
                        context,
                        ToolTerminal {
                            plugin_id,
                            kind: ToolTerminalKind::DeniedShortCircuit,
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

    async fn fold_terminal(&mut self, context: &ToolDispatchContext<'_>, candidate: ToolTerminal) {
        fold_tool_terminal(
            context,
            "before_tool_call",
            &mut self.terminal,
            candidate,
            EqualStrengthResolution::PluginId,
        )
        .await;
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
    hook_name: &'static str,
    later_plugin_id: &str,
    winner: &ToolTerminal,
    ignored: &ToolTerminal,
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
        format!("{hook_name}.directive_conflict"),
        payload.clone(),
        lash_trace::TraceContext::default().for_session(context.session_id.clone()),
    )
    .await
    {
        tracing::error!(
            target: "lash::plugin_composition",
            later_plugin_id,
            error = %err,
            hook_name,
            "failed to emit tool-call directive conflict trace"
        );
    }
    let _ = context
        .event_tx
        .try_send(crate::SessionStreamEvent::PluginEvent {
            plugin_id: later_plugin_id.to_string(),
            event: crate::PluginRuntimeEvent::Custom {
                name: format!("{hook_name}.directive_conflict"),
                payload,
            },
        });
}

pub(super) async fn apply_after_tool_directives(
    context: &ToolDispatchContext<'_>,
    result: ToolResult,
    directives: Vec<PluginOwned<PluginDirective>>,
) -> ToolResult {
    let mut terminal = None;
    for emitted in directives {
        let plugin_id = emitted.plugin_id;
        match emitted.value {
            PluginDirective::CreateSession { request } => {
                if let Err(err) = context.session_lifecycle.create_session(*request).await {
                    fold_after_tool_terminal(
                        context,
                        &mut terminal,
                        ToolTerminal {
                            plugin_id,
                            kind: ToolTerminalKind::DeniedShortCircuit,
                            result: ToolResult::failure(ToolFailure::runtime(
                                ToolFailureClass::Internal,
                                "plugin_session_create_failed",
                                err.to_string(),
                            )),
                        },
                    )
                    .await;
                }
            }
            PluginDirective::ShortCircuitTool { output } => {
                let kind = if output.is_success() {
                    ToolTerminalKind::SuccessfulShortCircuit
                } else {
                    ToolTerminalKind::DeniedShortCircuit
                };
                fold_after_tool_terminal(
                    context,
                    &mut terminal,
                    ToolTerminal {
                        plugin_id,
                        kind,
                        result: ToolResult::from_output(output),
                    },
                )
                .await;
            }
            PluginDirective::AbortTurn { message, .. } => {
                fold_after_tool_terminal(
                    context,
                    &mut terminal,
                    ToolTerminal {
                        plugin_id,
                        kind: ToolTerminalKind::AbortTurn,
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
                    fold_after_tool_terminal(
                        context,
                        &mut terminal,
                        ToolTerminal {
                            plugin_id,
                            kind: ToolTerminalKind::DeniedShortCircuit,
                            result: ToolResult::err_fmt(err),
                        },
                    )
                    .await;
                }
            }
            PluginDirective::EnqueueMessages { messages } => {
                context.checkpoint_messages.enqueue(messages);
            }
            PluginDirective::ReplaceToolArgs { .. } => {
                fold_after_tool_terminal(
                    context,
                    &mut terminal,
                    ToolTerminal {
                        plugin_id,
                        kind: ToolTerminalKind::DeniedShortCircuit,
                        result: ToolResult::err_fmt(
                            "after_tool_call only supports abort, short-circuit, session creation, events, and message injection",
                        ),
                    },
                )
                .await;
            }
        }
    }
    terminal.map_or(result, |terminal| terminal.result)
}

async fn fold_after_tool_terminal(
    context: &ToolDispatchContext<'_>,
    terminal: &mut Option<ToolTerminal>,
    candidate: ToolTerminal,
) {
    fold_tool_terminal(
        context,
        "after_tool_call",
        terminal,
        candidate,
        EqualStrengthResolution::FirstEmitted,
    )
    .await;
}

async fn fold_tool_terminal(
    context: &ToolDispatchContext<'_>,
    hook_name: &'static str,
    terminal: &mut Option<ToolTerminal>,
    candidate: ToolTerminal,
    equal_strength: EqualStrengthResolution,
) {
    let Some(current) = terminal.take() else {
        *terminal = Some(candidate);
        return;
    };
    let later_plugin_id = candidate.plugin_id.clone();
    let candidate_wins = candidate.kind > current.kind
        || (candidate.kind == current.kind
            && matches!(equal_strength, EqualStrengthResolution::PluginId)
            && candidate.plugin_id < current.plugin_id);
    let displaced_denial = (current.kind == ToolTerminalKind::DeniedShortCircuit
        && candidate.kind == ToolTerminalKind::AbortTurn)
        .then(|| current.result.clone());
    let (mut winner, ignored) = if candidate_wins {
        (candidate, current)
    } else {
        (current, candidate)
    };
    if winner.plugin_id != ignored.plugin_id {
        emit_terminal_conflict(context, hook_name, &later_plugin_id, &winner, &ignored).await;
    }
    if let Some(denial) = displaced_denial {
        winner.result = denial;
    }
    *terminal = Some(winner);
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
