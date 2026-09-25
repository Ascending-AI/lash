use super::*;
use crate::PluginError;
use crate::facade_support::RuntimeSessionStateFacadeOps;

impl<'run> RuntimeTurnDriver<'run> {
    /// The scope whose cancellation gate this turn's waits race: always the
    /// physical turn's, whatever scope admitted the run, because that is the
    /// gate the turn's own control keys, peeks and forwards stops to
    /// (FIG-3672 P9). A process- or drain-admitted turn is cancelled through
    /// the same gate as a foreground one.
    pub(super) fn turn_cancel_scope(&self) -> crate::ExecutionScope {
        crate::ExecutionScope::turn(self.session_id.clone(), self.turn_id.clone())
    }

    pub(super) fn effect_controller_handle(&self) -> RuntimeEffectControllerHandle<'run> {
        RuntimeEffectControllerHandle::borrowed(self.scoped_effect_controller.clone())
    }

    pub(super) fn execution_context(
        &self,
        event_tx: mpsc::Sender<SessionStreamEvent>,
        stream_event_tx: &TurnObserver,
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
                self.register_live_opener(context.dispatch(), stream_event_tx);
                context
                    .with_recorded_turn_cancel(
                        self.turn_cancel.is_some(),
                        Arc::clone(&self.turn_control),
                        self.children_stop.clone(),
                    )
                    .with_opener_state(self.opener_state.clone())
                    .with_group_closing(self.host.core.control.effect_host.effect_group_closing())
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
    /// The lent context's `event_tx` is rebound before capture: the context's
    /// own sender is this phase's channel, whose forwarder the caller awaits
    /// once the phase's senders drop — a registration holding a clone would
    /// keep that channel open for the turn's whole life and hang the phase.
    /// Children instead emit on a channel the registration owns, forwarded to
    /// the turn's stream for exactly the entry's lifetime: the registry's
    /// `ended` token fires on supersede and on the release [`run`](super::machine)
    /// performs before it returns, so a `RunToCompletion` child that outlives
    /// its opener stops publishing into the turn's observer.
    ///
    /// Three ways this registers nothing, all of them conservative — the child
    /// stays accepted rather than running under a context that cannot serve it:
    /// the deployment routes no tool children; the scope is not one an opener
    /// is derived from (see
    /// [`opener_for_execution_scope`](crate::facade_support::opener_for_execution_scope));
    /// or the host hands out no owned controller for the captured context's
    /// controller slots — the lend a `'static` capture needs, since the
    /// opener's own live controller (a Restate handler's `ctx`-bound one)
    /// cannot outlive its frame.
    fn register_live_opener(
        &self,
        dispatch: &std::sync::Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        stream_event_tx: &TurnObserver,
    ) {
        let Some(tool_children) = self.host.core.control.tool_children.as_ref() else {
            return;
        };
        let Some(opener) = crate::facade_support::opener_for_execution_scope(
            self.scoped_effect_controller.admitted_scope(),
        ) else {
            return;
        };
        let Ok(Some(lent_controller)) = self
            .host
            .core
            .control
            .effect_host
            .scoped_static(self.scoped_effect_controller.admitted_scope().clone())
        else {
            return;
        };
        let (child_event_tx, mut child_event_rx) = mpsc::channel::<SessionStreamEvent>(64);
        let (child_activity_tx, mut child_activity_rx) = mpsc::channel::<crate::TurnActivity>(64);
        let context = crate::facade_support::LiveOpenerContext::capture_with_event_sender(
            dispatch.as_ref(),
            lent_controller,
            child_event_tx,
            self.children_stop.clone(),
        )
        .with_turn_activity_sender(child_activity_tx);
        let (registration, ended) = tool_children.openers().register(opener, context);
        let stream_event_tx = stream_event_tx.clone();
        crate::task::spawn(async move {
            loop {
                tokio::select! {
                    // The registration's own lifetime, checked first: when the
                    // entry goes — superseded or released at run end — this
                    // sender leaves the turn's stream whether or not a child is
                    // still holding the far end.
                    () = ended.cancelled() => break,
                    event = child_event_rx.recv() => {
                        let Some(event) = event else { break };
                        stream_event_tx.session(event);
                    }
                    activity = child_activity_rx.recv() => {
                        let Some(activity) = activity else { break };
                        stream_event_tx.publish(RuntimeStreamEvent::Turn(activity));
                    }
                }
            }
        });
        *self.live_opener.lock_recover() = Some(registration);
    }
}
