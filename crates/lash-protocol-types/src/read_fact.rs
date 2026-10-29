//! Tool reads retained by the caller's next save, without admitted executions.

/// Whether tool-call activity describes a recorded call or an unrecorded read.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallExecution {
    #[default]
    Recorded,
    Read,
}

impl ToolCallExecution {
    pub fn is_recorded(&self) -> bool {
        *self == Self::Recorded
    }
}

/// The stable read metadata retained with a tool-call record. Live timing and
/// byte measurements are provisional and remain on activity events.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ToolReadIdentity {
    pub caller: ToolReadCaller,
    pub provider: lash_vocabulary::providers::ToolProviderHandle,
    pub ordinal: u64,
    pub site: String,
    pub origin: String,
}

/// The caller of an unrecorded tool read. This is observation identity only.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolReadCaller {
    CodeBlock {
        graph_key: String,
    },
    Process {
        process_id: lash_vocabulary::identity::ProcessId,
    },
    ModelRound {
        round_key: String,
    },
}

/// A provider panic has no admitted call id or attempt ordinal.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ReadPanic {
    pub tool_name: String,
    pub message: String,
}

impl ReadPanic {
    pub fn failure(&self) -> crate::ToolFailure {
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "tool_read_panicked",
            self.message.clone(),
        )
        .with_cause(crate::ToolFailureCause::ReadPanicked {
            panic: self.clone(),
        })
    }
}

/// The complete fact saved with a cell, process park or model tool results.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadFact {
    pub caller: ToolReadCaller,
    pub ordinal: u64,
    pub call_id: crate::ToolCallId,
    pub provider: lash_vocabulary::providers::ToolProviderHandle,
    pub tool: String,
    pub args: serde_json::Value,
    #[serde(serialize_with = "crate::durable_serde::serialize")]
    pub outcome: crate::ToolCallOutcome,
    pub site: String,
    pub origin: String,
}

impl ReadFact {
    pub fn record(&self) -> crate::ToolCallRecord {
        crate::ToolCallRecord {
            execution: crate::ToolCallExecution::Read,
            read: Some(Box::new(crate::ToolReadIdentity {
                caller: self.caller.clone(),
                provider: self.provider.clone(),
                ordinal: self.ordinal,
                site: self.site.clone(),
                origin: self.origin.clone(),
            })),
            call_id: self.call_id.clone(),
            provider_call_id: None,
            tool: self.tool.clone(),
            args: self.args.clone(),
            output: crate::ToolCallOutput {
                outcome: self.outcome.clone(),
                control: None,
                view: None,
                projection_value: None,
            },
        }
    }
}

/// An ordinary read refusal, or the terminal provider panic its owner must save.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadFailure {
    Arguments(String),
    Tool(Box<crate::ToolFailure>),
    Reader {
        message: String,
        data: serde_json::Value,
    },
    Cancelled,
    TimedOut,
    Panicked(crate::ReadPanic),
}

impl ReadFailure {
    pub fn output(&self) -> crate::ToolCallOutput {
        let failure = match self {
            Self::Tool(failure) => (**failure).clone(),
            Self::Panicked(panic) => panic.failure(),
            Self::Arguments(message) => crate::ToolFailure::runtime(
                crate::ToolFailureClass::InvalidRequest,
                "read_arguments",
                message.clone(),
            ),
            Self::Reader { message, data } => {
                crate::ToolFailure::new("read_failed", message.clone())
                    .with_raw(crate::ToolValue::untrusted_json(data.clone()))
            }
            Self::Cancelled => {
                return crate::ToolCallOutput::cancelled(crate::ToolCancellation::runtime(
                    "the read was stopped with its owner",
                ));
            }
            Self::TimedOut => crate::ToolFailure::runtime(
                crate::ToolFailureClass::Timeout,
                "read_timed_out",
                "the read exceeded its tool or parent deadline",
            ),
        };
        crate::ToolCallOutput::failure(failure)
    }
}

/// A read answer and the fact that its caller must retain at its next save.
pub struct ReadResult {
    pub fact: ReadFact,
    pub answer: Result<serde_json::Value, ReadFailure>,
}
