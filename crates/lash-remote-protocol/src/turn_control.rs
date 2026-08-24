//! Transport-neutral foreground-turn cancellation envelopes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::{REMOTE_PROTOCOL_VERSION, ensure_protocol_version};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnCancelDisposition {
    #[default]
    Defer,
    Drop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnCancellationEvidence {
    pub request_id: String,
    /// Opaque host-domain data; Lash records and returns it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "remote_disposition_is_defer")]
    pub undelivered: RemoteTurnCancelDisposition,
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
    pub protocol_version: u32,
    pub session_id: String,
    pub turn_id: String,
    pub request_id: String,
    /// Opaque host-domain data; Lash never interprets this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "remote_disposition_is_defer")]
    pub undelivered: RemoteTurnCancelDisposition,
}

fn remote_disposition_is_defer(value: &RemoteTurnCancelDisposition) -> bool {
    matches!(value, RemoteTurnCancelDisposition::Defer)
}

impl RemoteTurnCancelRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteTurnCancelRequest", "session_id", &self.session_id)?;
        require_non_empty("RemoteTurnCancelRequest", "turn_id", &self.turn_id)?;
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
    CompletionWonRace,
    UnknownOrRevoked,
}

impl RemoteTurnCancelOutcome {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Requested { cancellation } | Self::AlreadyRequested { cancellation } => {
                cancellation.validate()
            }
            Self::CompletionWonRace | Self::UnknownOrRevoked => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnCancelReceipt {
    pub protocol_version: u32,
    pub session_id: String,
    pub turn_id: String,
    pub outcome: RemoteTurnCancelOutcome,
}

impl RemoteTurnCancelReceipt {
    pub fn new(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        outcome: RemoteTurnCancelOutcome,
    ) -> Self {
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            outcome,
        }
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteTurnCancelReceipt", "session_id", &self.session_id)?;
        require_non_empty("RemoteTurnCancelReceipt", "turn_id", &self.turn_id)?;
        self.outcome.validate()
    }
}
