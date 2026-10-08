//! Durable turn-cancellation vocabulary.
//!
//! A turn's address, a host's cancel request, and what a cancel did with the
//! input the turn did not deliver.

use crate::{
    ExecutionScope, RuntimeError, SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy,
    TurnCancellationEvidence, TurnId,
};
use serde::{Deserialize, Serialize};

/// Stable routing identity for one foreground turn.
///
/// These identifiers select work; they are not authorization credentials.
/// Hosts exposing turn control to untrusted callers must authenticate and
/// authorize the request before calling Lash.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnAddress {
    pub session_id: SessionId,
    pub turn_id: TurnId,
}
impl TurnAddress {
    pub fn new(session_id: impl Into<SessionId>, turn_id: impl Into<TurnId>) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    pub fn execution_scope(&self) -> ExecutionScope {
        ExecutionScope::turn(&self.session_id, &self.turn_id)
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        Ok(self.execution_scope().validate()?)
    }
}
/// One undelivered active-turn input affected by cancellation repair.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TurnCancelAffectedInput {
    pub input_id: crate::InputId,
    pub payload: crate::TurnInput,
    pub disposition: TurnCancelUndeliveredInputPolicy,
}
impl PartialEq for TurnCancelAffectedInput {
    fn eq(&self, other: &Self) -> bool {
        self.input_id == other.input_id
            && self.disposition == other.disposition
            && serde_json::to_value(&self.payload).ok() == serde_json::to_value(&other.payload).ok()
    }
}
impl Eq for TurnCancelAffectedInput {}
/// Durable outcome accumulated on a turn-cancel request: the affected-item
/// record of ADR 0101 §10. Every host input the cancel left undelivered is
/// listed, in the order the cancel settled it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TurnCancelInputOutcome {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_inputs: Vec<TurnCancelAffectedInput>,
}
impl TurnCancelInputOutcome {
    /// Reports whether the cancellation affected no active-turn input, so
    /// hosts can skip restore, re-enqueue, or
    /// audit work without inspecting the lists.
    pub fn is_empty(&self) -> bool {
        self.affected_inputs.is_empty()
    }

    /// How many inputs the cancellation affected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.affected_inputs.len()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelRequest {
    pub address: TurnAddress,
    pub request_id: String,
    /// Opaque host-domain data. Lash never interprets this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Policy for active-turn input the cancelled turn did not deliver.
    pub undelivered: TurnCancelUndeliveredInputPolicy,
    /// When the request is honoured. `Immediate` fires the cooperative token
    /// and backtracks to the last checkpoint; `AfterStep` waits for the step
    /// boundary that closes the current protocol iteration. Records written
    /// before this field existed decode as `Immediate`.
    #[serde(default, skip_serializing_if = "TurnCancelMode::is_immediate")]
    pub mode: TurnCancelMode,
}
impl TurnCancelRequest {
    pub fn new(
        address: TurnAddress,
        request_id: impl Into<String>,
        origin: Option<String>,
    ) -> Self {
        Self {
            address,
            request_id: request_id.into(),
            origin,
            reason: None,
            undelivered: TurnCancelUndeliveredInputPolicy::Defer,
            mode: TurnCancelMode::Immediate,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn undelivered(mut self, policy: TurnCancelUndeliveredInputPolicy) -> Self {
        self.undelivered = policy;
        self
    }

    pub fn mode(mut self, mode: TurnCancelMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.address.validate()?;
        if self.request_id.trim().is_empty() {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "turn cancellation requires a non-empty request id",
            ));
        }
        if self
            .request_id
            .starts_with(TurnCancellationEvidence::INTERNAL_REQUEST_ID_PREFIX)
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                format!(
                    "a host turn cancellation request id cannot start with `{}`: lash \
                     reserves that namespace for cancellations it originates itself",
                    TurnCancellationEvidence::INTERNAL_REQUEST_ID_PREFIX
                ),
            ));
        }
        Ok(())
    }

    pub fn evidence(&self) -> TurnCancellationEvidence {
        TurnCancellationEvidence {
            request_id: self.request_id.clone(),
            origin: self.origin.clone(),
            reason: self.reason.clone(),
            undelivered: self.undelivered,
            mode: self.mode,
            honoured_after_step: None,
        }
    }
}
