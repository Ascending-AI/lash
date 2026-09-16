use super::*;

impl RuntimeTurnDriver<'_> {
    fn fail_runtime_effect_controller(
        machine: &mut TurnMachine,
        err: RuntimeEffectControllerError,
    ) {
        machine.fail_turn(make_error_event(
            crate::TurnFailureKind::RuntimeEffectController,
            Some(crate::TurnFailureCode::Other(err.code.as_str().to_string())),
            err.message,
            None,
        ));
    }

    pub(super) async fn should_abort_for_runtime_effect_error(
        &self,
        code: RuntimeErrorCode,
    ) -> Result<bool, RuntimeError> {
        match self
            .scoped_effect_controller
            .controller()
            .runtime_effect_failure_disposition(code)
            .await?
        {
            crate::RuntimeEffectFailureDisposition::AbortInvocation => Ok(true),
            crate::RuntimeEffectFailureDisposition::RecordTurnFailure => Ok(false),
        }
    }

    pub(super) async fn fail_or_abort_runtime_effect_controller(
        &self,
        machine: &mut TurnMachine,
        err: RuntimeEffectControllerError,
    ) -> Result<(), RuntimeError> {
        if self
            .should_abort_for_runtime_effect_error(err.code.clone())
            .await?
        {
            Err(err.into_runtime_error())
        } else {
            Self::fail_runtime_effect_controller(machine, err);
            Ok(())
        }
    }
}
