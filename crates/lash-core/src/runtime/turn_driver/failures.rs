use super::*;

impl RuntimeTurnDriver<'_> {
    fn fail_runtime_effect_controller(
        machine: &mut TurnMachine,
        err: RuntimeEffectControllerError,
    ) {
        machine.fail_turn(make_error_event(
            crate::TurnFailureKind::RuntimeEffectController,
            Some(crate::FailureCode::from(&err.code)),
            err.message,
            None,
        ));
    }

    pub(super) async fn fail_or_abort_runtime_effect_controller(
        &self,
        machine: &mut TurnMachine,
        err: RuntimeEffectControllerError,
    ) -> Result<(), RuntimeError> {
        if self
            .scoped_effect_controller
            .controller()
            .effect_journaling()
            == crate::EffectJournaling::Journaled
        {
            Err(err.into_runtime_error())
        } else {
            Self::fail_runtime_effect_controller(machine, err);
            Ok(())
        }
    }
}
