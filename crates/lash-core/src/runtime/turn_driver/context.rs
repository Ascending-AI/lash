use super::*;
use crate::PluginError;
use crate::facade_support::RuntimeSessionStateFacadeOps;

impl<'run> RuntimeTurnDriver<'run> {
    pub(super) fn turn_cancel_scope(&self) -> crate::ExecutionScope {
        match self.scoped_effect_controller.execution_scope() {
            crate::ExecutionScope::Turn { .. } => {
                crate::ExecutionScope::turn(self.session_id.clone(), self.turn_id.clone())
            }
            admitted_scope => admitted_scope.clone(),
        }
    }

    pub(super) fn turn_cancel_wait(
        &self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> crate::runtime::TurnCancelWait {
        crate::runtime::TurnCancelWait::observing(cancellation, self.turn_cancel_scope())
    }

    pub(super) fn effect_controller_handle(&self) -> RuntimeEffectControllerHandle<'run> {
        RuntimeEffectControllerHandle::borrowed(self.scoped_effect_controller.clone())
    }

    pub(super) fn execution_context(
        &self,
        event_tx: mpsc::Sender<SessionStreamEvent>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
    ) -> Result<crate::RuntimeExecutionContext<'run>, PluginError> {
        let manager = self.session_services.clone();
        let effect_controller = self.effect_controller_handle();
        let direct_completions = manager
            .direct_completion_client(effect_controller.clone_scoped(), Some(self.turn_id.clone()));
        let execution_env_spec = self
            .turn_pipeline
            .state()
            .process_execution_env_spec(&self.policy.policy);
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
                manager.trigger_router(),
                manager.process_definition_registry(),
                manager.process_engines().clone(),
                event_tx,
                chronological_projection,
                self.protocol_extension.clone(),
                self.turn_context.clone(),
                execution_env_spec,
                self.checkpoint_messages.clone(),
                Arc::clone(&self.host.core.attachment_source_policy),
            )
            .map(|context| {
                context
                    .with_turn_cancel_scope(self.turn_cancel_scope())
                    .with_engine_child_max_attempts(
                        self.host.core.control.engine_child_max_attempts,
                    )
                    .with_turn_phase_probe(self.turn_phase_probe.clone())
            })
    }
}
