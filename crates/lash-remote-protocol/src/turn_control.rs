//! Transport-neutral foreground-turn cancellation envelopes.

use lash_sansio::SessionId;
use lash_sansio::TurnId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::registry_errors::{RemoteProtocolError, require_non_empty};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnCancelUndeliveredInputPolicy {
    #[default]
    Defer,
    Drop,
}

/// The boundary at which a turn honours the cancellation request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnCancelMode {
    #[default]
    Immediate,
    AfterStep,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnCancellationEvidence {
    pub request_id: String,
    /// Opaque host-domain data; Lash records and returns it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "remote_undelivered_is_defer")]
    pub undelivered: RemoteTurnCancelUndeliveredInputPolicy,
    #[serde(default, skip_serializing_if = "remote_mode_is_immediate")]
    pub mode: RemoteTurnCancelMode,
    /// The committed protocol iteration that honoured an after-step stop.
    /// Absent for immediate stops and requests honoured before any step ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub honoured_after_step: Option<usize>,
}

impl RemoteTurnCancellationEvidence {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty(
            "RemoteTurnCancellationEvidence",
            "request_id",
            &self.request_id,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnCancelRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub request_id: String,
    /// Opaque host-domain data; Lash never interprets this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "remote_undelivered_is_defer")]
    pub undelivered: RemoteTurnCancelUndeliveredInputPolicy,
    /// Immediate cancellation interrupts the current step; after-step
    /// cancellation waits for that iteration's checkpoint to commit.
    #[serde(default, skip_serializing_if = "remote_mode_is_immediate")]
    pub mode: RemoteTurnCancelMode,
}

fn remote_mode_is_immediate(value: &RemoteTurnCancelMode) -> bool {
    matches!(value, RemoteTurnCancelMode::Immediate)
}

fn remote_undelivered_is_defer(value: &RemoteTurnCancelUndeliveredInputPolicy) -> bool {
    matches!(value, RemoteTurnCancelUndeliveredInputPolicy::Defer)
}

impl RemoteTurnCancelRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteTurnCancelRequest", "request_id", &self.request_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RemoteTurnCancelOutcome {
    Requested {
        cancellation: RemoteTurnCancellationEvidence,
    },
    AlreadyRequested {
        cancellation: RemoteTurnCancellationEvidence,
    },
    Escalated {
        cancellation: RemoteTurnCancellationEvidence,
    },
    PolicyConflict {
        requested: RemoteTurnCancelUndeliveredInputPolicy,
        accepted: RemoteTurnCancellationEvidence,
    },
    CompletionWonRace,
    UnknownOrRevoked,
}

impl RemoteTurnCancelOutcome {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Requested { cancellation }
            | Self::AlreadyRequested { cancellation }
            | Self::Escalated { cancellation } => cancellation.validate(),
            Self::PolicyConflict { accepted, .. } => accepted.validate(),
            Self::CompletionWonRace | Self::UnknownOrRevoked => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnCancelReceipt {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub outcome: RemoteTurnCancelOutcome,
}

impl RemoteTurnCancelReceipt {
    pub fn new(
        session_id: impl Into<SessionId>,
        turn_id: impl Into<TurnId>,
        outcome: RemoteTurnCancelOutcome,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            outcome,
        }
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.outcome.validate()
    }
}
