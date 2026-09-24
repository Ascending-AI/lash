use super::*;

impl RuntimeTurnDriver<'_> {
    /// Settle a controller error by its cause (FIG-3575), identically on every
    /// host: an outcome, including a journaled failure replaying, is recorded
    /// as a failed turn, and a live fault or a refusal that parks the turn
    /// (FIG-3586) aborts the invocation with `Err`.
    pub(super) fn fail_or_abort_runtime_effect_controller(
        machine: &mut TurnMachine,
        err: RuntimeEffectControllerError,
    ) -> Result<(), RuntimeError> {
        if err.turn_failure_cause().aborts_invocation() {
            return Err(err.into_runtime_error());
        }
        // A replay divergence keeps its structured evidence on the record.
        let raw = err
            .summary
            .as_ref()
            .and_then(|summary| serde_json::to_string(summary).ok());
        machine.fail_turn(make_error_event(
            crate::TurnFailureKind::RuntimeEffectController,
            Some(crate::FailureCode::from(&err.code)),
            err.message,
            raw,
        ));
        Ok(())
    }
}
