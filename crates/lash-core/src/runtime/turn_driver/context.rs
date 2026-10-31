use super::*;
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
use lash_execution_contracts::plugin_error::PluginError;

impl<'run> RuntimeTurnDriver<'run> {
    /// The logical Run this physical turn belongs to: the admitted scope's
    /// root, or this turn when it is its own root.
    pub(super) fn logical_run(&self) -> lash_vocabulary::identity::TurnId {
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
        TurnExecutionContext {
            session: &self.session,
            session_services: &self.session_services,
            host: &self.host,
            scoped_effect_controller: &self.scoped_effect_controller,
            session_id: &self.session_id,
            turn_id: &self.turn_id,
            frame: self.turn_pipeline.state().current_frame_node_id.as_ref(),
            run_config: &self.run_config,
            turn_context: &self.turn_context,
            children_stop: &self.children_stop,
            opener_state: &self.opener_state,
            turn_phase_probe: &self.turn_phase_probe,
        }
        .build(observer, chronological_projection)
    }
}

/// Inputs shared by turn presentation and an isolated code-effect body.
struct TurnExecutionContext<'a> {
    session: &'a Session,
    session_services: &'a Arc<RuntimeSessionServices>,
    host: &'a RuntimeHost,
    scoped_effect_controller: &'a ActorContext,
    session_id: &'a SessionId,
    turn_id: &'a lash_vocabulary::identity::TurnId,
    frame: Option<&'a crate::FrameNodeId>,
    run_config: &'a crate::runtime::durable::run_config::RecordedRunConfig,
    turn_context: &'a crate::TurnContext,
    children_stop: &'a CancellationToken,
    opener_state: &'a crate::session::OpenerState,
    turn_phase_probe: &'a Option<Arc<dyn RuntimeTurnPhaseProbe>>,
}

impl TurnExecutionContext<'_> {
    fn build<'run>(
        self,
        observer: Arc<dyn crate::engine::ObservationSink>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
    ) -> Result<crate::RuntimeExecutionContext<'run>, PluginError> {
        let manager = Arc::clone(self.session_services);
        let effect_controller = self.scoped_effect_controller.clone();
        let direct_completions =
            manager.direct_completion_client(effect_controller.clone(), Some(self.turn_id.clone()));
        // The turn runs under its logical Run's recorded configuration and
        // render (FIG-6021), never the session's live config.
        let run = self.run_config.shared_run();
        let facts = crate::plugin::ExecutionFacts::of_run(&run);
        self.session
            .code_execution_context(
                self.session_id,
                self.frame.cloned().ok_or_else(|| {
                    PluginError::Session(
                        "runtime turn execution requires an initialized agent frame".to_string(),
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
                facts,
            )
            .map(|context| {
                let context = context
                    .with_tool_material_store(self.host.core.backend().tool_material_store())
                    .with_tool_fault_retry(self.host.core.control.pacing.tool_fault_retry)
                    .with_process_work(self.host.work.process_wiring().cloned());
                context
                    .with_logical_run(crate::TurnAddress::new(
                        self.session_id.clone(),
                        self.scoped_effect_controller
                            .execution_scope()
                            .logical_run()
                            .unwrap_or_else(|| self.turn_id.clone()),
                    ))
                    .with_resolved_run(run)
                    .with_recorded_turn_cancel(self.children_stop.clone())
                    .with_opener_state(self.opener_state.clone())
                    .with_turn_cancel_scope(crate::ExecutionScope::turn(
                        self.session_id.clone(),
                        self.turn_id.clone(),
                    ))
                    .with_turn_phase_probe(self.turn_phase_probe.clone())
            })
    }
}

impl TurnEffectContext {
    pub(super) fn execution_context(
        &self,
        event_tx: &TurnObserver,
        chronological_projection: Arc<crate::ChronologicalProjection>,
    ) -> Result<crate::RuntimeExecutionContext<'static>, PluginError> {
        TurnExecutionContext {
            session: &self.session,
            session_services: &self.session_services,
            host: &self.host,
            scoped_effect_controller: &self.scoped_effect_controller,
            session_id: &self.session_id,
            turn_id: &self.turn_id,
            frame: self.execution_frame.as_ref(),
            run_config: &self.run_config,
            turn_context: &self.turn_context,
            children_stop: &self.children_stop,
            opener_state: &self.opener_state,
            turn_phase_probe: &self.turn_phase_probe,
        }
        .build(Arc::new(event_tx.clone()), chronological_projection)
    }
}
