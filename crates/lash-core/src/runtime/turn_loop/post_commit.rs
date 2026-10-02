//! The post-commit phase: everything a committed turn still owes its host.
//!
//! Observer failures are advisory: the committed turn and resident state remain valid.
//! Required host deliveries keep their own completion and failure handling.

use super::*;

pub(super) struct PostCommitDelivery {
    pub(super) turn: AssembledTurn,
    pub(super) events: Vec<SessionStreamEvent>,
    pub(super) post_commit_delivery_failed: bool,
}

impl TypedTurnPhase for PostCommitDelivery {
    const RUNTIME_PHASE: RuntimeTurnPhase = RuntimeTurnPhase::PostCommitDelivery;
}

impl LashRuntime {
    pub(super) async fn emit_turn_persisted_event(
        &self,
        returned_turn: &AssembledTurn,
        shift_fence: Option<&ShiftFence>,
    ) -> Result<Option<crate::PluginError>, RuntimeError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(None);
        };
        let manager = self
            .runtime_session_services_after_commit(shift_fence)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let result = session
            .plugins()
            .dispatch(self.turn_phase_probe.as_ref())
            .emit_runtime_event(crate::PluginLifecycleEvent::TurnPersisted(Box::new(
                crate::SessionStateChangedContext {
                    session_id: self.state.session_id.clone(),
                    plugin_config: session.plugins().admitted_plugin_config(),
                    state: crate::SessionReadView::from_snapshot(&returned_turn.state),
                    sessions: manager.read_service(),
                },
            )))
            .await;
        Ok(result.err())
    }
}
