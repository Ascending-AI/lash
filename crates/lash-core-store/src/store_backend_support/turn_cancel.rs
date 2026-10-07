//! The wire forms of a turn cancel request's mode and undelivered-input
//! policy, shared by both stores.
use crate::StoreError;

/// A `turn_cancel_requests` row: request id, origin, reason, disposition
/// and mode.
pub type TurnCancelRequestRow = (String, Option<String>, Option<String>, String, String);

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
