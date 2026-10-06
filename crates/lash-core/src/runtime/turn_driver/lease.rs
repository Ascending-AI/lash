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
        let local_executor = super::local_effects::turn_effect_executor(
            self,
            machine,
            event_tx.clone(),
            scoped_effect_controller.clone(),
            envelope.invocation.effect_replay_key(),
        );
        let outcome =
            super::issue::issue_effect(&scoped_effect_controller, envelope, local_executor).await;
        decode(outcome?)
    }
}
