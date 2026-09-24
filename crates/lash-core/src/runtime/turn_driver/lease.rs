use super::*;

impl<'run> RuntimeTurnDriver<'run> {
    pub(super) fn turn_effect_invocation(
        &self,
        machine: &TurnMachine,
        effect_id: crate::sansio::EffectId,
        effect_kind: RuntimeEffectKind,
    ) -> Result<RuntimeEffectInvocation, RuntimeEffectControllerError> {
        Ok(crate::runtime::causal::turn_effect_invocation(
            self.scoped_effect_controller.execution_scope(),
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
        event_tx: &TurnObserver,
        cancel: &CancellationToken,
        envelope: RuntimeEffectEnvelope,
        decode: impl FnOnce(RuntimeEffectOutcome) -> Result<T, RuntimeEffectControllerError>,
    ) -> Result<T, RuntimeEffectControllerError> {
        // The step body runs on a copy of this driver and hands nothing back
        // but its outcome: every decision it made rides the recorded outcome,
        // so a replay that never ran the body reconstructs the same state.
        let scoped_effect_controller = self.scoped_effect_controller.clone();
        let outcome = if let Some(task_controller) = scoped_effect_controller.to_static() {
            let local_executor = super::local_effects::turn_effect_executor(
                self,
                machine,
                event_tx.clone(),
                cancel.clone(),
                task_controller,
            );
            scoped_effect_controller
                .execute_effect(envelope, local_executor)
                .await
        } else {
            let (task_controller, task_requests) =
                crate::runtime::effect::EffectTaskController::scoped(
                    scoped_effect_controller.controller(),
                    scoped_effect_controller.admitted_scope().clone(),
                )
                .map_err(RuntimeEffectControllerError::from)?;
            let local_executor = super::local_effects::turn_effect_executor(
                self,
                machine,
                event_tx.clone(),
                cancel.clone(),
                task_controller,
            );
            crate::runtime::effect::drive_effect_controller_task(
                scoped_effect_controller.controller(),
                scoped_effect_controller.execution_scope().clone(),
                envelope,
                local_executor,
                task_requests,
            )
            .await
        };
        let outcome = outcome?;
        decode(outcome)
    }
}
