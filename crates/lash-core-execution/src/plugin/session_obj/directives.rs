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

/// The recorded decisions of a sequential turn callback slot: everything each
/// callback contributed but its state commands, which the recorded body
/// running them carries as resolutions.
fn recorded_contributions<O>(
    contributions: Vec<PluginOwned<O>>,
    split: impl Fn(
        O,
    ) -> (
        Vec<PluginMessage>,
        Vec<PluginRuntimeEvent>,
        Vec<PluginRecordContribution>,
    ),
) -> Vec<RecordedTurnContribution> {
    contributions
        .into_iter()
        .map(|PluginOwned { plugin_id, value }| {
            let (messages, events, records) = split(value);
            RecordedTurnContribution {
                plugin_id,
                messages,
                events,
                records,
            }
        })
        .collect()
}

impl PluginSession {
    /// Apply before-turn decisions in recorded callback order.
    pub fn apply_before_turn(
        recorded: Vec<RecordedTurnContribution>,
        mut messages: crate::MessageSequence,
        turn_scope_id: &str,
    ) -> TurnPreparation {
        let message_scope_id = format!("{turn_scope_id}:before_turn");
        let mut events = Vec::new();
        let mut next_message_ordinal = 0usize;
        for RecordedTurnContribution {
            plugin_id,
            messages: plugin_messages,
            events: plugin_events,
            ..
        } in recorded
        {
            append_plugin_messages(
                &mut messages,
                &plugin_messages,
                &message_scope_id,
                &mut next_message_ordinal,
            );
            events.extend(crate::plugin::plugin_runtime_session_events(
                &plugin_id,
                plugin_events,
            ));
        }
        TurnPreparation { messages, events }
    }

    /// Whether any before-turn callback is registered: a turn records the
    /// slot's decisions only then.
    pub fn has_before_turn_hooks(&self) -> bool {
        !self
            .capabilities()
            .contributions
            .before_turn_hooks
            .is_empty()
    }

    /// Whether any after-turn callback is registered: a turn records the
    /// slot's decisions only then.
    pub fn has_after_turn_hooks(&self) -> bool {
        !self
            .capabilities()
            .contributions
            .after_turn_hooks
            .is_empty()
    }

    /// Run the checkpoint callbacks inside the checkpoint's recorded body:
    /// their state commands publish with its outcome.
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
    /// Run every before-turn callback, in recorded registration order, and
    /// return their decisions. Their state commands go to the recorded body
    /// this runs in, which carries them with these decisions.
    pub async fn before_turn_decisions(
        &self,
        ctx: TurnHookContext,
    ) -> Result<Vec<RecordedTurnContribution>, PluginError> {
        Ok(recorded_contributions(
            self.before_turn(ctx).await?,
            |TurnContributions {
                 messages, events, ..
             }| (messages, events, Vec::new()),
        ))
    }

    /// Run every after-turn callback over `turn`, in recorded registration
    /// order, and return their decisions. Their state commands go to the
    /// recorded body this runs in, which carries them with these decisions.
    pub async fn after_turn_decisions(
        &self,
        ctx: TurnResultHookContext,
    ) -> Result<Vec<RecordedTurnContribution>, PluginError> {
        Ok(recorded_contributions(
            self.after_turn(ctx).await?,
            |AfterTurnContributions {
                 messages,
                 events,
                 records,
                 ..
             }| (messages, events, records),
        ))
    }

    /// Apply the after-turn decisions to `turn`, in recorded callback order,
    /// then deliver the finalized turn to the lifecycle observers.
    pub async fn finalize_turn(
        &self,
        mut turn: AssembledTurn,
        recorded: Vec<RecordedTurnContribution>,
        turn_scope_id: &str,
        clock: &dyn crate::Clock,
    ) -> TurnFinalization {
        let mut events = Vec::new();
        let mut updated_messages: Option<crate::MessageSequence> = None;
        let mut next_message_ordinal = 0usize;
        let mut next_plugin_ordinal = 0usize;
        for RecordedTurnContribution {
            plugin_id,
            messages,
            events: plugin_events,
            records,
        } in recorded
        {
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

        TurnFinalization { turn, events }
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
