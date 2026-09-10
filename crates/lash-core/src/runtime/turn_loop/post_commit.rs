//! The post-commit phase: everything a committed turn still owes its host.
//!
//! Nothing here may fail the turn. A failure is recorded on the assembled turn
//! as a blocking issue and invalidates resident state, because the durable
//! commit already happened and cannot be taken back.

use super::*;
use crate::TurnId;

pub(super) struct PostCommitDelivery {
    pub(super) turn: AssembledTurn,
    pub(super) events: Vec<SessionStreamEvent>,
    pub(super) enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    pub(super) post_commit_delivery_failed: bool,
}

impl TypedTurnPhase for PostCommitDelivery {
    const RUNTIME_PHASE: RuntimeTurnPhase = RuntimeTurnPhase::PostCommitDelivery;
}

impl LashRuntime {
    pub(super) async fn emit_turn_persisted_event(
        &self,
        returned_turn: &AssembledTurn,
        scoped_effect_controller: &ScopedEffectController<'_>,
        trace_turn_id: &TurnId,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<Option<crate::PluginError>, RuntimeError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(None);
        };
        let manager = self
            .runtime_session_services_after_commit(session_execution_lease)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let phase_turn_id = turn_phase_id(trace_turn_id, "turn-persisted");
        let phase_controller = scoped_child_turn_controller(
            scoped_effect_controller,
            &self.state.session_id,
            &phase_turn_id,
        )?;
        let direct_completions = manager.direct_completion_client(
            RuntimeEffectControllerHandle::borrowed(phase_controller),
            Some(phase_turn_id),
        );

        let result = session
            .plugins()
            .emit_runtime_event_with_phase_probe(
                crate::PluginLifecycleEvent::TurnPersisted(Box::new(
                    crate::SessionStateChangedContext {
                        session_id: self.state.session_id.clone(),
                        state: crate::SessionReadView::from_snapshot(&returned_turn.state),
                        sessions: manager.state_service(),
                        session_graph: manager.graph_service(),
                        direct_completions,
                    },
                )),
                self.turn_phase_probe.clone(),
            )
            .await;
        Ok(result.err())
    }
}
