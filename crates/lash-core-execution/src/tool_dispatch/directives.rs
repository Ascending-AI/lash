use crate::plugin::{
    AfterToolCallPluginDirective, AmbientDirectiveAction, AmbientDirectiveError,
    BeforeToolCallPluginDirective, PluginOwned, PluginTerminalStrength as ToolTerminalKind,
    interpret_ambient_directive,
};
use crate::{ToolFailure, ToolFailureClass, ToolOutcome};

use super::context::ToolDispatchContext;

pub(super) struct BeforeToolDirectiveOutcome {
    pub args: serde_json::Value,
    pub short_circuit: Option<ToolOutcome>,
}

struct ToolTerminal {
    plugin_id: String,
    kind: ToolTerminalKind,
    result: ToolOutcome,
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
        directives: Vec<PluginOwned<BeforeToolCallPluginDirective>>,
    ) {
        for emitted in directives {
            let plugin_id = emitted.plugin_id;
            match emitted.value {
                BeforeToolCallPluginDirective::Ambient(directive) => {
                    match interpret_ambient_directive(
                        PluginOwned {
                            plugin_id: plugin_id.clone(),
                            value: directive,
                        },
                        &context.session_lifecycle,
                        &context.session_graph,
                    )
                    .await
                    {
                        Ok(action) => apply_ambient_action(context, action).await,
                        Err(error) => {
                            self.fold_terminal(
                                context,
                                ToolTerminal {
                                    plugin_id,
                                    kind: ToolTerminalKind::DeniedShortCircuit,
                                    result: ToolOutcome::err_fmt(error.message()),
                                },
                            )
                            .await;
                        }
                    }
                }
                BeforeToolCallPluginDirective::ReplaceToolArgs(directive) => {
                    self.args = directive.args;
                }
                BeforeToolCallPluginDirective::ShortCircuitTool(directive) => {
                    let kind = if directive.output.is_success() {
                        ToolTerminalKind::SuccessfulShortCircuit
                    } else {
                        ToolTerminalKind::DeniedShortCircuit
                    };
                    self.fold_terminal(
                        context,
                        ToolTerminal {
                            plugin_id,
                            kind,
                            result: ToolOutcome::from_output(directive.output),
                        },
                    )
                    .await;
                }
                BeforeToolCallPluginDirective::AbortTurn(directive) => {
                    self.fold_terminal(
                        context,
                        ToolTerminal {
                            plugin_id,
                            kind: ToolTerminalKind::AbortTurn,
                            result: ToolOutcome::err_fmt(directive.message),
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
    directives: Vec<PluginOwned<BeforeToolCallPluginDirective>>,
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
    result: ToolOutcome,
    directives: Vec<PluginOwned<AfterToolCallPluginDirective>>,
) -> ToolOutcome {
    let mut terminal = None;
    for emitted in directives {
        let plugin_id = emitted.plugin_id;
        match emitted.value {
            AfterToolCallPluginDirective::Ambient(directive) => {
                match interpret_ambient_directive(
                    PluginOwned {
                        plugin_id: plugin_id.clone(),
                        value: directive,
                    },
                    &context.session_lifecycle,
                    &context.session_graph,
                )
                .await
                {
                    Ok(action) => apply_ambient_action(context, action).await,
                    Err(error) => {
                        let result = match error {
                            AmbientDirectiveError::CreateSession(message) => {
                                ToolOutcome::failure(ToolFailure::runtime(
                                    ToolFailureClass::Internal,
                                    "plugin_session_create_failed",
                                    message,
                                ))
                            }
                            AmbientDirectiveError::EmitTrace(error) => {
                                ToolOutcome::err_fmt(error.to_string())
                            }
                        };
                        fold_after_tool_terminal(
                            context,
                            &mut terminal,
                            ToolTerminal {
                                plugin_id,
                                kind: ToolTerminalKind::DeniedShortCircuit,
                                result,
                            },
                        )
                        .await;
                    }
                }
            }
            AfterToolCallPluginDirective::ShortCircuitTool(directive) => {
                let kind = if directive.output.is_success() {
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
                        result: ToolOutcome::from_output(directive.output),
                    },
                )
                .await;
            }
            AfterToolCallPluginDirective::AbortTurn(directive) => {
                fold_after_tool_terminal(
                    context,
                    &mut terminal,
                    ToolTerminal {
                        plugin_id,
                        kind: ToolTerminalKind::AbortTurn,
                        result: ToolOutcome::err_fmt(directive.message),
                    },
                )
                .await;
            }
            AfterToolCallPluginDirective::EnqueueMessages(directive) => {
                context.checkpoint_messages.enqueue(directive.messages);
            }
        }
    }
    terminal.map_or(result, |terminal| terminal.result)
}

async fn apply_ambient_action(context: &ToolDispatchContext<'_>, action: AmbientDirectiveAction) {
    match action {
        AmbientDirectiveAction::EmitRuntimeEvents { plugin_id, events } => {
            crate::plugin::emit_plugin_runtime_events(&context.event_tx, &plugin_id, events).await;
        }
        AmbientDirectiveAction::None => {}
    }
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
