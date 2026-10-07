use std::sync::Arc;

use super::*;

/// The recorded decisions of a sequential turn callback slot: everything each
/// callback contributed but its state commands, which the recorded body
/// running them carries as resolutions.
fn recorded_contributions<O>(
    contributions: Vec<PluginOwned<O>>,
    split: impl Fn(
        O,
    ) -> (
        Vec<PluginRuntimeEvent>,
        Vec<PluginRecordContribution>,
        SessionContributions,
    ),
) -> Vec<RecordedTurnContribution> {
    contributions
        .into_iter()
        .map(|PluginOwned { plugin_id, value }| {
            let (events, records, session) = split(value);
            RecordedTurnContribution {
                plugin_id,
                events,
                records,
                session,
            }
        })
        .collect()
}

impl PluginSession {
    /// Apply before-turn decisions in recorded callback order: their session
    /// changes and runtime events.
    pub fn apply_before_turn(recorded: Vec<RecordedTurnContribution>) -> TurnPreparation {
        let mut events = Vec::new();
        let mut session = Vec::new();
        for RecordedTurnContribution {
            plugin_id,
            events: plugin_events,
            session: session_changes,
            ..
        } in recorded
        {
            session.push(session_changes);
            events.extend(crate::plugin::plugin_runtime_session_events(
                &plugin_id,
                plugin_events,
            ));
        }
        TurnPreparation { session, events }
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
        let mut events = Vec::new();
        let mut session = Vec::new();
        for PluginOwned { plugin_id, value } in contributions {
            session.push(value.session);
            events.extend(crate::plugin::plugin_runtime_session_events(
                &plugin_id,
                value.events,
            ));
        }
        Ok(CheckpointApplication { events, session })
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
                 events, session, ..
             }| (events, Vec::new(), session),
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
                 events,
                 records,
                 session,
                 ..
             }| (events, records, session),
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
        let mut next_plugin_ordinal = 0usize;
        for RecordedTurnContribution {
            plugin_id,
            events: plugin_events,
            records,
            ..
        } in recorded
        {
            events.extend(crate::plugin::plugin_runtime_session_events(
                &plugin_id,
                plugin_events,
            ));
            for PluginRecordContribution { plugin_type, body } in records {
                turn.state.session_graph.append_node_drafts_at(
                    &format!("{turn_scope_id}:after_turn:{plugin_id}:plugin:{next_plugin_ordinal}"),
                    [crate::session_graph::SessionNodeDraft::plugin(
                        plugin_type,
                        body,
                    )],
                    clock.node_timestamp(),
                );
                next_plugin_ordinal += 1;
            }
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
