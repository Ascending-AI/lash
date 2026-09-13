//! Terminal process outcomes: the await output a settled process reports, and
//! the tool-call result, failure, and cancellation shapes it carries.

use super::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteProcessAwaitOutput {
    Settled {
        output: RemoteProcessToolCallOutput,
    },
    Abandoned {
        evidence: RemoteAbandonEvidence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<serde_json::Value>,
    },
    NoLongerRetained {
        terminal_label: String,
        pruned_at_ms: u64,
    },
}

impl RemoteProcessAwaitOutput {
    pub(super) fn terminal_status(&self) -> Option<RemoteProcessStatus> {
        match self {
            Self::Settled { output } => Some(match &output.outcome {
                RemoteProcessToolCallOutcome::Success(_) => RemoteProcessStatus::Completed,
                RemoteProcessToolCallOutcome::Failure(_) => RemoteProcessStatus::Failed,
                RemoteProcessToolCallOutcome::Cancelled(_) => RemoteProcessStatus::Cancelled,
            }),
            Self::Abandoned { .. } => Some(RemoteProcessStatus::Abandoned),
            Self::NoLongerRetained { .. } => None,
        }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Settled { output } => output.validate(type_name),
            Self::Abandoned { evidence, .. } => match &evidence.owner {
                Some(owner) => owner.validate(type_name),
                None => Ok(()),
            },
            Self::NoLongerRetained { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessToolCallOutput {
    pub outcome: RemoteProcessToolCallOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<serde_json::Value>,
}

impl RemoteProcessToolCallOutput {
    fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match &self.outcome {
            RemoteProcessToolCallOutcome::Success(_) => Ok(()),
            RemoteProcessToolCallOutcome::Failure(failure) => {
                require_non_empty(type_name, "await_output.output.outcome.code", &failure.code)?;
                require_non_empty(
                    type_name,
                    "await_output.output.outcome.message",
                    &failure.message,
                )
            }
            RemoteProcessToolCallOutcome::Cancelled(cancellation) => require_non_empty(
                type_name,
                "await_output.output.outcome.message",
                &cancellation.message,
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum RemoteProcessToolCallOutcome {
    Success(serde_json::Value),
    Failure(RemoteProcessToolFailure),
    Cancelled(RemoteProcessToolCancellation),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessToolFailure {
    pub class: RemoteToolFailureClass,
    pub code: String,
    pub message: String,
    pub source: RemoteProcessToolFailureSource,
    pub retry: RemoteProcessToolRetryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessToolFailureSource {
    Runtime,
    Tool,
    Plugin,
    Policy,
    Cancellation,
    UnknownLegacy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteProcessToolRetryStatus {
    Never,
    Safe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_ms: Option<u64>,
    },
    Exhausted {
        attempts: u32,
    },
    UnknownLegacy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessToolCancellation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<lash_sansio::CancelOrigin>,
    pub message: String,
    pub source: RemoteProcessToolFailureSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteToolFailureClass {
    InvalidRequest,
    Io,
    Unavailable,
    PermissionDenied,
    Timeout,
    Execution,
    External,
    ResourceLimit,
    Internal,
}
