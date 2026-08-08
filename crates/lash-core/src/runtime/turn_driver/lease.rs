use super::*;
use crate::facade_support::ScopedEffectControllerFacadeOps;
use lash_sansio::sync::MutexExt;

impl<'run> RuntimeTurnDriver<'run> {
    pub(super) fn turn_effect_invocation(
        &self,
        machine: &TurnMachine,
        effect_id: crate::sansio::EffectId,
        effect_kind: RuntimeEffectKind,
    ) -> Result<RuntimeInvocation, RuntimeEffectControllerError> {
        Ok(crate::runtime::causal::turn_effect_invocation(
            &self.session_id,
            &self.turn_id,
            self.turn_index,
            machine.protocol_iteration(),
            effect_id,
            effect_kind,
        ))
    }

    pub(super) async fn execute_typed_turn_effect<T>(
        &mut self,
        machine: &mut TurnMachine,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
        envelope: RuntimeEffectEnvelope,
        decode: impl FnOnce(RuntimeEffectOutcome) -> Result<T, RuntimeEffectControllerError>,
    ) -> Result<T, RuntimeEffectControllerError> {
        let scoped_effect_controller = self.scoped_effect_controller.clone();
        let outcome = if let Some(task_controller) = scoped_effect_controller.to_static() {
            let (local_executor, update) = crate::RuntimeEffectLocalExecutor::turn(
                self,
                machine,
                event_tx.clone(),
                cancel.clone(),
                task_controller,
            );
            let outcome = scoped_effect_controller
                .controller()
                .execute_effect(envelope, local_executor)
                .await;
            self.apply_turn_effect_update(&update);
            outcome
        } else {
            let (task_controller, task_requests) =
                crate::runtime::effect::EffectTaskController::scoped(
                    scoped_effect_controller.controller(),
                    scoped_effect_controller.execution_scope().clone(),
                )
                .map_err(RuntimeEffectControllerError::from)?;
            let (local_executor, update) = crate::RuntimeEffectLocalExecutor::turn(
                self,
                machine,
                event_tx.clone(),
                cancel.clone(),
                task_controller,
            );
            let outcome = crate::runtime::effect::drive_effect_controller_task(
                scoped_effect_controller.controller(),
                envelope,
                local_executor,
                task_requests,
            )
            .await;
            self.apply_turn_effect_update(&update);
            outcome
        };
        let outcome = outcome?;
        decode(outcome)
    }

    fn apply_turn_effect_update(
        &mut self,
        update: &std::sync::Mutex<Option<crate::runtime::effect::TurnEffectStateUpdate>>,
    ) {
        let update = update.lock_recover().take();
        if let Some(update) = update {
            self.policy = update.policy;
            self.llm_stream_summaries = update.llm_stream_summaries;
            self.next_llm_ordinal = update.next_llm_ordinal;
            self.pending_queue_claims = update.pending_queue_claims;
            self.pending_turn_input_claims = update.pending_turn_input_claims;
            self.pending_checkpoint_turn_input_claim = update.pending_checkpoint_turn_input_claim;
        }
    }
}
