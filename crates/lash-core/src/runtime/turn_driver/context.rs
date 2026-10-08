use super::*;
use crate::PluginError;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;

impl<'run> RuntimeTurnDriver<'run> {
    /// The scope whose cancellation gate this turn's waits race: always the
    /// physical turn's, whatever scope admitted the run, because that is the
    /// gate the turn's own control keys, peeks and forwards stops to
    /// (FIG-3672 P9). A process- or drain-admitted turn is cancelled through
    /// the same gate as a foreground one.
    pub(super) fn turn_cancel_scope(&self) -> crate::ExecutionScope {
        crate::ExecutionScope::turn(self.session_id.clone(), self.turn_id.clone())
    }

    /// The logical Run this physical turn belongs to: the admitted scope's
    /// root, or this turn when it is its own root.
    pub(super) fn logical_run(&self) -> crate::TurnId {
        self.scoped_effect_controller
            .execution_scope()
            .logical_run()
            .unwrap_or_else(|| self.turn_id.clone())
    }

    pub(super) fn execution_context(
        &self,
        event_tx: &TurnObserver,
        chronological_projection: Arc<crate::ChronologicalProjection>,
    ) -> Result<crate::RuntimeExecutionContext<'run>, PluginError> {
        self.execution_context_observing(Arc::new(event_tx.clone()), chronological_projection)
    }

    /// [`Self::execution_context`] publishing through `observer` instead of
    /// the turn's stream — for shift paths whose emissions have no host lane.
    pub(super) fn execution_context_observing(
        &self,
        observer: Arc<dyn crate::engine::ObservationSink>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
    ) -> Result<crate::RuntimeExecutionContext<'run>, PluginError> {
        let manager = self.session_services.clone();
        let effect_controller = self.scoped_effect_controller.clone();
        let direct_completions =
            manager.direct_completion_client(effect_controller.clone(), Some(self.turn_id.clone()));
        let execution_env_spec = self
            .turn_pipeline
            .state()
            .process_execution_env_spec(&self.policy.policy);
        let run_capabilities = self
            .turn_pipeline
            .state()
            .authority
            .run_view()
            .map(|view| view.run.capabilities.clone())
            .unwrap_or_default();
        self.session
            .code_execution_context(
                &self.session_id,
                self.turn_pipeline
                    .state()
                    .current_frame_node_id
                    .clone()
                    .ok_or_else(|| {
                        PluginError::Session(
                            "runtime turn execution requires an initialized agent frame"
                                .to_string(),
                        )
                    })?,
                manager.state_service(),
                manager.lifecycle_service(),
                manager.graph_service(),
                manager.model_tool_process_service(),
                effect_controller,
                direct_completions,
                manager.process_engines().clone(),
                observer,
                chronological_projection,
                self.turn_context.clone(),
                execution_env_spec,
            )
            .map(|context| {
                let context = context
                    .with_tool_material_store(self.host.core.backend().tool_material_store())
                    .with_process_work(self.host.work.process_wiring().cloned());
                context
                    .with_logical_run(crate::TurnAddress::new(
                        self.session_id.clone(),
                        self.logical_run(),
                    ))
                    .with_run_capabilities(run_capabilities)
                    .with_recorded_turn_cancel(self.children_stop.clone())
                    .with_opener_state(self.opener_state.clone())
                    .with_turn_cancel_scope(self.turn_cancel_scope())
                    .with_turn_phase_probe(self.turn_phase_probe.clone())
            })
    }
}
