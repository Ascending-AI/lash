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
        terminal_label: RemoteRetiredProcessStatus,
        pruned_at_ms: u64,
    },
}

/// Mirrors [`lash_core::ProcessTerminal`]: the outcome a process ended in,
/// which a terminal record and a terminal event carry. Its status is derived
/// from it ([`Self::status`]) and carried nowhere beside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteProcessTerminal {
    Settled {
        output: RemoteProcessToolCallOutput,
    },
    Abandoned {
        evidence: RemoteAbandonEvidence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<serde_json::Value>,
    },
}

impl RemoteProcessTerminal {
    /// The status this outcome ends a process in.
    pub fn status(&self) -> RemoteTerminalProcessStatus {
        match self {
            Self::Settled { output } => match &output.outcome {
                RemoteProcessToolCallOutcome::Success(_) => RemoteTerminalProcessStatus::Completed,
                RemoteProcessToolCallOutcome::Failure(_) => RemoteTerminalProcessStatus::Failed,
                RemoteProcessToolCallOutcome::Cancelled(_) => {
                    RemoteTerminalProcessStatus::Cancelled
                }
            },
            Self::Abandoned { .. } => RemoteTerminalProcessStatus::Abandoned,
        }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        RemoteProcessAwaitOutput::from(self.clone()).validate(type_name)
    }
}

impl From<RemoteProcessTerminal> for RemoteProcessAwaitOutput {
    fn from(terminal: RemoteProcessTerminal) -> Self {
        match terminal {
            RemoteProcessTerminal::Settled { output } => Self::Settled { output },
            RemoteProcessTerminal::Abandoned { evidence, control } => {
                Self::Abandoned { evidence, control }
            }
        }
    }
}

impl TryFrom<RemoteProcessAwaitOutput> for RemoteProcessTerminal {
    type Error = RemoteProtocolError;

    fn try_from(output: RemoteProcessAwaitOutput) -> Result<Self, Self::Error> {
        match output {
            RemoteProcessAwaitOutput::Settled { output } => Ok(Self::Settled { output }),
            RemoteProcessAwaitOutput::Abandoned { evidence, control } => {
                Ok(Self::Abandoned { evidence, control })
            }
            RemoteProcessAwaitOutput::NoLongerRetained { .. } => {
                Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessTerminal",
                    message: "a pruned process has no retained outcome".to_string(),
                })
            }
        }
    }
}

impl RemoteProcessAwaitOutput {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<lash_sansio::ToolView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_value: Option<serde_json::Value>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<lash_sansio::ToolFailureCause>>,
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

/// Typed classification of the terminal outcome an observed process's display
/// error string summarizes (FIG-3094).
///
/// A polling host reads this instead of matching the prose in
/// [`RemoteObservedProcess::error`](super::RemoteObservedProcess::error): the
/// variant separates a failure from a cancellation, `class` is the failure's
/// typed class, and `origin` is the typed cancellation origin when the
/// settling side recorded one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteObservedProcessFailure {
    /// The process settled with a tool failure.
    Failed {
        /// Typed failure class, closed and matchable.
        class: RemoteToolFailureClass,
        /// The producer-authored failure code. An open vocabulary this
        /// protocol does not own, carried verbatim beside the typed `class`.
        code: String,
    },
    /// The process settled cancelled.
    Cancelled {
        /// Typed cancellation origin, when the settling side recorded one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<lash_sansio::CancelOrigin>,
    },
}
