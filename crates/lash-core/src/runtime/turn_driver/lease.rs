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
        envelope: RuntimeEffectEnvelope,
        decode: impl FnOnce(RuntimeEffectOutcome) -> Result<T, RuntimeEffectControllerError>,
    ) -> Result<T, RuntimeEffectControllerError> {
        // The step body runs on a copy of this driver and hands nothing back
        // but its outcome: every decision it made rides the recorded outcome,
        // so a replay that never ran the body reconstructs the same state.
        let scoped_effect_controller = self.scoped_effect_controller.clone();
        let scoped_effect_controller = match &envelope.command {
            RuntimeEffectCommand::AssistantResponseHooks { plan, .. } => {
                match self
                    .session
                    .plugins()
                    .validate_assistant_response_plan(plan)
                {
                    Ok(()) => scoped_effect_controller,
                    Err(error) => scoped_effect_controller.with_journal_guard(Arc::new(
                        crate::CommandJournalGuard::open()
                            .served_only(crate::ServedOnlyRange::every_key(error.into())),
                    )),
                }
            }
            _ => scoped_effect_controller,
        };
        let outcome = if let Some(task_controller) = scoped_effect_controller.to_static() {
            let local_executor = super::local_effects::turn_effect_executor(
                self,
                machine,
                event_tx.clone(),
                task_controller,
                envelope.invocation.effect_replay_key(),
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
            // The proxy issues this same shift's steps: one frontier.
            let task_controller = task_controller.in_drive_of(&scoped_effect_controller);
            let local_executor = super::local_effects::turn_effect_executor(
                self,
                machine,
                event_tx.clone(),
                task_controller,
                envelope.invocation.effect_replay_key(),
            );
            let local_executor =
                scoped_effect_controller.guard_local_executor(&envelope, local_executor)?;
            crate::runtime::effect::drive_effect_controller_task(
                scoped_effect_controller.controller(),
                scoped_effect_controller.execution_scope().clone(),
                envelope,
                local_executor,
                task_requests,
            )
            .await
        };
        let outcome = self.session.plugins().restore_effect_state(outcome?)?;
        decode(outcome)
    }
}
