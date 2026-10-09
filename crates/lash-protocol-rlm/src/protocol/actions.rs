use lash_core::session_model::{TurnFailureCode, TurnFailureKind, make_error_event};
use lash_core::{DriverAction, facade_support::TurnOutcome, facade_support::TurnStop};

/// Refuse the parked driver state the driver was handed back: `error` is why
/// this build does not decode it. The turn neither finishes nor commits; its
/// rows keep the state for a build that reads it.
pub(crate) fn refuse_driver_state(error: String) -> Vec<DriverAction> {
    vec![DriverAction::RefuseState(
        lash_sansio::UndecodableDriverState {
            driver: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            reason: error,
        },
    )]
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
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        )),
        DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
    ]
}
