//! Typed decoding for relational turn cancellation, shared by both stores.
use crate::{SessionId, StoreError, TurnId};

pub fn turn_cancel_snapshot_from_row(
    session_id: &SessionId,
    turn_id: &TurnId,
    row: Option<TurnCancelIntentRow>,
) -> Result<crate::TurnCancelIntentSnapshot, StoreError> {
    let Some((request_id, origin, reason, disposition, mode, revision)) = row else {
        return Ok(crate::TurnCancelIntentSnapshot::Absent);
    };
    let revision = u64::try_from(revision).map_err(|_| StoreError::StoredDataCorrupt {
        record_kind: "TurnCancelRequest",
        message: "intent revision is negative".to_string(),
    })?;
    if revision == 0 {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: "intent revision is zero".to_string(),
        });
    }
    Ok(crate::TurnCancelIntentSnapshot::Present {
        request: crate::TurnCancelRequest {
            address: crate::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
            undelivered: turn_cancel_undelivered_from_wire(&disposition)?,
            mode: turn_cancel_mode_from_wire(&mode)?,
        },
        revision,
    })
}

/// One `turn_cancel_requests` intent: request id, origin, reason,
/// disposition, mode and revision.
pub type TurnCancelIntentRow = (String, Option<String>, Option<String>, String, String, i64);

pub type TurnCancelRequestRow = (String, Option<String>, Option<String>, String, String);

/// One `turn_cancel_affected_inputs` row: ingress row id, payload, disposition,
/// item kind and — for a held wake — its batch.
pub type TurnCancelAffectedRow = (String, String, String, String, Option<String>);

pub const AFFECTED_INPUT_KIND: &str = "input";
pub const AFFECTED_WAKE_KIND: &str = "process_wake";

pub fn turn_cancel_record_from_rows(
    session_id: &SessionId,
    turn_id: &TurnId,
    row: TurnCancelRequestRow,
    affected_rows: Vec<TurnCancelAffectedRow>,
) -> Result<crate::TurnCancelRequestRecord, StoreError> {
    let (request_id, origin, reason, disposition, mode) = row;
    let mut outcome = crate::TurnCancelInputOutcome::default();
    for (item_id, payload_json, applied_disposition, item_kind, batch_id) in affected_rows {
        let applied_disposition = turn_cancel_undelivered_from_wire(&applied_disposition)?;
        match (item_kind.as_str(), batch_id) {
            (AFFECTED_INPUT_KIND, None) => {
                outcome.affected_inputs.push(
                    crate::turn_control_vocabulary::TurnCancelAffectedInput {
                        input_id: item_id.into(),
                        payload: decode(&payload_json, "turn input")?,
                        disposition: applied_disposition,
                    },
                );
            }
            (AFFECTED_WAKE_KIND, Some(batch_id)) => {
                outcome.affected_wakes.push(
                    crate::turn_control_vocabulary::TurnCancelAffectedWake {
                        batch_id: batch_id.into(),
                        wake: decode(&payload_json, "process wake")?,
                        disposition: applied_disposition,
                    },
                );
            }
            (other, _) => {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "TurnCancelRequest",
                    message: format!("malformed turn cancel affected item of kind `{other}`"),
                });
            }
        }
    }
    Ok(crate::TurnCancelRequestRecord {
        request: crate::TurnCancelRequest {
            address: crate::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
            undelivered: turn_cancel_undelivered_from_wire(&disposition)?,
            mode: turn_cancel_mode_from_wire(&mode)?,
        },
        outcome: (!outcome.is_empty()).then_some(outcome),
    })
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

fn decode<T: serde::de::DeserializeOwned>(
    json: &str,
    record_kind: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|error| StoreError::StoredDataCorrupt {
        record_kind,
        message: error.to_string(),
    })
}
