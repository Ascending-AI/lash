use lash_sansio::core_support::*;
use std::sync::Arc;

use super::*;
use crate::session_model::plugin_message_to_message;

#[derive(Clone, Copy)]
struct PluginDirectivePolicy {
    abort_error: Option<&'static str>,
    tool_directive_error: &'static str,
}

impl PluginDirectivePolicy {
    const BEFORE_TURN: Self = Self {
        abort_error: None,
        tool_directive_error: "tool directives are not valid in before_turn",
    };

    const CHECKPOINT: Self = Self {
        abort_error: None,
        tool_directive_error: "checkpoint hooks only support abort, message enqueue, session creation, events, and trace events",
    };

    const AFTER_TURN: Self = Self {
        abort_error: Some("only message enqueue and session creation are valid in after_turn"),
        tool_directive_error: "only message enqueue, session creation, events, and trace events are valid in after_turn",
    };
}

enum DirectiveAction {
    Abort(PluginAbort),
    EnqueueMessages(Vec<PluginMessage>),
    EmitRuntimeEvents(Vec<crate::SessionStreamEvent>),
    None,
}

fn append_plugin_messages(
    messages: &mut crate::MessageSequence,
    plugin_messages: &[PluginMessage],
    scope_id: &str,
    next_ordinal: &mut usize,
) {
    let new_messages = plugin_messages
        .iter()
        .filter(|message| matches!(message.role, MessageRole::User | MessageRole::System))
        .map(|message| {
            let ordinal = *next_ordinal;
            *next_ordinal += 1;
            plugin_message_to_message(message, &format!("m_plugin_{scope_id}_{ordinal}"))
        })
        .collect::<Vec<_>>();
    if !new_messages.is_empty() {
        messages.extend(new_messages);
    }
}

async fn interpret_directive(
    emitted: PluginOwned<PluginDirective>,
    session_lifecycle: &Arc<dyn SessionLifecycleService>,
    session_graph: &Arc<dyn SessionGraphService>,
    policy: PluginDirectivePolicy,
) -> Result<DirectiveAction, PluginError> {
    match emitted.value {
        PluginDirective::AbortTurn { code, message } => {
            if let Some(error) = policy.abort_error {
                return Err(PluginError::Session(error.to_string()));
            }
            Ok(DirectiveAction::Abort(PluginAbort { code, message }))
        }
        PluginDirective::EnqueueMessages { messages } => {
            Ok(DirectiveAction::EnqueueMessages(messages))
        }
        PluginDirective::CreateSession { request } => {
            session_lifecycle
                .create_session(*request)
                .await
                .map_err(|err| PluginError::Session(err.to_string()))?;
            Ok(DirectiveAction::None)
        }
        PluginDirective::EmitRuntimeEvents { events: surface } => {
            Ok(DirectiveAction::EmitRuntimeEvents(
                crate::plugin::plugin_runtime_session_events(&emitted.plugin_id, surface),
            ))
        }
        PluginDirective::EmitTrace {
            name,
            payload,
            context,
        } => {
            session_graph
                .emit_trace_event(
                    *context,
                    lash_trace::TraceEvent::Custom {
                        name: format!("plugin.{}.{}", emitted.plugin_id, name),
                        payload,
                    },
                )
                .await?;
            Ok(DirectiveAction::None)
        }
        PluginDirective::ReplaceToolArgs { .. } | PluginDirective::ShortCircuitTool { .. } => Err(
            PluginError::Session(policy.tool_directive_error.to_string()),
        ),
    }
}

impl PluginSession {
    async fn apply_turn_directives(
        &self,
        directives: Vec<PluginOwned<PluginDirective>>,
        mut messages: crate::MessageSequence,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        policy: PluginDirectivePolicy,
        message_scope_id: &str,
    ) -> Result<TurnPreparation, PluginError> {
        let mut events = Vec::new();
        let mut abort = None;
        let mut next_message_ordinal = 0usize;

        for emitted in directives {
            match interpret_directive(emitted, &session_lifecycle, &session_graph, policy).await? {
                DirectiveAction::Abort(next) => abort = Some(next),
                DirectiveAction::EnqueueMessages(plugin_messages) => {
                    append_plugin_messages(
                        &mut messages,
                        &plugin_messages,
                        message_scope_id,
                        &mut next_message_ordinal,
                    );
                }
                DirectiveAction::EmitRuntimeEvents(next_events) => events.extend(next_events),
                DirectiveAction::None => {}
            }
        }

        Ok(TurnPreparation {
            messages,
            events,
            abort,
        })
    }

