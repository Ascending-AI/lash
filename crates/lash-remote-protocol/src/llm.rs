//! LLM request/response envelopes: messages, attachments, tool specs, output
//! specs, provider metadata, and schema-projection contracts.

use std::collections::{BTreeMap, HashMap};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ensure_protocol_version;
use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::usage_activity::RemoteUsage;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSchemaContract {
    pub canonical: serde_json::Value,
    #[serde(
        default,
        skip_serializing_if = "RemoteSchemaProjectionPolicy::is_default"
    )]
    pub projection: RemoteSchemaProjectionPolicy,
}

impl RemoteSchemaContract {
    fn new(canonical: serde_json::Value) -> Self {
        Self {
            canonical,
            projection: RemoteSchemaProjectionPolicy::default(),
        }
    }
}

impl Default for RemoteSchemaContract {
    fn default() -> Self {
        Self::new(serde_json::Value::Null)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSchemaProjectionPolicy {
    #[serde(default, skip_serializing_if = "RemoteProjectionMode::is_auto")]
    pub mode: RemoteProjectionMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<RemoteSchemaProjectionOverride>,
}

impl RemoteSchemaProjectionPolicy {
    fn is_default(&self) -> bool {
        self.mode == RemoteProjectionMode::Auto && self.overrides.is_empty()
    }
}

impl Default for RemoteSchemaProjectionPolicy {
    fn default() -> Self {
        Self {
            mode: RemoteProjectionMode::Auto,
            overrides: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProjectionMode {
    #[default]
    Auto,
    ExplicitOnly,
    Exact,
}

impl RemoteProjectionMode {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSchemaProjectionOverride {
    pub dialect: String,
    pub schema: serde_json::Value,
}

pub(crate) fn default_remote_input_schema() -> RemoteSchemaContract {
    RemoteSchemaContract::new(serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": true
    }))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLlmRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub scope: RemoteLlmRequestScope,
    pub model_intent: RemoteModelIntent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<RemoteLlmMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<RemoteAttachmentSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<RemoteLlmToolSpec>,
    #[serde(default)]
    pub tool_choice: RemoteLlmToolChoice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_spec: Option<RemoteLlmOutputSpec>,
    #[serde(default, skip_serializing_if = "RemoteGenerationOptions::is_empty")]
    pub generation: RemoteGenerationOptions,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl RemoteLlmRequest {
    /// Decode one JSON request with the protocol-version refusal ahead of the
    /// nested LLM vocabulary.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        Self::decode_json_expecting_protocol_version(bytes, crate::REMOTE_PROTOCOL_VERSION)
    }

    pub(crate) fn decode_json_expecting_protocol_version(
        bytes: &[u8],
        expected_version: u32,
    ) -> Result<Self, RemoteProtocolError> {
        #[derive(Deserialize)]
        struct VersionProbe {
            protocol_version: u32,
        }

        let probe: VersionProbe = serde_json::from_slice(bytes)?;
        if probe.protocol_version != expected_version {
            return Err(RemoteProtocolError::UnsupportedProtocolVersion {
                actual: probe.protocol_version,
                expected: expected_version,
            });
        }
        let request: Self = serde_json::from_slice(bytes)?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteLlmRequest", "request_id", &self.request_id)?;
        self.scope.validate()?;
        self.model_intent.validate()?;
        self.generation.validate("RemoteLlmRequest")?;
        for (index, message) in self.messages.iter().enumerate() {
            message.validate(index)?;
        }
        for (index, attachment) in self.attachments.iter().enumerate() {
            attachment.validate(index)?;
        }
        for tool in &self.tools {
            tool.validate()?;
        }
        if let Some(output_spec) = &self.output_spec {
            output_spec.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLlmResponse {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(default)]
    pub full_text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_parts: Vec<RemoteLlmOutputPart>,
    #[serde(default)]
    pub usage: RemoteUsage,
    #[serde(default)]
    pub terminal_reason: RemoteLlmTerminalReason,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<RemoteDiagnostic>,
    #[serde(default, skip_serializing_if = "RemoteProviderMetadata::is_empty")]
    pub provider_metadata: RemoteProviderMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_evidence: Option<RemoteExecutionEvidence>,
    /// Which of the caller's generation options the worker's adapter put on
    /// the wire. Absent when the adapter does not report, which is distinct
    /// from a report that nothing was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<RemoteGenerationDisposition>,
}

/// Mirror of the core `GenerationDisposition`: the adapter-reported fate of a
/// request's generation and prompt-cache intent, so a remote host can tell an
/// honored repeatability request from a silently dropped one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteGenerationDisposition {
    #[serde(default)]
    pub output_token_cap: RemoteGenerationOptionDisposition,
    #[serde(default)]
    pub temperature: RemoteGenerationOptionDisposition,
    #[serde(default)]
    pub seed: RemoteGenerationOptionDisposition,
    #[serde(default)]
    pub stop_sequences: RemoteGenerationOptionDisposition,
    #[serde(default)]
    pub cache: RemoteGenerationOptionDisposition,
}

/// Mirror of the core `GenerationOptionDisposition`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteGenerationOptionDisposition {
    #[default]
    NotRequested,
    Applied,
    SuppressedProtocolOwned,
    OmittedUnsupported,
    OmittedSamplingPinned,
    ClampedToCapacity,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteExecutionEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_interruption: Option<RemoteExecutionEvidenceCollectionInterruption>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteExecutionEvidenceCollectionInterruption {
    ProtocolAbort,
}

/// Wire mirror of one logical LLM call and every provider attempt it consumed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLlmCallRecord {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replay_drops: Vec<RemoteProviderReplayDrop>,
    pub attempts: Vec<RemoteAttemptRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProviderReplayDrop {
    pub kind: RemoteProviderReplayKind,
    pub reason: RemoteProviderReplayDropReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minting_route: Option<RemoteProviderRouteIdentity>,
    pub serving_route: RemoteProviderRouteIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProviderReplayKind {
    ResponseText,
    Reasoning,
    ToolCall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProviderReplayDropReason {
    Unstamped,
    ForeignRoute,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteAttemptRecord {
    pub ordinal: u32,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub outcome: RemoteAttemptOutcome,
    pub protocol_position: RemoteProtocolPosition,
    pub retry_budget_consumed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_decision: Option<RemoteRetryDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RemoteNormalizedError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<RemoteExecutionEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<RemoteGenerationDisposition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<RemoteUsage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAttemptOutcome {
    Completed,
    Failed,
    Aborted,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProtocolPosition {
    NoResponse,
    ResponseObserved,
    OutputStarted,
    TerminalObserved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteRetryDecision {
    pub scheduled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteNormalizedError {
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

pub(crate) fn validate_llm_call_record(
    record: &RemoteLlmCallRecord,
) -> Result<(), RemoteProtocolError> {
    require_non_empty("RemoteLlmCallRecord", "call_id", &record.call_id)?;
    for drop in &record.replay_drops {
        if let Some(route) = &drop.minting_route {
            route.validate("RemoteProviderReplayDrop.minting_route")?;
        }
        drop.serving_route
            .validate("RemoteProviderReplayDrop.serving_route")?;
    }
    if record.attempts.is_empty() {
        return Err(RemoteProtocolError::InvalidEnvelope {
            type_name: "RemoteLlmCallRecord",
            message: "attempts must contain at least one provider attempt".to_string(),
        });
    }
    for (index, attempt) in record.attempts.iter().enumerate() {
        let expected =
            u32::try_from(index + 1).map_err(|_| RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteLlmCallRecord",
                message: "attempt count exceeds the supported ordinal range".to_string(),
            })?;
        if attempt.ordinal != expected {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteLlmCallRecord",
                message: format!(
                    "attempt ordinal {} must equal its one-based position {expected}",
                    attempt.ordinal
                ),
            });
        }
        if let Some(error) = &attempt.error {
            require_non_empty("RemoteNormalizedError", "class", &error.class)?;
        }
        match attempt.outcome {
            RemoteAttemptOutcome::Completed => {
                if attempt.protocol_position != RemoteProtocolPosition::TerminalObserved {
                    return Err(RemoteProtocolError::InvalidEnvelope {
                        type_name: "RemoteLlmCallRecord",
                        message: format!(
                            "completed attempt {} must have terminal_observed protocol position",
                            attempt.ordinal
                        ),
                    });
                }
                if attempt.error.is_some() {
                    return Err(RemoteProtocolError::InvalidEnvelope {
                        type_name: "RemoteLlmCallRecord",
                        message: format!(
                            "completed attempt {} must not carry an error",
                            attempt.ordinal
                        ),
                    });
                }
                if attempt
                    .retry_decision
                    .as_ref()
                    .is_some_and(|decision| decision.scheduled)
                {
                    return Err(RemoteProtocolError::InvalidEnvelope {
                        type_name: "RemoteLlmCallRecord",
                        message: format!(
                            "completed attempt {} must not schedule a retry",
                            attempt.ordinal
                        ),
                    });
                }
            }
            RemoteAttemptOutcome::Failed if attempt.error.is_none() => {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteLlmCallRecord",
                    message: format!("failed attempt {} must carry an error", attempt.ordinal),
                });
            }
            RemoteAttemptOutcome::Failed
            | RemoteAttemptOutcome::Aborted
            | RemoteAttemptOutcome::Interrupted => {}
        }
        if attempt
            .retry_decision
            .as_ref()
            .is_some_and(|decision| decision.scheduled)
            && index + 1 == record.attempts.len()
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteLlmCallRecord",
                message: format!(
                    "attempt {} schedules a retry but no following attempt is sealed",
                    attempt.ordinal
                ),
            });
        }
    }
    Ok(())
}

impl RemoteLlmResponse {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteLlmResponse", "request_id", &self.request_id)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteModelIntent {
    pub model: String,
    #[serde(default)]
    pub variant: RemoteReasoningSelection,
    /// Host-supplied capability metadata for the model (mirrors the core
    /// `ModelCapability` contract).
    #[serde(default, skip_serializing_if = "RemoteModelCapability::is_empty")]
    pub capability: RemoteModelCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// Mirror of the core `ModelCapability`: host-supplied model capability
/// metadata carried with the model intent so remote workers validate and
/// encode effort exactly like a local runtime.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteModelCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<RemoteReasoningCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<RemoteCacheControlDialect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_termination: Option<RemoteStreamTermination>,
    /// Whether this model lets a caller set the sampling temperature.
    #[serde(default, skip_serializing_if = "RemoteSamplingCapability::is_default")]
    pub sampling: RemoteSamplingCapability,
}

impl RemoteModelCapability {
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_none()
            && self.cache_control.is_none()
            && self.stream_termination.is_none()
            && self.sampling.is_default()
    }
}

/// Mirror of the core `SamplingCapability`: whether the model accepts a
/// caller-set temperature at all, or pins its own sampling and rejects one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSamplingCapability {
    #[default]
    Configurable,
    Pinned,
}

impl RemoteSamplingCapability {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Mirror of the core `CacheControlDialect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCacheControlDialect {
    Anthropic,
    Gemini,
}

/// Mirror of the core `StreamTermination`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteStreamTermination {
    RequireTerminalEvidence,
    EofTolerated,
}

/// Mirror of the core `ReasoningCapability`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteReasoningCapability {
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub encoding: RemoteReasoningEncoding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable: Option<RemoteReasoningDisableEncoding>,
    #[serde(default)]
    pub mandatory: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteReasoningSelection {
    #[default]
    ProviderDefault,
    Disabled,
    Effort(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteReasoningDisableEncoding {
    Native,
    Omit,
    Effort(String),
    Budget(u32),
    ToggleFalse,
}

/// Mirror of the core `ReasoningEncoding`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteReasoningEncoding {
    #[default]
    Effort,
    Budget(BTreeMap<String, u32>),
}

impl RemoteModelIntent {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            variant: RemoteReasoningSelection::ProviderDefault,
            capability: RemoteModelCapability::default(),
            provider: None,
            metadata: HashMap::new(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteModelIntent", "model", &self.model)
    }
}

/// Closed generation-option set: only options this protocol can actually
/// deliver to the core request are accepted. Unknown keys — including the
/// removed `top_p`, `stop` and `provider_options` — fail deserialization
/// rather than being silently discarded on the way in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteGenerationOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_cap: Option<u64>,
    /// Sampling temperature, carried as a JSON number — the same JSON-number
    /// encoding the core `NonNegativeFiniteF64` uses, and carried through the
    /// conversion unchanged, so a number survives the round trip exactly as
    /// the sender spelled it — negative zero is the single exception, and
    /// normalizes to zero. Validated on the way in: finite, non-negative,
    /// and exactly representable as a binary64 float (integers up to 2^53).
    /// `serde_json::Number` also keeps this type `Eq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
}

impl RemoteGenerationOptions {
    pub fn is_empty(&self) -> bool {
        self.output_token_cap.is_none()
            && self.temperature.is_none()
            && self.seed.is_none()
            && self.stop_sequences.is_empty()
    }

    pub(crate) fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if self.output_token_cap == Some(0) {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "generation.output_token_cap must be greater than zero".to_string(),
            });
        }
        if let Some(temperature) = &self.temperature {
            let finite_and_non_negative = temperature.as_f64().is_some_and(|value| value >= 0.0);
            // The core sampling number binds the same rule: an integer above
            // 2^53 has no exact binary64 form, so it cannot cross the boundary
            // unchanged. Hosts that only validate reject exactly what hosts
            // that convert reject.
            let exactly_representable = !temperature
                .as_u64()
                .is_some_and(|integer| integer > (1_u64 << 53));
            if !(finite_and_non_negative && exactly_representable) {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: format!(
                        "generation.temperature must be a finite, non-negative number that binary64 represents exactly, got {temperature}"
                    ),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLlmRequestScope {
    pub session_id: String,
    pub agent_frame_id: String,
    pub request_id: String,
}

impl RemoteLlmRequestScope {
    pub fn new(
        session_id: impl Into<String>,
        agent_frame_id: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: agent_frame_id.into(),
            request_id: request_id.into(),
        }
    }

    fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteLlmRequestScope", "session_id", &self.session_id)?;
        require_non_empty(
            "RemoteLlmRequestScope",
            "agent_frame_id",
            &self.agent_frame_id,
        )?;
        require_non_empty("RemoteLlmRequestScope", "request_id", &self.request_id)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteLlmRole {
    #[default]
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLlmMessage {
    pub role: RemoteLlmRole,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<RemoteLlmContentBlock>,
}

impl RemoteLlmMessage {
    fn validate(&self, index: usize) -> Result<(), RemoteProtocolError> {
        if self.content.is_empty() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteLlmMessage",
                message: format!("message at index {index} must contain at least one block"),
            });
        }
        for block in &self.content {
            block.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteLlmContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_meta: Option<RemoteResponseTextMeta>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cache_breakpoint: bool,
    },
    Attachment {
        attachment_index: usize,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        input_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<RemoteProviderReplayMeta>,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<RemoteProviderReasoningReplay>,
    },
}

impl RemoteLlmContentBlock {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        match self {
            Self::ToolCall {
                call_id,
                tool_name,
                replay,
                ..
            } => {
                require_non_empty("RemoteLlmContentBlock::ToolCall", "call_id", call_id)?;
                require_non_empty("RemoteLlmContentBlock::ToolCall", "tool_name", tool_name)?;
                if let Some(origin) = replay.as_ref().and_then(|replay| replay.origin.as_ref()) {
                    origin.validate("RemoteProviderReplayMeta.origin")?;
                }
                Ok(())
            }
            Self::ToolResult { call_id, .. } => {
                require_non_empty("RemoteLlmContentBlock::ToolResult", "call_id", call_id)
            }
            Self::Text { response_meta, .. } => {
                if let Some(origin) = response_meta
                    .as_ref()
                    .and_then(|metadata| metadata.origin.as_ref())
                {
                    origin.validate("RemoteResponseTextMeta.origin")?;
                }
                Ok(())
            }
            Self::Reasoning { replay, .. } => {
                if let Some(origin) = replay.as_ref().and_then(|replay| replay.origin.as_ref()) {
                    origin.validate("RemoteProviderReasoningReplay.origin")?;
                }
                Ok(())
            }
            Self::Attachment { .. } => Ok(()),
        }
    }
}

/// Exact configured LLM Provider route that owns opaque replay state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProviderRouteIdentity {
    pub provider: String,
    /// Stable routing metadata, not a secret-bearing URL. LLM Provider
    /// endpoints reject URL userinfo; paths and query strings remain visible
    /// in remote envelopes and traces and therefore must not carry secrets.
    pub endpoint: String,
    pub model: String,
}

impl RemoteProviderRouteIdentity {
    fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "provider", &self.provider)?;
        require_non_empty(type_name, "endpoint", &self.endpoint)?;
        require_non_empty(type_name, "model", &self.model)?;
        if let Some((_, remainder)) = self.endpoint.split_once("://") {
            let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
            if remainder[..authority_end].contains('@') {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message:
                        "LLM Provider endpoint must not contain userinfo; configure credentials separately"
                            .to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteResponseTextMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_payload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RemoteProviderRouteIdentity>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProviderReplayMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<String>,
    /// Exact LLM Provider route that minted the opaque replay state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RemoteProviderRouteIdentity>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProviderReasoningReplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<String>,
    /// Exact LLM Provider route that minted the reasoning replay state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RemoteProviderRouteIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteAttachmentSource {
    Inline {
        media_type: String,
        data_base64: String,
    },
    Stored {
        attachment_ref: RemoteAttachmentRef,
    },
    ExternalUrl {
        media_type: String,
        url: String,
    },
    ProviderFile {
        provider_scope: RemoteProviderFileScope,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
}

impl RemoteAttachmentSource {
    pub(crate) fn validate(&self, _index: usize) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Inline {
                media_type,
                data_base64,
            } => {
                require_non_empty("RemoteAttachmentSource::Inline", "media_type", media_type)?;
                validate_media_type("RemoteAttachmentSource::Inline", media_type)?;
                require_non_empty("RemoteAttachmentSource::Inline", "data_base64", data_base64)
            }
            Self::Stored { attachment_ref } => attachment_ref.validate(),
            Self::ExternalUrl { media_type, url } => {
                require_non_empty(
                    "RemoteAttachmentSource::ExternalUrl",
                    "media_type",
                    media_type,
                )?;
                validate_media_type("RemoteAttachmentSource::ExternalUrl", media_type)?;
                require_non_empty("RemoteAttachmentSource::ExternalUrl", "url", url)
            }
            Self::ProviderFile {
                provider_scope,
                id,
                media_type,
            } => {
                provider_scope.validate()?;
                require_non_empty("RemoteAttachmentSource::ProviderFile", "id", id)?;
                if let Some(media_type) = media_type {
                    require_non_empty(
                        "RemoteAttachmentSource::ProviderFile",
                        "media_type",
                        media_type,
                    )?;
                    validate_media_type("RemoteAttachmentSource::ProviderFile", media_type)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProviderFileScope {
    pub provider: String,
    pub credential_scope: String,
}

impl RemoteProviderFileScope {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProviderFileScope", "provider", &self.provider)?;
        require_non_empty(
            "RemoteProviderFileScope",
            "credential_scope",
            &self.credential_scope,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteAttachmentRef {
    pub id: String,
    pub media_type: String,
    pub byte_len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_metadata: Option<RemoteAttachmentTypeMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl RemoteAttachmentRef {
    pub(crate) fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteAttachmentRef", "id", &self.id)?;
        require_non_empty("RemoteAttachmentRef", "media_type", &self.media_type)?;
        validate_media_type("RemoteAttachmentRef", &self.media_type)
    }
}

fn validate_media_type(type_name: &'static str, value: &str) -> Result<(), RemoteProtocolError> {
    let mut pieces = value.split('/');
    let type_token = pieces.next().unwrap_or_default();
    let subtype_token = pieces.next().unwrap_or_default();
    if pieces.next().is_some()
        || !is_media_type_token(type_token)
        || !is_media_type_token(subtype_token)
    {
        return Err(RemoteProtocolError::InvalidEnvelope {
            type_name,
            message: format!("media_type `{value}` must be a syntactically valid type/subtype"),
        });
    }
    Ok(())
}

fn is_media_type_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteAttachmentTypeMetadata {
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLlmToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_remote_input_schema")]
    pub input_schema: RemoteSchemaContract,
    #[serde(default)]
    pub output_schema: RemoteSchemaContract,
}

impl RemoteLlmToolSpec {
    pub(crate) fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteLlmToolSpec", "name", &self.name)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteLlmToolChoice {
    #[default]
    Auto,
    None,
    Required,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteLlmOutputSpec {
    JsonObject,
    JsonSchema {
        name: String,
        schema: RemoteSchemaContract,
        strict: bool,
    },
}

impl RemoteLlmOutputSpec {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        match self {
            Self::JsonObject => Ok(()),
            Self::JsonSchema { name, .. } => {
                require_non_empty("RemoteLlmOutputSpec::JsonSchema", "name", name)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteLlmOutputPart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_meta: Option<RemoteResponseTextMeta>,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<RemoteProviderReasoningReplay>,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        input_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<RemoteProviderReplayMeta>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteLlmTerminalReason {
    Stop,
    ToolUse,
    OutputLimit,
    ContextOverflow,
    ContentFilter,
    ProviderError,
    Cancelled,
    #[default]
    Unknown,
}

/// Wire mirror of the core `ProviderFailureKind` classification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProviderFailureKind {
    Transport,
    Timeout,
    Http,
    Stream,
    Auth,
    Validation,
    Quota,
    Unsupported,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProviderMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_summary: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, serde_json::Value>,
}

impl RemoteProviderMetadata {
    pub fn is_empty(&self) -> bool {
        self.usage.is_none()
            && self.request_body.is_none()
            && self.http_summary.is_none()
            && self.data.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteDiagnostic {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
