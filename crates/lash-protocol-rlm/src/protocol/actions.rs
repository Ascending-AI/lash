use lash_core::session_model::make_error_event;
use lash_core::{DriverAction, facade_support::TurnOutcome, facade_support::TurnStop};

pub(super) fn invalid_driver_state_actions(error: String) -> Vec<DriverAction> {
    runtime_error_actions("rlm_driver_state", "invalid_driver_state", error)
}

pub(super) fn invalid_turn_options_actions(error: String) -> Vec<DriverAction> {
    runtime_error_actions("rlm_turn_options", "invalid_turn_options", error)
}

pub(super) fn runtime_error_actions(
    category: &'static str,
    code: &'static str,
    error: String,
) -> Vec<DriverAction> {
    vec![
        DriverAction::Emit(make_error_event(
            category,
            Some(code),
            error.clone(),
            Some(error),
        )),
        DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
    ]
}