    pub async fn prepare_turn_with_phase_probe(
        &self,
        request: PrepareTurnRequest,
        phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
        turn_scope_id: &str,
    ) -> Result<TurnPreparation, PluginError> {
        let PrepareTurnRequest {
            session_id,
            state,
            messages,
            sessions,
            session_lifecycle,
            session_graph,
            turn_context,
        } = request;
        let directives = self
            .before_turn_with_phase_probe(
                TurnHookContext {
                    session_id,
                    state,
                    sessions,
                    turn_context,
                },
                phase_probe.as_ref(),
            )
            .await?;
        self.apply_turn_directives(
            directives,
            messages,
            session_lifecycle,
            session_graph,
            PluginDirectivePolicy::BEFORE_TURN,
            &format!("{turn_scope_id}:before_turn"),
        )
        .await
    }

    pub async fn apply_checkpoint(
        &self,
        ctx: CheckpointHookContext,
    ) -> Result<CheckpointApplication, PluginError> {
        let directives = self.at_checkpoint(ctx.clone()).await?;
        let mut messages = Vec::new();
        let mut events = Vec::new();
        let mut abort = None;

        for emitted in directives {
            match interpret_directive(
                emitted,
                &ctx.session_lifecycle,
                &ctx.session_graph,
                PluginDirectivePolicy::CHECKPOINT,
            )
            .await?
            {
                DirectiveAction::Abort(next) => abort = Some(next),
                DirectiveAction::EnqueueMessages(queued) => messages.extend(queued),
                DirectiveAction::EmitRuntimeEvents(next_events) => events.extend(next_events),
                DirectiveAction::None => {}
            }
        }

        Ok(CheckpointApplication {
            messages,
            events,
            abort,
        })
    }

    pub async fn finalize_turn_with_phase_probe(
        &self,
        mut turn: AssembledTurn,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
        turn_scope_id: &str,
    ) -> Result<TurnFinalization, PluginError> {
        let session_id = turn.state.session_id.clone();
        let directives = if self.contributions.after_turn_hooks.is_empty() {
            Vec::new()
        } else {
            self.after_turn_with_phase_probe(
                TurnResultHookContext {
                    session_id: session_id.clone(),
                    turn: Arc::new(crate::plugin::TurnResultSummary::from_assembled(&turn)),
                    sessions,
                },
                phase_probe.as_ref(),
            )
            .await?
        };
        let mut events = Vec::new();
        let mut updated_messages: Option<crate::MessageSequence> = None;
        let mut next_message_ordinal = 0usize;
        for emitted in directives {
            match interpret_directive(
                emitted,
                &session_lifecycle,
                &session_graph,
                PluginDirectivePolicy::AFTER_TURN,
            )
            .await?
            {
                DirectiveAction::Abort(_) => unreachable!("after_turn policy rejects abort"),
                DirectiveAction::EnqueueMessages(plugin_messages) => {
                    let messages = updated_messages.get_or_insert_with(|| {
                        crate::MessageSequence::from_base(
                            turn.state.read_view().messages().to_vec().into(),
                        )
                    });
                    append_plugin_messages(
                        messages,
                        &plugin_messages,
                        &format!("{turn_scope_id}:after_turn"),
                        &mut next_message_ordinal,
                    );
                }
                DirectiveAction::EmitRuntimeEvents(next_events) => events.extend(next_events),
                DirectiveAction::None => {}
            }
        }
        if let Some(messages) = updated_messages.as_ref() {
            turn.state.replace_active_read_state(messages.as_slice());
        }

        if self.has_runtime_event_hooks()
            && let Err(error) = self
                .emit_runtime_event_with_phase_probe(
                    PluginLifecycleEvent::TurnFinalized(Arc::new(turn.clone())),
                    phase_probe,
                )
                .await
        {
            turn.errors.push(super::plugin_lifecycle_hook_issue(error));
        }

        Ok(TurnFinalization { turn, events })
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn plugin_fallback_message_id_is_scoped_to_the_turn_phase() {
        let mut messages = crate::MessageSequence::default();
        let mut next_ordinal = 0;
        append_plugin_messages(
            &mut messages,
            &[
                PluginMessage::text(MessageRole::User, "same"),
                PluginMessage::text(MessageRole::System, "same"),
            ],
            "turn-42:before_turn",
            &mut next_ordinal,
        );
        assert_eq!(messages[0].id, "m_plugin_turn-42:before_turn_0");
        assert_eq!(messages[1].id, "m_plugin_turn-42:before_turn_1");
    }
}
