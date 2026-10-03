use lash_sansio::core_support::*;
use std::sync::Arc;

use super::*;
use crate::session_model::plugin_message_to_message;

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

impl PluginSession {
    /// Apply before-turn contributions in recorded callback order.
    fn apply_turn_contributions(
        contributions: Vec<PluginOwned<TurnContributions>>,
        mut messages: crate::MessageSequence,
        message_scope_id: &str,
    ) -> TurnPreparation {
        let mut events = Vec::new();
        let mut next_message_ordinal = 0usize;
        for PluginOwned { plugin_id, value } in contributions {
            append_plugin_messages(
                &mut messages,
                &value.messages,
                message_scope_id,
                &mut next_message_ordinal,
            );
            events.extend(crate::plugin::plugin_runtime_session_events(
                &plugin_id,
                value.events,
            ));
        }
        TurnPreparation { messages, events }
    }

    pub async fn apply_checkpoint(
        &self,
        ctx: CheckpointHookContext,
    ) -> Result<CheckpointApplication, PluginError> {
        let contributions = self.at_checkpoint(ctx).await?;
        let mut messages = Vec::new();
        let mut events = Vec::new();
        for PluginOwned { plugin_id, value } in contributions {
            messages.extend(value.messages);
            events.extend(crate::plugin::plugin_runtime_session_events(
                &plugin_id,
                value.events,
            ));
        }
        Ok(CheckpointApplication { messages, events })
    }
}

impl PluginDispatchContext<'_> {
    pub async fn prepare_turn(
        &self,
        request: PrepareTurnRequest,
        turn_scope_id: &str,
    ) -> Result<TurnPreparation, PluginError> {
        let PrepareTurnRequest {
            session_id,
            state,
            messages,
            sessions,
            turn_context,
        } = request;
        let contributions = self
            .before_turn(TurnHookContext {
                session_id,
                plugin_config: self.session.admitted_plugin_config(),
                state,
                sessions,
                turn_context,
            })
            .await?;
        Ok(PluginSession::apply_turn_contributions(
            contributions,
            messages,
            &format!("{turn_scope_id}:before_turn"),
        ))
    }

    pub async fn finalize_turn(
        &self,
        mut turn: AssembledTurn,
        sessions: Arc<dyn SessionStateService>,
        session_graph: Arc<dyn SessionGraphService>,
        turn_scope_id: &str,
        clock: &dyn crate::Clock,
    ) -> Result<TurnFinalization, PluginError> {
        let session_id = turn.state.session_id.clone();
        let contributions = if self
            .session
            .capabilities()
            .contributions
            .after_turn_hooks
            .is_empty()
        {
            Vec::new()
        } else {
            self.after_turn(TurnResultHookContext {
                session_id: session_id.clone(),
                plugin_config: self.session.admitted_plugin_config(),
                turn: Arc::new(crate::plugin::TurnHookReport::from_assembled(&turn)),
                sessions,
                session_graph: Arc::clone(&session_graph),
            })
            .await?
        };
        let mut events = Vec::new();
        let mut updated_messages: Option<crate::MessageSequence> = None;
        let mut next_message_ordinal = 0usize;
        let mut next_plugin_ordinal = 0usize;
        for PluginOwned { plugin_id, value } in contributions {
            let AfterTurnContributions {
                messages,
                events: plugin_events,
                records,
            } = value;
            events.extend(crate::plugin::plugin_runtime_session_events(
                &plugin_id,
                plugin_events,
            ));
            if !records.is_empty()
                && let Some(messages) = updated_messages.take()
            {
                turn.state.replace_active_read_state(messages.as_slice());
            }
            for PluginRecordContribution { plugin_type, body } in records {
                turn.state.session_graph.append_node_drafts_at(
                    &format!("{turn_scope_id}:after_turn:{plugin_id}:plugin:{next_plugin_ordinal}"),
                    [crate::session_graph::SessionNodeDraft::plugin(
                        plugin_type,
                        body,
                    )],
                    clock.timestamp_rfc3339(),
                );
                next_plugin_ordinal += 1;
            }
            if !messages.is_empty() {
                let messages_so_far = updated_messages.get_or_insert_with(|| {
                    let read_view = turn.state.read_view();
                    crate::MessageSequence::from_base(read_view.messages().to_vec().into())
                });
                append_plugin_messages(
                    messages_so_far,
                    &messages,
                    &format!("{turn_scope_id}:after_turn"),
                    &mut next_message_ordinal,
                );
            }
        }
        if let Some(messages) = updated_messages.as_ref() {
            turn.state.replace_active_read_state(messages.as_slice());
        }

        if self.session.has_runtime_event_hooks()
            && let Err(error) = self
                .emit_runtime_event(PluginLifecycleEvent::TurnFinalized(Arc::new(turn.clone())))
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
