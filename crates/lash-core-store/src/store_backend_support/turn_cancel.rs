//! The wire forms of a turn cancel request's mode and undelivered-input
//! policy, and cancellation input facts, shared by both stores.
use crate::StoreError;

/// A `turn_cancel_requests` row: request id, origin, reason, disposition
/// and mode.
pub type TurnCancelRequestRow = (String, Option<String>, Option<String>, String, String);

/// Decode an open active-turn input selected by the terminal transaction.
/// Inputs addressed to another run are outside this cancellation.
pub fn affected_input(
    run: &crate::TurnId,
    disposition: crate::TurnCancelUndeliveredInputPolicy,
    input_id: String,
    input_json: &str,
    ingress_json: &str,
) -> Result<Option<crate::turn_control_vocabulary::TurnCancelAffectedInput>, StoreError> {
    let corrupt = |message: String| StoreError::StoredDataCorrupt {
        record_kind: "TurnCancelInput",
        message,
    };
    let ingress: crate::turn_input_vocabulary::TurnInputIngress =
        serde_json::from_str(ingress_json).map_err(|error| corrupt(error.to_string()))?;
    if ingress.active_turn_id() != Some(run) {
        return Ok(None);
    }
    Ok(Some(
        crate::turn_control_vocabulary::TurnCancelAffectedInput {
            input_id: crate::InputId::parse(input_id)
                .map_err(|error| corrupt(error.to_string()))?,
            payload: serde_json::from_str(input_json)
                .map_err(|error| corrupt(error.to_string()))?,
            disposition,
        },
    ))
}

pub fn turn_cancel_mode_wire(mode: crate::TurnCancelMode) -> &'static str {
    match mode {
        crate::TurnCancelMode::Immediate => "immediate",
        crate::TurnCancelMode::AfterStep => "after_step",
    }
}

pub fn turn_cancel_mode_from_wire(mode: &str) -> Result<crate::TurnCancelMode, StoreError> {
    match mode {
        "immediate" => Ok(crate::TurnCancelMode::Immediate),
        "after_step" => Ok(crate::TurnCancelMode::AfterStep),
        other => Err(StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: format!("unknown turn cancel mode `{other}`"),
        }),
    }
}

pub fn turn_cancel_undelivered_from_wire(
    disposition: &str,
) -> Result<crate::TurnCancelUndeliveredInputPolicy, StoreError> {
    match disposition {
        "defer" => Ok(crate::TurnCancelUndeliveredInputPolicy::Defer),
        "drop" => Ok(crate::TurnCancelUndeliveredInputPolicy::Drop),
        other => Err(StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: format!("unknown turn cancel disposition `{other}`"),
        }),
    }
}

pub fn turn_cancel_undelivered_wire(
    policy: crate::TurnCancelUndeliveredInputPolicy,
) -> &'static str {
    match policy {
        crate::TurnCancelUndeliveredInputPolicy::Defer => "defer",
        crate::TurnCancelUndeliveredInputPolicy::Drop => "drop",
    }
}
