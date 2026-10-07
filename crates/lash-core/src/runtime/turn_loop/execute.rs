//! The before-turn hooks of a turn's preparation (ADR 0132 §4): run as one
//! recorded step whose decisions are the turn's input.

use super::*;
use crate::ActorContext;

/// The preamble step of the execute phase: the plugin prepare-turn hooks and
/// the context transform that produce the message sequence the driver runs.
pub(super) struct TurnPreambleContext<'preamble, 'run> {
    pub(super) plugins: &'preamble Arc<crate::PluginSession>,
    pub(super) scoped_effect_controller: &'preamble ActorContext,
    pub(super) manager: &'preamble Arc<RuntimeSessionServices>,
    pub(super) messages: crate::MessageSequence,
    pub(super) turn_policy: &'preamble crate::SessionPolicy,
    pub(super) effective_protocol_turn_options: &'preamble crate::ProtocolTurnOptions,
    pub(super) turn_context: &'preamble crate::TurnContext,
    pub(super) turn_scope_id: &'preamble str,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(super) run: std::marker::PhantomData<&'run ()>,
}

impl LashRuntime {
    /// Run the turn's before-turn callbacks as one recorded step: their
    /// decisions and the resolutions of their state commands are served from
    /// the journal on replay, and no callback runs again (K10).
    pub(super) async fn prepare_turn_preamble(
        &mut self,
        context: TurnPreambleContext<'_, '_>,
    ) -> Result<crate::plugin::TurnPreparation, RuntimeError> {
        let TurnPreambleContext {
            run: std::marker::PhantomData,
            plugins,
            scoped_effect_controller,
            manager,
            messages,
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
        let prepared = crate::PluginSession::apply_before_turn(recorded, messages, turn_scope_id);
        self.mark_phase_end(RuntimeTurnPhase::BeforeTurnHooks);
        Ok(prepared)
    }
}
