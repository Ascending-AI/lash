//! Turn result envelopes: the turn result itself, outcomes, stops, assistant
//! output, usage/execution summaries, tool-call summaries, issues, and causal
//! references.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ensure_protocol_version;
use crate::llm::{RemoteLlmCallRecord, validate_llm_call_record};
use crate::llm::{RemoteLlmTerminalReason, RemoteProviderFailureKind};
use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::turn_control::RemoteTurnCancellationEvidence;
use crate::usage_activity::{RemoteTokenLedgerEntry, RemoteTurnActivity, RemoteUsage};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnReport {
    pub protocol_version: u32,
    pub session_id: String,
    pub turn_id: String,
    /// Derived from `outcome` on encode and checked against it on decode.
    pub status: RemoteTurnStatus,
    pub outcome: RemoteTurnOutcome,
    /// Wire projection of the cancellation evidence carried by
    /// `outcome`. Present exactly when `status == cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<RemoteTurnCancellationEvidence>,
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
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteTurnReport", "session_id", &self.session_id)?;
        require_non_empty("RemoteTurnReport", "turn_id", &self.turn_id)?;
        if (self.status == RemoteTurnStatus::Cancelled) != self.cancellation.is_some() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteTurnReport",
                message: "cancellation evidence must be present if and only if status is cancelled"
                    .to_string(),
            });
        }
        let expected_status = RemoteTurnStatus::from(&self.outcome);
        if self.status != expected_status {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteTurnReport",
                message: format!("turn status `{:?}` contradicts its outcome", self.status),
            });
        }
        if let Some(cancellation) = self.cancellation.as_ref() {
            cancellation.validate()?;
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
            if activity.protocol_version != self.protocol_version {
                return Err(RemoteProtocolError::MismatchedNestedProtocolVersion {
                    parent: "RemoteTurnReport",
                    child: "activities",
                    parent_version: self.protocol_version,
                    child_version: activity.protocol_version,
                });
            }
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
        session_id: String,
        turn_id: String,
    },
    Effect {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        effect_id: String,
    },
    ToolCall {
        session_id: String,
        call_id: String,
    },
    Process {
        process_id: String,
    },
    ProcessEvent {
        process_id: String,
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
        session_id: String,
        node_id: String,
    },
}

/// Derived projection of [`RemoteTurnOutcome`]. Producers compute it with
/// `RemoteTurnStatus::from(&outcome)`; consumers get it checked against the
/// outcome by [`RemoteTurnReport::validate`]. It never states a fact the
/// outcome does not already carry, so there is no in-progress status: a turn
/// report exists only once the turn has a terminal outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnStatus {
    #[default]
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnOutcome {
    Finished { finish: RemoteTurnFinish },
    AgentFrameSwitch { frame_key: String, task: String },
    Stopped { stop: RemoteTurnStop },
}

impl Default for RemoteTurnOutcome {
    fn default() -> Self {
        Self::Stopped {
            stop: RemoteTurnStop::Incomplete,
        }
    }
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
    Cancelled,
    Incomplete,
    InvalidInput,
    MaxTurns,
    ToolFailure,
    ProviderError,
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
                Self::Completed
            }
            RemoteTurnOutcome::Stopped {
                stop: RemoteTurnStop::Cancelled,
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
    pub parent: RemoteUsage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<RemoteTokenLedgerEntry>,
    #[serde(default)]
    pub total: RemoteUsage,
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
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteToolIntentIdentity {
    pub session_id: String,
    pub execution_scope_id: String,
    pub tool_call_id: String,
    pub intent_index: u32,
    pub replay_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteToolIntentKind {
    StartProcess,
    SignalProcess,
    CancelProcess,
    EmitProcessEvent,
    EmitTrigger,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessParentEndPolicy {
    Abandon,
    #[default]
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteToolIntentParentEnd {
    pub process_id: String,
    pub policy: RemoteProcessParentEndPolicy,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RemoteToolIntentExecutionOutcome {
    Executed {
        identity: RemoteToolIntentIdentity,
        kind: RemoteToolIntentKind,
        result: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_end: Option<RemoteToolIntentParentEnd>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnIssue {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
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
