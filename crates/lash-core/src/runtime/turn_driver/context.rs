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
                self.register_live_opener(context.dispatch());
                context
                    .with_turn_cancel_scope(self.turn_cancel_scope())
                    .with_engine_child_max_attempts(
                        self.host.core.control.engine_child_max_attempts,
                    )
                    .with_turn_phase_probe(self.turn_phase_probe.clone())
            })
    }

    /// Publishes this turn as a live opener, lending its tool-execution context
    /// to the group children it opens (ADR 0099 §2, §3).
    ///
    /// Called from the one place that builds a turn's dispatch context, and
    /// re-registered on every later one: a turn builds a fresh context per
    /// phase, and a child must borrow the live half of the *current* one.
    /// Re-registration supersedes rather than duplicates, and the registry's
    /// generation guard keeps the superseded guard from evicting its
    /// replacement.
    ///
    /// Three ways this registers nothing, all of them conservative — the child
    /// stays accepted rather than running under a context that cannot serve it:
    /// the deployment routes no tool children; the scope is not one an opener
    /// is derived from (see
    /// [`opener_for_execution_scope`](crate::facade_support::opener_for_execution_scope));
    /// or the context cannot be taken to `'static`, which is the same condition
    /// that would stop a borrowed child outliving the caller that opened it.
    fn register_live_opener(
        &self,
        dispatch: &std::sync::Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    ) {
        let Some(tool_children) = self.host.core.control.tool_children.as_ref() else {
            return;
        };
        let Some(opener) = crate::facade_support::opener_for_execution_scope(
            self.scoped_effect_controller.execution_scope(),
        ) else {
            return;
        };
        let Some(context) = crate::facade_support::LiveOpenerContext::capture(dispatch.as_ref())
        else {
            return;
        };
        *self.live_opener.lock_recover() = Some(tool_children.openers().register(opener, context));
    }
}
