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

    pub fn encode_json(
        &self,
        negotiated: &crate::Negotiated,
    ) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::at(negotiated, self).encode_json()
    }

    /// Decodes one JSON report after refusing a mismatched protocol version,
    /// before the report's versioned payload vocabulary is deserialized.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let report =
            crate::Envelope::<Self>::decode_json(bytes, crate::REMOTE_PROTOCOL)?.into_body();
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
        call_id: lash_sansio::ToolCallId,
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
            // A `ToolCallId` is well formed by construction.
            Self::ToolCall { session_id, .. } => {
                require_non_empty(type_name, "caused_by.session_id", session_id)
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
    /// The input was accepted, but its delivery to the engine stalled: no
    /// root took it, and none will until an operator re-arms it. Durable and
    /// not terminal.
    Stalled {
        /// Why it stalled: `attempts_exhausted`, `refused` or `undecodable`.
        reason: String,
        /// Delivery attempts made before it stalled.
        attempts: u32,
        /// The stable error code of the last delivery failure: present
        /// exactly when `last_error` is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        /// The last delivery failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_error: Option<String>,
        /// When it stalled, in milliseconds since the Unix epoch.
        stalled_at_ms: u64,
    },
}

/// The recorded answer to a sent input. Status and root are derived from
/// the variant's report or park, never stored beside them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteSendOutcome {
    Settled {
        session_id: SessionId,
        input_id: String,
        report: Box<RemoteTurnReport>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gaps: Vec<crate::observations::RemoteLiveReplayGap>,
    },
    Parked {
        session_id: SessionId,
        input_id: String,
        parked: RemoteParkedTurn,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gaps: Vec<crate::observations::RemoteLiveReplayGap>,
    },
    Stalled {
        session_id: SessionId,
        input_id: String,
        stalled: RemoteStalledDelivery,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gaps: Vec<crate::observations::RemoteLiveReplayGap>,
    },
    Withdrawn {
        session_id: SessionId,
        input_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gaps: Vec<crate::observations::RemoteLiveReplayGap>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteParkedTurn {
    pub root: TurnId,
    pub park_id: u64,
    pub reason: RemoteTurnParkReason,
    pub since_ms: u64,
    pub attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteStalledDelivery {
    pub reason: String,
    pub attempts: u32,
    /// The stable error code of the last delivery failure: present exactly
    /// when `last_error` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub stalled_at_ms: u64,
}

impl RemoteSendOutcome {
    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::Settled { session_id, .. }
            | Self::Parked { session_id, .. }
            | Self::Stalled { session_id, .. }
            | Self::Withdrawn { session_id, .. } => session_id,
        }
    }

    pub fn input_id(&self) -> &str {
        match self {
            Self::Settled { input_id, .. }
            | Self::Parked { input_id, .. }
            | Self::Stalled { input_id, .. }
            | Self::Withdrawn { input_id, .. } => input_id,
        }
    }

    pub fn root(&self) -> Option<&TurnId> {
        match self {
            Self::Settled { report, .. } => Some(&report.turn_id),
            Self::Parked { parked, .. } => Some(&parked.root),
            Self::Stalled { .. } | Self::Withdrawn { .. } => None,
        }
    }

    pub fn report(&self) -> Option<&RemoteTurnReport> {
        match self {
            Self::Settled { report, .. } => Some(report),
            Self::Parked { .. } | Self::Stalled { .. } | Self::Withdrawn { .. } => None,
        }
    }

    pub fn gaps(&self) -> &[crate::observations::RemoteLiveReplayGap] {
        match self {
            Self::Settled { gaps, .. }
            | Self::Parked { gaps, .. }
            | Self::Stalled { gaps, .. }
            | Self::Withdrawn { gaps, .. } => gaps,
        }
    }

    pub fn status(&self) -> RemoteTurnStatus {
        match self {
            Self::Settled { report, .. } => report.status(),
            Self::Parked { parked, .. } => RemoteTurnStatus::Parked {
                root: parked.root.clone(),
                park_id: parked.park_id,
                reason: parked.reason.clone(),
                since_ms: parked.since_ms,
                attempts: parked.attempts,
            },
            Self::Stalled { stalled, .. } => RemoteTurnStatus::Stalled {
                reason: stalled.reason.clone(),
                attempts: stalled.attempts,
                code: stalled.code.clone(),
                last_error: stalled.last_error.clone(),
                stalled_at_ms: stalled.stalled_at_ms,
            },
            Self::Withdrawn { .. } => RemoteTurnStatus::Cancelled,
        }
    }

    pub fn encode_json(
        &self,
        negotiated: &crate::Negotiated,
    ) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::at(negotiated, self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let outcome =
            crate::Envelope::<Self>::decode_json(bytes, crate::REMOTE_PROTOCOL)?.into_body();
        outcome.validate()?;
        Ok(outcome)
    }

    /// Validates identities and payload contents. The enum owns the
    /// relationship between a send's state and its data.
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        const TYPE: &str = "RemoteSendOutcome";
        require_non_empty(TYPE, "session_id", self.session_id())?;
        require_non_empty(TYPE, "input_id", self.input_id())?;
        for gap in self.gaps() {
            gap.validate()?;
        }
        match self {
            Self::Settled {
                session_id, report, ..
            } => {
                report.validate()?;
                if report.session_id != session_id {
                    return Err(RemoteProtocolError::InvalidEnvelope {
                        type_name: TYPE,
                        message: "the report belongs to another session".to_string(),
                    });
                }
                Ok(())
            }
            Self::Parked { parked, .. } => require_non_empty(TYPE, "parked.root", &parked.root),
            Self::Stalled { stalled, .. } => {
                if stalled.code.is_some() != stalled.last_error.is_some() {
                    return Err(RemoteProtocolError::InvalidEnvelope {
                        type_name: TYPE,
                        message: "a stalled input's last error and its code are present together"
                            .to_string(),
                    });
                }
                require_non_empty(TYPE, "stalled.reason", &stalled.reason)
            }
            Self::Withdrawn { .. } => Ok(()),
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
    /// The recorded model key the parked root could not bind, when that is
    /// why its engine retries ran out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_key: Option<String>,
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
    pub call_id: lash_sansio::ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_call_id: Option<String>,
    pub tool_name: String,
    #[serde(default)]
    pub args: serde_json::Value,
    pub output: RemoteToolCallOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteToolIntentIdentity {
    /// Who the intent was declared under: a session, or a process.
    pub owner: lash_sansio::RuntimeOwner,
    pub execution_scope_id: String,
    pub tool_call_id: lash_sansio::ToolCallId,
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
    PublishDefinition,
    GetDefinition,
    RegisterTrigger,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RemoteToolIntentRefusalReason {
    UnsupportedProtocolVersion {
        #[schemars(transform = crate::omit_schema_integer_maximum)]
        recorded: u16,
    },
    IntentIndexOverflow,
    ExecutionEnvMissing,
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
    OwnerMismatch {
        expected: String,
        recorded: String,
    },
    ForeignTriggerOwnerScope {
        expected: String,
        recorded: String,
    },
    ForeignTriggerActor {
        expected: String,
        recorded: String,
    },
    CommandFailed {
        code: String,
        message: String,
    },
    MintingGroupChildCancelled,
    DeclaredStartIdentityMismatch {
        expected: Box<RemoteToolIntentIdentity>,
        recorded: Box<RemoteToolIntentIdentity>,
    },
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
#[serde(deny_unknown_fields)]
pub struct RemoteToolCallOutput {
    pub outcome: RemoteToolCallOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<RemoteToolControlProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<lash_sansio::ToolView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_value: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteToolControlProjection {
    /// Observation data omits the seed bodies. The directed process-await
    /// reply remains the lossless carrier of a frame switch.
    SwitchAgentFrame {
        #[schemars(with = "String")]
        frame_key: lash_sansio::FrameKey,
        seed_count: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    Finish {
        value: serde_json::Value,
    },
    Fail {
        failure: RemoteToolFailure,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "status",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RemoteToolCallOutcome {
    Success(serde_json::Value),
    Failure(RemoteToolFailure),
    Cancelled(RemoteToolCancellation),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteToolFailure {
    pub class: crate::RemoteToolFailureClass,
    pub code: String,
    pub message: String,
    pub source: lash_sansio::ToolFailureSource,
    pub retry: lash_sansio::ToolRetryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteToolCancellation {
    pub message: String,
    pub source: lash_sansio::ToolFailureSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<lash_sansio::CancelOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
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
