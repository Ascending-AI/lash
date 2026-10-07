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
}

impl PluginSession {
    /// Run every after-turn callback over `ctx`, in registration order, for
    /// the turn whose commit `address` names, and return their decisions
    /// with the resolutions of the state commands they returned, staged to
    /// commit with that turn: nothing they change is published until the
    /// commit is acknowledged.
    ///
    /// # Errors
    ///
    /// A callback's failure, or a namespace a refused publication fenced:
    /// nothing is staged.
    pub async fn after_turn_decisions(
        self: &Arc<Self>,
        phase_probe: Option<&Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
        ctx: TurnResultHookContext,
        address: crate::EffectAddress,
    ) -> Result<AfterTurnDecisions, PluginError> {
        let segment = self.state_segment();
        let (contributions, proposals) =
            super::state::collect_proposals(self, self.dispatch(phase_probe).after_turn(ctx)).await;
        let decisions = recorded_contributions(
            contributions?,
            |AfterTurnContributions {
                 events,
                 records,
                 session,
                 ..
             }| (events, records, session),
        );
        let resolutions = Box::pin(self.reduce_proposals(&address, segment, proposals)).await?;
        Ok(AfterTurnDecisions {
            decisions,
            state: super::EffectPublication::begin(Arc::clone(self), address).stage(resolutions),
        })
    }
}
