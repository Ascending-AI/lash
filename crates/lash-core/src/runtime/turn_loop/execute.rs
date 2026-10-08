//! The before-turn hooks of a fresh turn's preparation (ADR 0132 §4): run
//! as one step whose decisions are the turn's input. Every phase of the
//! turn commits the decisions with its checkpoint, and a resumed turn is
//! prepared from them: no callback runs again (ADR 0133 §6).

use super::*;
use crate::ActorContext;

/// The preamble step of the execute phase: the plugin before-turn hooks.
pub(super) struct TurnPreambleContext<'preamble, 'run> {
    pub(super) plugins: &'preamble Arc<crate::PluginSession>,
    pub(super) scoped_effect_controller: &'preamble ActorContext,
    pub(super) manager: &'preamble Arc<RuntimeSessionServices>,
    pub(super) turn_policy: &'preamble crate::SessionPolicy,
    pub(super) effective_protocol_turn_options: &'preamble crate::ProtocolTurnOptions,
    pub(super) turn_context: &'preamble crate::TurnContext,
    pub(super) turn_scope_id: &'preamble str,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(super) run: std::marker::PhantomData<&'run ()>,
}

impl LashRuntime {
    /// Run the fresh turn's before-turn callbacks as one step and answer
    /// their decisions, their state commands published.
    pub(super) async fn prepare_turn_preamble(
        &mut self,
        context: TurnPreambleContext<'_, '_>,
    ) -> Result<Vec<crate::plugin::RecordedTurnContribution>, RuntimeError> {
        let TurnPreambleContext {
            run: std::marker::PhantomData,
            plugins,
            scoped_effect_controller,
            manager,
            turn_policy,
            effective_protocol_turn_options,
            turn_context,
            turn_scope_id,
        } = context;
        self.mark_phase_begin(RuntimeTurnPhase::BeforeTurnHooks);
        let recorded = if plugins.has_before_turn_hooks() {
            let hook_context = crate::plugin::TurnHookContext {
                session_id: self.state.session_id.clone(),
                plugin_config: plugins.admitted_plugin_config(),
                state: crate::SessionReadView::from_runtime_state(
                    &self.state,
                    turn_policy.clone(),
                    effective_protocol_turn_options.clone(),
                ),
                sessions: manager.read_service(),
                turn_context: turn_context.clone(),
            };
            let callbacks = Arc::clone(plugins);
            let probe = self.turn_phase_probe.clone();
            let step = format!("plugin-callbacks:before-turn:{turn_scope_id}");
            let recorded = Box::pin(crate::plugin::record_plugin_callbacks(
                scoped_effect_controller,
                crate::RuntimeAttribution::for_session(self.state.session_id.clone()),
                step,
                crate::plugin::RecordedCallbackPhase::BeforeTurn,
                Arc::clone(plugins),
                Box::pin(async move {
                    callbacks
                        .dispatch(probe.as_ref())
                        .before_turn_decisions(hook_context)
                        .await
                }),
            ))
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
            recorded.map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginPrepareTurn))?
        } else {
            Vec::new()
        };
        self.mark_phase_end(RuntimeTurnPhase::BeforeTurnHooks);
        Ok(recorded)
    }
}
