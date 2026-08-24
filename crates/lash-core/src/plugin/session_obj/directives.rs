use lash_sansio::core_support::*;
use std::sync::Arc;

use super::*;
use crate::session_model::plugin_message_to_message;

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

async fn interpret_ambient(
    emitted: PluginOwned<PluginDirective>,
    session_lifecycle: &Arc<dyn SessionLifecycleService>,
    session_graph: &Arc<dyn SessionGraphService>,
) -> Result<crate::plugin::AmbientDirectiveAction, PluginError> {
    crate::plugin::interpret_ambient_directive(emitted, session_lifecycle, session_graph)
        .await
        .map_err(|error| error.into_plugin_error())
}

async fn interpret_directive(
    emitted: PluginOwned<TurnPluginDirective>,
    session_lifecycle: &Arc<dyn SessionLifecycleService>,
    session_graph: &Arc<dyn SessionGraphService>,
) -> Result<DirectiveAction, PluginError> {
    let PluginOwned { plugin_id, value } = emitted;
    match value {
        TurnPluginDirective::Ambient(directive) => {
            match interpret_ambient(
                PluginOwned {
                    plugin_id,
                    value: directive,
                },
                session_lifecycle,
                session_graph,
            )
            .await?
            {
                crate::plugin::AmbientDirectiveAction::EmitRuntimeEvents { plugin_id, events } => {
                    Ok(DirectiveAction::EmitRuntimeEvents(
                        crate::plugin::plugin_runtime_session_events(&plugin_id, events),
                    ))
                }
                crate::plugin::AmbientDirectiveAction::None => Ok(DirectiveAction::None),
            }
        }
        TurnPluginDirective::AbortTurn(directive) => Ok(DirectiveAction::Abort(PluginAbort {
            code: directive.code,
            message: directive.message,
        })),
        TurnPluginDirective::EnqueueMessages(directive) => {
            Ok(DirectiveAction::EnqueueMessages(directive.messages))
        }
    }
}

impl PluginSession {
    async fn apply_turn_directives(
        &self,
        directives: Vec<PluginOwned<TurnPluginDirective>>,
        mut messages: crate::MessageSequence,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        message_scope_id: &str,
    ) -> Result<TurnPreparation, PluginError> {
        let mut events = Vec::new();
        let mut abort = None;
        let mut next_message_ordinal = 0usize;

        for emitted in directives {
            match interpret_directive(emitted, &session_lifecycle, &session_graph).await? {
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
            match interpret_directive(emitted, &ctx.session_lifecycle, &ctx.session_graph).await? {
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
                    turn: Arc::new(crate::plugin::TurnHookReport::from_assembled(&turn)),
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
            let PluginOwned { plugin_id, value } = emitted;
            match value {
                AfterTurnPluginDirective::Ambient(directive) => {
                    match interpret_ambient(
                        PluginOwned {
                            plugin_id,
                            value: directive,
                        },
                        &session_lifecycle,
                        &session_graph,
                    )
                    .await?
                    {
                        crate::plugin::AmbientDirectiveAction::EmitRuntimeEvents {
                            plugin_id,
                            events: next_events,
                        } => {
                            events.extend(crate::plugin::plugin_runtime_session_events(
                                &plugin_id,
                                next_events,
                            ));
                        }
                        crate::plugin::AmbientDirectiveAction::None => {}
                    }
                }
                AfterTurnPluginDirective::EnqueueMessages(directive) => {
                    let messages = updated_messages.get_or_insert_with(|| {
                        crate::MessageSequence::from_base(
                            turn.state.read_view().messages().to_vec().into(),
                        )
                    });
                    append_plugin_messages(
                        messages,
                        &directive.messages,
                        &format!("{turn_scope_id}:after_turn"),
                        &mut next_message_ordinal,
                    );
                }
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
