//! Turn result envelopes: the turn result itself, outcomes, stops, assistant
//! output, usage/execution summaries, tool-call summaries, issues, and causal
//! references.

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::llm::{RemoteLlmCallRecord, validate_llm_call_record};
use crate::llm::{RemoteLlmTerminalReason, RemoteProviderFailureKind};
use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::turn_control::RemoteTurnCancellationEvidence;
use crate::usage_activity::{RemoteTurnActivity, RemoteUsage};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnReport {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub outcome: RemoteTurnOutcome,
    pub assistant_output: RemoteAssistantOutput,
    #[serde(default)]
    pub usage: RemoteTurnUsageReport,
    #[serde(default)]
    pub execution: RemoteTurnExecutionMetrics,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<RemoteToolCallRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub llm_calls: Vec<RemoteLlmCallRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<RemoteTurnIssue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activities: Vec<RemoteTurnActivity>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl RemoteTurnReport {
    pub fn status(&self) -> RemoteTurnStatus {
        RemoteTurnStatus::from(&self.outcome)
    }

    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    /// Decodes one JSON report after refusing a mismatched protocol version,
    /// before the report's versioned payload vocabulary is deserialized.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        Self::decode_json_expecting_protocol_version(bytes, crate::REMOTE_PROTOCOL_VERSION)
    }

    pub(crate) fn decode_json_expecting_protocol_version(
        bytes: &[u8],
        expected_version: u32,
    ) -> Result<Self, RemoteProtocolError> {
        let report = crate::Envelope::<Self>::decode_json_expecting_protocol_version(
            bytes,
            expected_version,
        )?
        .into_body();
        report.validate()?;
        Ok(report)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteTurnReport", "session_id", &self.session_id)?;
        require_non_empty("RemoteTurnReport", "turn_id", &self.turn_id)?;
        if let RemoteTurnOutcome::Stopped {
            stop: RemoteTurnStop::Cancelled { evidence },
        } = &self.outcome
        {
            evidence.validate()?;
        }
        let mut summary_records = HashMap::new();
        for record in &self.llm_calls {
            validate_llm_call_record(record)?;
            if summary_records
                .insert(record.call_id.as_str(), record)
                .is_some()
            {
                return Err(RemoteProtocolError::DuplicateLlmCallSummary {
                    call_id: record.call_id.clone(),
                });
            }
        }
        let mut activity_records = HashMap::new();
        for activity in &self.activities {
            activity.validate()?;
            if let crate::usage_activity::RemoteTurnEvent::ModelCallRecorded { record } =
                &activity.event
                && activity_records
                    .insert(record.call_id.as_str(), record)
                    .is_some()
            {
                return Err(RemoteProtocolError::DuplicateLlmCallActivity {
                    call_id: record.call_id.clone(),
                });
            }
        }
        for (call_id, activity_record) in &activity_records {
            let Some(summary_record) = summary_records.get(call_id) else {
                return Err(RemoteProtocolError::MissingLlmCallSummary {
                    call_id: (*call_id).to_string(),
                });
            };
            if summary_record != activity_record {
                return Err(RemoteProtocolError::ConflictingLlmCallRecord {
                    call_id: (*call_id).to_string(),
                });
            }
        }
        for call_id in summary_records.keys() {
            if !activity_records.contains_key(call_id) {
                return Err(RemoteProtocolError::MissingLlmCallActivity {
                    call_id: (*call_id).to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteCausalRef {
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Effect {
        address: lash_sansio::EffectAddress,
    },
    ToolCall {
        session_id: SessionId,
        call_id: String,
    },
    Process {
        process_id: ProcessId,
    },
    ProcessEvent {
        process_id: ProcessId,
        sequence: u64,
    },
    TriggerOccurrence {
        occurrence_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_incarnation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_revision: Option<u64>,
    },
    SessionNode {
        session_id: SessionId,
        node_id: String,
    },
}

impl RemoteCausalRef {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        use crate::registry_errors::require_non_empty;
        match self {
            Self::Turn {
                session_id,
                turn_id,
            } => {
                require_non_empty(type_name, "caused_by.session_id", session_id)?;
                require_non_empty(type_name, "caused_by.turn_id", turn_id)
            }
            Self::Effect { address } => {
                address
                    .validate()
                    .map_err(|error| RemoteProtocolError::InvalidEnvelope {
                        type_name,
                        message: format!("caused_by.address: {error}"),
                    })
            }
            Self::ToolCall {
                session_id,
                call_id,
            } => {
                require_non_empty(type_name, "caused_by.session_id", session_id)?;
                require_non_empty(type_name, "caused_by.call_id", call_id)
            }
            Self::Process { process_id } | Self::ProcessEvent { process_id, .. } => {
                require_non_empty(type_name, "caused_by.process_id", process_id)
            }
            Self::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                subscription_incarnation,
                ..
            } => {
                require_non_empty(type_name, "caused_by.occurrence_id", occurrence_id)?;
                if let Some(subscription_id) = subscription_id {
                    require_non_empty(type_name, "caused_by.subscription_id", subscription_id)?;
                }
                if let Some(incarnation) = subscription_incarnation {
                    require_non_empty(
                        type_name,
                        "caused_by.subscription_incarnation",
                        incarnation,
                    )?;
                }
                Ok(())
            }
            Self::SessionNode {
                session_id,
                node_id,
            } => {
                require_non_empty(type_name, "caused_by.session_id", session_id)?;
                require_non_empty(type_name, "caused_by.node_id", node_id)
            }
        }
    }
}

/// What a sent input's root answered: the remote form of a send's outcome
/// status. [`RemoteTurnReport::status`] derives the three terminal arms from
/// a report; `Parked` has no report, because a parked root has not settled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnStatus {
    Answered,
    Failed,
    Cancelled,
    /// The root parked: durable and not terminal. It holds its claims until
    /// an operator or a later build resolves the park.
    Parked {
        root: TurnId,
        /// The park's id: the feed sequence it opened with.
        park_id: u64,
        reason: RemoteTurnParkReason,
        /// When the park opened, in milliseconds since the Unix epoch.
        since_ms: u64,
        /// How many drives met the park's refusal.
        attempts: u32,
    },
}

/// What a sent input's root answered, for a transport: the four-way status,
/// the settled report when the root ran, and the ids a peer re-attaches by.
///
/// A settled turn is only one of the four answers: a parked root holds its
/// work and has no report yet, and an input withdrawn before any root took it
/// has neither report nor root. A peer resumes a parked or unfinished send by
/// `input_id`, never by fabricating a failed turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSendOutcome {
    pub session_id: SessionId,
    /// The accepted input's id.
    pub input_id: String,
    /// The root that took the input; `None` only for an input withdrawn
    /// before any root took it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<TurnId>,
    pub status: RemoteTurnStatus,
    /// The settled turn: present for Answered and Failed, for a Cancelled
    /// root that ran, and never for Parked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<RemoteTurnReport>,
    /// Where the report's activity list is incomplete: the follower lost
    /// replay events, or the root ran where it could not be observed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<crate::observations::RemoteLiveReplayGap>,
}

impl RemoteSendOutcome {
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    /// Decodes one JSON outcome after refusing a mismatched protocol
    /// version, then refuses an inconsistent one.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let outcome = crate::Envelope::<Self>::decode_json_expecting_protocol_version(
            bytes,
            crate::REMOTE_PROTOCOL_VERSION,
        )?
        .into_body();
        outcome.validate()?;
        Ok(outcome)
    }

    /// The status, root and report agree: a report's own status is the
    /// outcome's; Answered and Failed carry one; Parked carries none and names
    /// its root; only an input no root took lacks a root.
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        const TYPE: &str = "RemoteSendOutcome";
        require_non_empty(TYPE, "session_id", &self.session_id)?;
        require_non_empty(TYPE, "input_id", &self.input_id)?;
        let invalid = |message: &str| RemoteProtocolError::InvalidEnvelope {
            type_name: TYPE,
            message: message.to_string(),
        };
        if let Some(root) = &self.root_id {
            require_non_empty(TYPE, "root_id", root)?;
        }
        if let Some(report) = &self.report {
            report.validate()?;
            if report.status() != self.status {
                return Err(invalid("the report's status is not the outcome's"));
            }
            if report.session_id != self.session_id {
                return Err(invalid("the report belongs to another session"));
            }
            if self.root_id.is_none() {
                return Err(invalid("a settled report names the root that ran it"));
            }
        }
        for gap in &self.gaps {
            gap.validate()?;
        }
        match &self.status {
            RemoteTurnStatus::Answered | RemoteTurnStatus::Failed if self.report.is_none() => {
                Err(invalid("an Answered or Failed outcome carries its report"))
            }
            RemoteTurnStatus::Parked { root, .. } => {
                if self.report.is_some() {
                    return Err(invalid("a parked root has no settled report"));
                }
                require_non_empty(TYPE, "status.root", root)?;
                if self.root_id.as_ref() != Some(root) {
                    return Err(invalid("a parked outcome's root is its park's root"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Why a root parked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnParkReason {
    /// The reason's code, as the park store keys it (`replay_divergence`,
    /// `binding_drift`, ...).
    pub code: String,
    /// The operator-facing refusal message.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnOutcome {
    Finished { finish: RemoteTurnFinish },
    AgentFrameSwitch { frame_key: String, task: String },
    Stopped { stop: RemoteTurnStop },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnFinish {
    AssistantMessage {
        text: String,
    },
    FinalValue {
        value: serde_json::Value,
    },
    ToolValue {
        tool_name: String,
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnStop {
    Cancelled {
        evidence: RemoteTurnCancellationEvidence,
    },
    Incomplete,
    InvalidInput,
    MaxTurns,
    ToolFailure,
    ProviderError,
    ContextOverflow,
    PluginAbort,
    RuntimeError,
    SubmittedError {
        value: serde_json::Value,
    },
    ToolError {
        tool_name: String,
        value: serde_json::Value,
    },
}

impl From<&RemoteTurnOutcome> for RemoteTurnStatus {
    fn from(value: &RemoteTurnOutcome) -> Self {
        match value {
            RemoteTurnOutcome::Finished { .. } | RemoteTurnOutcome::AgentFrameSwitch { .. } => {
                Self::Answered
            }
            RemoteTurnOutcome::Stopped {
                stop: RemoteTurnStop::Cancelled { .. },
            } => Self::Cancelled,
            RemoteTurnOutcome::Stopped { .. } => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteAssistantOutput {
    #[serde(default)]
    pub safe_text: String,
    #[serde(default)]
    pub raw_text: String,
    #[serde(default)]
    pub state: RemoteAssistantOutputState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAssistantOutputState {
    #[default]
    Usable,
    EmptyOutput,
    TracebackOnly,
    RecoveredFromError,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnUsageReport {
    #[serde(default)]
    pub usage: RemoteUsage,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnExecutionMetrics {
    #[serde(default)]
    pub had_tool_calls: bool,
    #[serde(default)]
    pub had_code_execution: bool,
    /// Wall-clock turn start (epoch milliseconds), measured from turn claim.
    /// `0` when the producer predates the field.
    #[serde(default)]
    pub started_at_ms: u64,
    /// Whole-turn duration in milliseconds (claim → final commit). `0` when
    /// the producer predates the field.
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteToolCallRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub tool_name: String,
    #[serde(default)]
    pub args: serde_json::Value,
    pub outcome: RemoteToolCallOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteToolIntentIdentity {
    pub session_id: SessionId,
    pub execution_scope_id: String,
    pub tool_call_id: String,
    pub intent_index: u32,
    pub replay_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minting_emission_replay_key: Option<String>,
}

/// Wire mirror of the core tool-intent kind set.
///
/// Deliberately spelled out rather than generated from
/// `lash_sansio::tool_intent_variants!`: the version-bump gate projects this
/// enum's literal text, so generating it would move the variant list out of
/// the `REMOTE_PROTOCOL_VERSION` guard's sight. The two `From` impls in
/// `core_conversions::turn_result` are generated from that list instead, so a
/// variant added to the core set fails to compile until it is added here and
/// the wire version is bumped (FIG-2994).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteToolIntentKind {
    StartProcess,
    SignalProcess,
    CancelProcess,
    EmitProcessEvent,
    EmitTrigger,
    RegisterProcessDefinition,
    RegisterTrigger,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RemoteToolIntentRefusalReason {
    UnsupportedProtocolVersion {
        recorded: u16,
    },
    MissingToolCallId,
    IntentIndexOverflow,
    CountBudgetExceeded {
        actual: usize,
        maximum: usize,
    },
    CanonicalByteBudgetExceeded {
        actual: usize,
        maximum: usize,
    },
    PerKindBudgetExceeded {
        kind: RemoteToolIntentKind,
        actual: usize,
        maximum: usize,
    },
    SessionMismatch {
        expected: String,
        recorded: String,
    },
    CommandFailed {
        code: String,
        message: String,
    },
    MintingGroupChildCancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RemoteToolIntentExecutionOutcome {
    Executed {
        identity: RemoteToolIntentIdentity,
        kind: RemoteToolIntentKind,
        result: serde_json::Value,
    },
    Refused {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<RemoteToolIntentIdentity>,
        intent_index: u32,
        kind: RemoteToolIntentKind,
        refusal: RemoteToolIntentRefusalReason,
    },
    ProtocolRefused {
        refusal: RemoteToolIntentRefusalReason,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum RemoteToolCallOutcome {
    Success(serde_json::Value),
    Failure(serde_json::Value),
    Cancelled(serde_json::Value),
}

/// Namespaced failure code, as carried to a host.
pub use lash_sansio::FailureCode as RemoteFailureCode;
/// Typed origin of a turn failure, as carried to a host.
pub use lash_sansio::TurnFailureKind as RemoteTurnFailureKind;

/// Producer-selected effect of an issue on turn completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnIssueSeverity {
    Advisory,
    Blocking,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnIssue {
    pub severity: RemoteTurnIssueSeverity,
    /// Typed origin of the failure. Serializes as the same snake_case string
    /// the field carried before it was typed; an unrecognized spelling decodes
    /// into `RemoteTurnFailureKind::Unknown` rather than failing.
    pub kind: RemoteTurnFailureKind,
    /// The failure's namespaced code (`<namespace>:<spelling>`): `lash`
    /// carries this workspace's [`TurnFailureCode`](lash_sansio::TurnFailureCode)
    /// vocabulary, `provider` carries codes the provider emitted on the wire,
    /// and host or plugin vocabularies keep their own namespaces verbatim —
    /// never reinterpreted into a Lash spelling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<RemoteFailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<RemoteLlmTerminalReason>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Typed retryability signal; `None` when the source did not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Typed provider-failure classification, present only for classified
    /// LLM provider/transport failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_failure_kind: Option<RemoteProviderFailureKind>,
}

#[cfg(test)]
#[path = "turn_result_tests.rs"]
mod turn_result_tests;
