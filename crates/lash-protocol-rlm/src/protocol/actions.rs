use lash_core::session_model::{TurnFailureCode, TurnFailureKind, make_error_event};
use lash_core::{DriverAction, facade_support::TurnOutcome, facade_support::TurnStop};

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
    let cause = match code {
        TurnFailureCode::InvalidTurnOptions => {
            lash_protocol_types::TurnErrorCode::InvalidTurnOptions
        }
        _ => lash_protocol_types::TurnErrorCode::RuntimeRefused,
    };
    vec![
        DriverAction::Emit(make_error_event(
            category,
            Some(code.into()),
            error.clone(),
            Some(error),
            lash_protocol_types::session_model::RuntimeOutputCuts::standard(),
        )),
        DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError {
            cause: lash_protocol_types::TurnErrorCause::new(cause),
        })),
    ]
}
