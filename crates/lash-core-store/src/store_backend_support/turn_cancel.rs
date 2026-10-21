//! The wire forms of a turn cancel request's mode and undelivered-input
//! policy, and cancellation input facts, shared by both stores.
use crate::StoreError;

/// A `turn_cancel_requests` row: request id, origin, reason, disposition
/// and mode.
pub type TurnCancelRequestRow = (String, Option<String>, Option<String>, String, String);

/// Open inputs addressed to the physical turns or admission members an ended
/// run owns (FIG-3946). No open row stays bound to a terminal run: adapters
/// first release its retained input and queued-work bindings so a later run
/// can admit them. Only its addressed input takes the cancel request's policy.
/// Drop writes a cancellation tombstone at the terminal instant (FIG-4098);
/// Defer writes nothing. The cancellation outcome retains the input facts.
///
/// Decode every candidate before consulting the cancel request, so an
/// unrelated corrupt input still refuses the terminal transaction.
pub struct EndedRunInputRelease<I>(Vec<I>);

impl<I> EndedRunInputRelease<I> {
    pub fn select(
        ended: &crate::store::RunTurns,
        inputs: impl IntoIterator<
            Item = Result<(I, Option<lash_vocabulary::identity::TurnId>), StoreError>,
        >,
    ) -> Result<Self, StoreError> {
        let mut addressed = Vec::new();
        for input in inputs {
            let (input_id, turn) = input?;
            if turn.as_ref().is_some_and(|turn| ended.contains(turn)) {
                addressed.push(input_id);
            }
        }
        Ok(Self(addressed))
    }

    /// An empty selection needs no cancel-request read.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Only an explicit Drop withdraws addressed input. Default and explicit
    /// Defer write nothing: immutable delivery becomes next-turn input by rule
    /// at its existing ingress position (ADR 0101 §5.1).
    pub fn dropped_inputs(
        self,
        policy: Option<crate::TurnCancelUndeliveredInputPolicy>,
    ) -> impl Iterator<Item = I> {
        self.0
            .into_iter()
            .filter(move |_| policy == Some(crate::TurnCancelUndeliveredInputPolicy::Drop))
    }
}

/// Decode an open active-turn input selected by the terminal transaction.
/// Inputs addressed to another run are outside this cancellation.
pub fn affected_input(
    run: &lash_vocabulary::identity::TurnId,
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
            input_id: lash_vocabulary::identity::InputId::parse(input_id)
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
