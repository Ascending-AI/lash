use lash_core::session_model::{TurnFailureCode, TurnFailureKind, make_error_event};
use lash_core::{DriverAction, facade_support::TurnOutcome, facade_support::TurnStop};

pub(crate) fn invalid_driver_state_actions(error: String) -> Vec<DriverAction> {
    runtime_error_actions(
        TurnFailureKind::RlmDriverState,
        TurnFailureCode::InvalidDriverState,
        error,
    )
}

pub(crate) fn invalid_turn_options_actions(error: String) -> Vec<DriverAction> {
    runtime_error_actions(
        TurnFailureKind::RlmTurnOptions,
        TurnFailureCode::InvalidTurnOptions,
        error,
    )
}

pub(crate) fn runtime_error_actions(
    category: TurnFailureKind,
    code: TurnFailureCode,
    error: String,
) -> Vec<DriverAction> {
    vec![
        DriverAction::Emit(make_error_event(
            category,
            Some(code.into()),
            error.clone(),
            Some(error),
        )),
        DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
    ]
}
