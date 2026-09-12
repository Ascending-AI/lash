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

    pub(in crate::runtime) async fn finish_parent_end_actions(
        &self,
    ) -> Result<Vec<SessionStreamEvent>, crate::RuntimeError> {
        let _phase = crate::runtime::RuntimeNamedPhase::begin(
            self.turn_phase_probe.clone(),
            "tool_intent.parent_end",
        );
        let capacity = self.recorded_intent_outcomes.snapshot().len().max(1);
        let (event_tx, mut event_rx) = mpsc::channel(capacity);
        let context = self
            .execution_context(
                event_tx,
                Arc::new(crate::ChronologicalProjection::default()),
            )
            .map_err(|error| {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::PluginSessionManager,
                    error.to_string(),
                )
            })?;
        context.finish_parent_end_actions().await.map_err(|error| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::PluginSessionManager,
                error.to_string(),
            )
        })?;
        drop(context);
        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }
        Ok(events)
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
                event_tx,
                chronological_projection,
                self.protocol_extension.clone(),
                self.turn_context.clone(),
                execution_env_spec,
                self.checkpoint_messages.clone(),
                self.recorded_intent_outcomes.clone(),
                Arc::clone(&self.host.core.attachment_source_policy),
            )
            .map(|context| {
                context
                    .with_turn_cancel_scope(self.turn_cancel_scope())
                    .with_turn_phase_probe(self.turn_phase_probe.clone())
            })
    }
}
