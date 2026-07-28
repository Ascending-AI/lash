use super::*;

impl RuntimeTurnDriver<'_> {
    fn fail_runtime_effect_controller(
        machine: &mut TurnMachine,
        err: RuntimeEffectControllerError,
    ) {
        machine.fail_turn(make_error_event(
            "runtime_effect_controller",
            Some(&err.code),
            err.message,
            None,
        ));
    }

    pub(super) fn should_abort_for_runtime_effect_error(&self) -> bool {
        self.scoped_effect_controller
            .controller()
            .replay_ownership()
            == crate::EffectReplayOwnership::Controller
    }

    pub(super) fn fail_or_abort_runtime_effect_controller(
        &self,
        machine: &mut TurnMachine,
        err: RuntimeEffectControllerError,
    ) -> Result<(), RuntimeError> {
        if self.should_abort_for_runtime_effect_error() {
            Err(err.into_runtime_error())
        } else {
            Self::fail_runtime_effect_controller(machine, err);
            Ok(())
        }
    }
}
