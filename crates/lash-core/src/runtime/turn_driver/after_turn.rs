//! A finished turn's after-turn callbacks and finalized-turn observers
//! (FIG-5283).
//!
//! The after-turn callbacks run once the turn's outcome is known, before its
//! head commit is built, over the turn's durable report: its outcome and the
//! tool calls its rounds committed. What they decide commits in the turn's
//! own `turn.commit`: their records and graph appends join the head commit,
//! and the resolutions of their state commands ride it, staged, and publish
//! only once the commit is acknowledged. A turn redriven before its
//! `turn.commit` landed runs them again over the same report; once it landed
//! the turn is never redriven, so no callback runs again past its decision.
//! The lifecycle observers see the finalized turn after the commit.

use std::sync::Arc;

use super::*;
use crate::runtime::durable::services::RuntimeTurnServices;
use crate::runtime::durable::session::TurnError;

/// A finished turn as its commit records it: its outcome and the tool calls
/// its rounds committed, whichever owners ran them.
pub(super) struct FinishedTurn {
    outcome: TurnOutcome,
    tool_calls: Vec<crate::ToolCallRecord>,
    omitted: Option<crate::OmittedToolCalls>,
}

impl FinishedTurn {
    /// Run `run` of `session` finished with `outcome`. `unrecorded` is the
    /// tool calls of its last cell when no commit followed the cell: the
    /// turn's commit records them, after every round its rows hold, and the
    /// report lists them there (FIG-5330).
    ///
    /// # Errors
    ///
    /// [`TurnError::Durable`] when the turn's tool records do not read.
    pub(super) async fn read(
        cx: &ActorContext,
        session: &SessionId,
        run: &crate::TurnId,
        outcome: &TurnOutcome,
        unrecorded: Option<crate::runtime::durable::session::CellToolCalls>,
    ) -> Result<Self, TurnError> {
        let (mut tool_calls, mut omitted) =
            RuntimeTurnServices::recorded_tool_calls(cx, session, run)
                .await
                .map_err(TurnError::Durable)?;
        if let Some(cell) = unrecorded {
            tool_calls.extend(cell.calls);
            if let Some(left_out) = cell.omitted {
                crate::runtime::durable::session::add_omitted(&mut omitted, left_out);
            }
        }
        Ok(Self {
            outcome: outcome.clone(),
            tool_calls,
            omitted,
        })
    }

    fn report(&self) -> crate::plugin::TurnHookReport {
        crate::plugin::TurnHookReport {
            outcome: self.outcome.clone(),
            execution: Default::default(),
            token_usage: Default::default(),
            tool_calls: Arc::new(self.tool_calls.clone()),
            omitted: self.omitted.clone(),
            errors: Arc::new(Vec::new()),
        }
    }

    /// The finalized turn the lifecycle observers see, over its committed
    /// `state`.
    pub(super) fn finalized(self, state: crate::SessionSnapshot) -> crate::AssembledTurn {
        crate::AssembledTurn {
            state,
            outcome: self.outcome,
            execution: Default::default(),
            token_usage: Default::default(),
            llm_calls: Vec::new(),
            tool_calls: self.tool_calls,
            omitted: self.omitted,
            retained_outputs: Vec::new(),
            failure_evidence: Vec::new(),
            errors: Vec::new(),
            turn_input_acceptance: None,
            turn_cancel_input_outcome: Default::default(),
        }
    }
}

impl RuntimeTurnDriver<'static> {
    /// Run the turn's after-turn callbacks over `turn`, when it has any,
    /// reading the committed head the turn started from: their graph
    /// appends join the turn's draft and their events go to `observer`;
    /// their records and staged state return for the head commit to carry.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] with [`RuntimeErrorCode::PluginFinalizeTurn`] when a
    /// callback failed or its contributions do not apply: nothing of it is
    /// staged, and the turn does not commit.
    pub(super) async fn run_after_turn(
        &mut self,
        turn: &FinishedTurn,
        observer: &TurnObserver,
    ) -> Result<Option<crate::plugin::AfterTurnDecisions>, RuntimeError> {
        let Some(sessions) = self.after_turn_reads.clone() else {
            return Ok(None);
        };
        let plugins = Arc::clone(self.session.plugins());
        let ctx = crate::plugin::TurnResultHookContext {
            writer_formats: Arc::new(crate::protocol_build::FleetWriterFormats(
                self.session.fleet_format(),
            )),
            session_id: self.session_id.clone(),
            plugin_config: plugins.admitted_plugin_config(),
            turn: Arc::new(turn.report()),
            sessions,
        };
        let address = crate::EffectAddress::new(
            self.scoped_effect_controller.execution_scope().clone(),
            format!("plugin-callbacks:after-turn:{}", self.turn_id),
        )
        .map_err(RuntimeEffectControllerError::from)
        .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let mut decided =
            Box::pin(plugins.after_turn_decisions(self.turn_phase_probe.as_ref(), ctx, address))
                .await
                .map_err(|error| error.into_turn_failure(RuntimeErrorCode::PluginFinalizeTurn))?;
        let session = decided
            .decisions
            .iter()
            .map(|decision| decision.session.clone())
            .collect::<Vec<_>>();
        self.turn_pipeline
            .graph_appends()
            .apply_session_contributions(&self.session_id, &plugins, &session)
            .map_err(|error| error.into_turn_failure(RuntimeErrorCode::PluginFinalizeTurn))?;
        for decision in &mut decided.decisions {
            crate::runtime::session_manager::emit_session_events(
                observer,
                crate::plugin::plugin_runtime_session_events(
                    &decision.plugin_id,
                    std::mem::take(&mut decision.events),
                ),
            );
        }
        Ok(Some(decided))
    }
}
