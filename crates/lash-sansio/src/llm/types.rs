use crate::SessionId;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::{AttachmentRef, MediaType, SchemaContract};

pub use crate::llm::capability::{
    AnthropicThinkingRetention, AttachmentAcceptanceRule, AttachmentAcceptor,
    AttachmentCapabilitySnapshot, AttachmentMimeSource, CacheControlDialect, GoogleDialect,
    InstructionRole, ModelCapability, ModelEffortValidationCategory, ModelEffortValidationError,
    OpenAiReasoningContext, ProviderReasoningRetentionSupport, ReasoningCapability,
    ReasoningDisableEncoding, ReasoningEncoding, ReasoningRetentionCapability,
    ReasoningRetentionPolicy, ReasoningRetentionSelection, ReasoningRetentionValidationCategory,
    ReasoningRetentionValidationError, ReasoningSelection, SamplingCapability, StreamTermination,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmTerminalReason {
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

impl LlmTerminalReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::ToolUse => "tool_use",
            Self::OutputLimit => "output_limit",
            Self::ContextOverflow => "context_overflow",
            Self::ContentFilter => "content_filter",
            Self::ProviderError => "provider_error",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// Classification of a provider/transport failure.
///
/// This is the single canonical failure-kind vocabulary: provider transports
/// classify failures into it (`lash-core` re-exports it from
/// `llm::transport`), the turn machine carries it on
/// [`ErrorEnvelope`](crate::session_model::ErrorEnvelope), and hosts read it
/// back from `TurnIssue`s without scraping traces.
///
/// `Unknown` is an explicit classification, never a decoder fallback.
/// Persisted failure records reject unrecognized kind literals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureKind {
    Transport,
    Timeout,
    Http,
    Stream,
    Auth,
    Validation,
    Quota,
    Unsupported,
    #[default]
    Unknown,
}

impl ProviderFailureKind {
    /// Stable snake_case code, identical to the serde wire form.
    pub fn code(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Http => "http",
            Self::Stream => "stream",
            Self::Auth => "auth",
            Self::Validation => "validation",
            Self::Quota => "quota",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// Provider replay phase tag: the two-value vocabulary the kernel interprets
/// (`"commentary"`/`"final_answer"` on the wire). An unknown spelling fails
/// decoding rather than silently meaning "neither".
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePhase {
    Commentary,
    FinalAnswer,
}

impl ResponsePhase {
    /// The provider wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }

    /// An unrecognized phase is not a phase the kernel interprets, so it decodes to `None`
    /// rather than guessing.
    pub fn from_provider_wire(phase: &str) -> Option<Self> {
        match phase {
            "commentary" => Some(Self::Commentary),
            "final_answer" => Some(Self::FinalAnswer),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResponseTextMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Provider replay phase tag; see [`ResponsePhase`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ResponsePhase>,
    /// Provider-owned payload needed to replay this text part on a future
    /// request. The kernel stores it opaquely and providers decide whether it
    /// is valid for their next wire request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_payload: Option<String>,
    /// Exact LLM Provider route that minted this provider-owned response
    /// metadata. Missing on sessions persisted before replay provenance was
    /// introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ProviderRouteIdentity>,
    /// LLM Provider kind decoded from the pre-route-identity JSON vocabulary.
    ///
    /// This is identity-compatibility material only. Because the legacy pair
    /// has no endpoint, it never certifies replay for a current route.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "origin_provider"
    )]
    pub legacy_origin_provider: Option<String>,
    /// Model decoded from the pre-route-identity JSON vocabulary. See
    /// [`ResponseTextMeta::legacy_origin_provider`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "origin_model"
    )]
    pub legacy_origin_model: Option<String>,
}

impl ResponseTextMeta {
    pub fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.status.is_none()
            && self.phase.is_none()
            && self.provider_payload.is_none()
    }

    pub fn is_final_answer_phase(&self) -> bool {
        self.phase == Some(ResponsePhase::FinalAnswer)
    }

    pub fn is_commentary_phase(&self) -> bool {
        self.phase == Some(ResponsePhase::Commentary)
    }
}

/// Stable identity of one configured LLM Provider route.
///
/// `endpoint` is either a normalized base URL or a host-supplied opaque route
/// id. Together with the LLM Provider kind and requested model it is the replay
/// contract. Model aliases behind one endpoint are intentionally out of scope:
/// hosts must use the exact model string they want included in route equality.
/// HTTP(S) endpoints reject userinfo because credentials are neither route
/// identity nor safe trace metadata. Explicit default ports remain distinct
/// from implicit ports. Scheme and host case, plus an empty path versus `/`,
/// normalize; path case and query strings remain identity-significant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ProviderRouteIdentity {
    pub provider: Box<str>,
    pub endpoint: Box<str>,
    pub model: Box<str>,
}

pub use super::provider_route::ProviderEndpointError;
pub use super::stream_senders::{LlmEventSender, LlmProviderTraceEvent, LlmProviderTraceSender};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: SchemaContract,
    pub output_schema: SchemaContract,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LlmToolChoice {
    #[default]
    Auto,
    None,
    Required,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderReplayMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<String>,
    /// Exact LLM Provider route that minted this opaque replay state. Missing
    /// on sessions persisted before replay provenance was introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ProviderRouteIdentity>,
}

impl ProviderReplayMeta {
    pub fn is_empty(&self) -> bool {
        self.item_id.is_none() && self.opaque.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderReasoningReplay {
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
    /// Exact LLM Provider route that minted this reasoning replay state.
    /// Missing on sessions persisted before replay provenance was introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ProviderRouteIdentity>,
}

impl ProviderReasoningReplay {
    pub fn is_empty(&self) -> bool {
        self.item_id.is_none()
            && self.encrypted_content.is_none()
            && self.signature.is_none()
            && !self.redacted
            && self.summary.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReplayKind {
    ResponseText,
    Reasoning,
    ToolCall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReplayDropReason {
    Unstamped,
    ForeignRoute,
}

/// Typed evidence produced when provider-owned replay state cannot safely be
/// served on the selected route. The surrounding neutral content remains in
/// the request; only the opaque provider state is removed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderReplayDrop {
    pub kind: ProviderReplayKind,
    pub reason: ProviderReplayDropReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minting_route: Option<ProviderRouteIdentity>,
    pub serving_route: ProviderRouteIdentity,
}

/// Typed contract violation returned when an LLM Provider attempts to
/// recertify replay state already stamped by another route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderReplayOriginConflict {
    pub kind: ProviderReplayKind,
    pub actual: ProviderRouteIdentity,
    pub expected: ProviderRouteIdentity,
}

impl std::fmt::Display for ProviderReplayOriginConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?} replay origin conflict: minted by {:?}, returned by {:?}",
            self.kind, self.actual, self.expected
        )
    }
}

impl std::error::Error for ProviderReplayOriginConflict {}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LlmOutputPart {
    Text {
        text: String,
        response_meta: Option<ResponseTextMeta>,
    },
    /// Model "thinking" / reasoning output from providers that expose a
    /// chain-of-thought channel.
    ///
    /// * `text` — human-readable summary for display.
    /// * `replay` — opaque provider replay state. Provider crates decide
    ///   how to map it back to their wire format on the next turn.
    Reasoning {
        text: String,
        replay: Option<ProviderReasoningReplay>,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        input_json: String,
        /// Opaque provider replay state. Core may use `item_id` for stable
        /// correlation, but provider crates own the wire semantics.
        replay: Option<ProviderReplayMeta>,
    },
}

impl LlmOutputPart {
    pub fn stamp_replay_origin(
        &mut self,
        route: &ProviderRouteIdentity,
    ) -> Result<(), ProviderReplayOriginConflict> {
        if let Some(conflict) = self.replay_origin_conflict(route) {
            return Err(conflict);
        }
        match self {
            Self::Text {
                response_meta: Some(meta),
                ..
            } if !meta.is_empty() => {
                meta.origin = Some(route.clone());
            }
            Self::Reasoning {
                replay: Some(meta), ..
            } if !meta.is_empty() => {
                meta.origin = Some(route.clone());
            }
            Self::ToolCall {
                replay: Some(meta), ..
            } if !meta.is_empty() => {
                meta.origin = Some(route.clone());
            }
            _ => {}
        }
        Ok(())
    }

    fn replay_origin_conflict(
        &self,
        expected: &ProviderRouteIdentity,
    ) -> Option<ProviderReplayOriginConflict> {
        let (kind, actual) = match self {
            Self::Text {
                response_meta: Some(meta),
                ..
            } if !meta.is_empty() => (ProviderReplayKind::ResponseText, meta.origin.as_ref()),
            Self::Reasoning {
                replay: Some(meta), ..
            } if !meta.is_empty() => (ProviderReplayKind::Reasoning, meta.origin.as_ref()),
            Self::ToolCall {
                replay: Some(meta), ..
            } if !meta.is_empty() => (ProviderReplayKind::ToolCall, meta.origin.as_ref()),
            _ => return None,
        };
        actual
            .filter(|actual| *actual != expected)
            .cloned()
            .map(|actual| ProviderReplayOriginConflict {
                kind,
                actual,
                expected: expected.clone(),
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LlmRole {
    User,
    Assistant,
    System,
}

/// A structured content block inside an `LlmMessage`. Kept close to the
/// per-provider block shapes and maps cleanly onto each wire format so the
/// adapters can emit the right shape without re-coalescing flat messages.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LlmContentBlock {
    Text {
        text: Arc<str>,
        response_meta: Option<ResponseTextMeta>,
        cache_breakpoint: bool,
    },
    /// The source travels with the block through projection and transport.
    Attachment { source: Box<AttachmentSource> },
    /// Assistant tool call with optional opaque provider replay state.
    ToolCall {
        call_id: String,
        tool_name: String,
        input_json: String,
        replay: Option<ProviderReplayMeta>,
    },
    /// User tool-result block. Some providers allow multiple per user turn;
    /// adapters that want one-per-message split as needed.
    ToolResult {
        call_id: String,
        content: String,
        /// Name of the tool that produced this result. Some provider replay
        /// formats require this; others ignore it.
        tool_name: Option<String>,
    },
    /// Chain-of-thought / reasoning block. See [`LlmOutputPart::Reasoning`]
    /// for field semantics. Adapters that don't support reasoning replay
    /// drop these blocks silently.
    Reasoning {
        text: String,
        replay: Option<ProviderReasoningReplay>,
    },
}

/// A single role turn in the LLM conversation. `blocks` holds structured
/// content that maps 1:1 onto provider wire types. The old flat
/// `content: String` + `kind` discriminator has been retired in favor of
/// this block model.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmMessage {
    pub role: LlmRole,
    /// True only for a host-confirmed genuine user input. Client-side retention
    /// may cut before this message; synthetic observations and tool results do
    /// not receive the marker even when their provider role is `user`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starts_user_segment: bool,
    pub blocks: Arc<Vec<LlmContentBlock>>,
}

impl LlmMessage {
    pub fn new(role: LlmRole, blocks: Vec<LlmContentBlock>) -> Self {
        Self {
            role,
            starts_user_segment: false,
            blocks: Arc::new(blocks),
        }
    }

    /// Convenience constructor for a single-text-block message.
    pub fn text(role: LlmRole, text: impl Into<Arc<str>>) -> Self {
        Self {
            role,
            starts_user_segment: false,
            blocks: Arc::new(vec![LlmContentBlock::Text {
                text: text.into(),
                response_meta: None,
                cache_breakpoint: false,
            }]),
        }
    }

    pub fn with_user_segment_start(mut self) -> Self {
        self.starts_user_segment = true;
        self
    }

    pub fn is_blank(&self) -> bool {
        self.blocks.iter().all(|b| match b {
            LlmContentBlock::Text { text, .. } => text.trim().is_empty(),
            _ => false,
        })
    }
}

/// Object key under which a historical tool call's raw argument text is
/// wrapped on provider replay when the text never parsed as JSON.
pub const RAW_TOOL_CALL_ARGUMENTS_KEY: &str = "_raw";

/// The one replay rule for a historical tool call's argument text: valid
/// JSON replays as the parsed value; anything else is wrapped as
/// `{ "_raw": "<verbatim text>" }`. Every provider presents a malformed call
/// identically instead of refusing the request or inventing arguments.
pub fn tool_call_input_replay_value(input_json: &str) -> serde_json::Value {
    serde_json::from_str(input_json)
        .unwrap_or_else(|_| serde_json::json!({ RAW_TOOL_CALL_ARGUMENTS_KEY: input_json }))
}

/// The same replay rule for wire fields that carry tool-call arguments as a
/// JSON string (OpenAI Chat `arguments`, Responses `function_call.arguments`):
/// valid JSON replays verbatim; malformed text replays as the `_raw`
/// wrapper's JSON encoding.
pub fn tool_call_input_replay_string(input_json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(input_json) {
        Ok(_) => input_json.to_string(),
        Err(_) => tool_call_input_replay_value(input_json).to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmRequestScope {
    /// Logical Lash session.
    pub session_id: SessionId,
    /// Durable agent frame/branch inside the session. Providers must use this
    /// when caching continuation state so frame switches do not inherit each
    /// other's provider-local response ids.
    pub agent_frame_id: String,
    /// One provider call, suitable for request correlation/idempotency.
    pub request_id: String,
}

impl LlmRequestScope {
    pub fn new(
        session_id: impl Into<SessionId>,
        agent_frame_id: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: agent_frame_id.into(),
            request_id: request_id.into(),
        }
    }

    pub fn continuation_key(&self) -> String {
        format!("{}::{}", self.session_id, self.agent_frame_id)
    }
}

/// Provider/account boundary for a provider-owned file id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFileScope {
    pub provider: String,
    pub credential_scope: String,
}

impl ProviderFileScope {
    pub fn new(provider: impl Into<String>, credential_scope: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            credential_scope: credential_scope.into(),
        }
    }
}

/// The ownership-explicit attachment source at the LLM/content seam.
///
/// Inline bytes are transient and must be normalized to `Stored` before a
/// durable effect is emitted. Borrowed sources are never fetched by Lash and
/// never enter the attachment manifest.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttachmentSource {
    Inline {
        media_type: MediaType,
        bytes: Vec<u8>,
    },
    Stored {
        attachment_ref: AttachmentRef,
    },
    ExternalUrl {
        media_type: MediaType,
        url: String,
    },
    ProviderFile {
        provider_scope: ProviderFileScope,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<MediaType>,
    },
}

// Current attachment content carrier; measured 104 B on rustc 1.97.0,
// x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<AttachmentSource>() <= 128);

impl AttachmentSource {
    pub fn inline(media_type: MediaType, bytes: Vec<u8>) -> Self {
        Self::Inline { media_type, bytes }
    }

    pub fn stored(attachment_ref: AttachmentRef) -> Self {
        Self::Stored { attachment_ref }
    }

    pub fn external_url(media_type: MediaType, url: impl Into<String>) -> Self {
        Self::ExternalUrl {
            media_type,
            url: url.into(),
        }
    }

    pub fn provider_file(
        provider_scope: ProviderFileScope,
        id: impl Into<String>,
        media_type: Option<MediaType>,
    ) -> Self {
        Self::ProviderFile {
            provider_scope,
            id: id.into(),
            media_type,
        }
    }

    pub fn media_type(&self) -> Option<&MediaType> {
        match self {
            Self::Inline { media_type, .. } | Self::ExternalUrl { media_type, .. } => {
                Some(media_type)
            }
            Self::Stored { attachment_ref } => Some(&attachment_ref.media_type),
            Self::ProviderFile { media_type, .. } => media_type.as_ref(),
        }
    }

    pub fn stored_ref(&self) -> Option<&AttachmentRef> {
        match self {
            Self::Stored { attachment_ref } => Some(attachment_ref),
            Self::Inline { .. } | Self::ExternalUrl { .. } | Self::ProviderFile { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmJsonSchema {
    pub name: String,
    pub schema: SchemaContract,
    pub strict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LlmOutputSpec {
    JsonObject,
    JsonSchema(LlmJsonSchema),
}

/// Largest integer magnitude binary64 holds exactly. Above it, consecutive
/// integers share a representation, so reading the value back as `f64` would
/// silently change it.
const MAX_EXACT_INTEGER: u64 = 1 << 53;

/// A sampling number that is safe to place in a provider request body: finite
/// (never NaN or ±infinity), never negative, and exactly representable as a
/// binary64 float.
///
/// Backed by [`serde_json::Number`] rather than `f64` for two reasons. It keeps
/// [`GenerationOptions`] — and therefore every request, durable envelope and
/// protocol type that carries it — `Eq`, which a bare `f64` would destroy. And
/// it makes the invariant structural: a value that cannot be encoded as JSON
/// can never be constructed, so no adapter has to re-check before building a
/// `json!` body.
///
/// The JSON number a caller wrote is preserved, not re-spelled: an integer `1`
/// decoded from a wire payload stays the integer `1` and re-encodes as `1`, so
/// a protocol round trip is exact in both directions. The only representations
/// refused are the ones binary64 cannot hold — integers above 2<sup>53</sup> —
/// which keeps [`get`](Self::get) exact for every value that can exist.
/// Equality is numeric rather than textual: `1` and `1.0` are the same sampling
/// number.
///
/// Endpoint- and model-specific upper bounds (OpenAI caps temperature at 2,
/// Anthropic at 1) stay in the adapters, which are the only layer that knows
/// them.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "serde_json::Number", into = "serde_json::Number")]
pub struct NonNegativeFiniteF64(serde_json::Number);

impl NonNegativeFiniteF64 {
    pub fn new(value: f64) -> Result<Self, NonNegativeFiniteF64Error> {
        Self::check_finite_non_negative(value)?;
        // `-0.0` passes the sign check but would serialize as `-0.0`, which
        // reads as a negative number on the wire. Normalize it away.
        let value = if value == 0.0 { 0.0 } else { value };
        serde_json::Number::from_f64(value)
            .map(Self)
            .ok_or_else(|| NonNegativeFiniteF64Error {
                message: format!("{value} is not representable as a JSON number"),
            })
    }

    fn check_finite_non_negative(value: f64) -> Result<(), NonNegativeFiniteF64Error> {
        if !value.is_finite() {
            return Err(NonNegativeFiniteF64Error {
                message: format!("expected a finite number, got {value}"),
            });
        }
        // `-0.0 < 0.0` is false, so negative zero is accepted here and
        // normalized by the caller rather than rejected.
        if value < 0.0 {
            return Err(NonNegativeFiniteF64Error {
                message: format!("expected a non-negative number, got {value}"),
            });
        }
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "every constructor runs `validate`, which refuses anything but a finite JSON number"
    )]
    pub fn get(&self) -> f64 {
        self.0
            .as_f64()
            .expect("NonNegativeFiniteF64 only ever holds a finite JSON number")
    }
}

/// Numeric, not textual: two sampling numbers of the same value are equal even
/// when they were written with different JSON spellings.
impl PartialEq for NonNegativeFiniteF64 {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

/// Total because the invariant excludes NaN.
impl Eq for NonNegativeFiniteF64 {}

impl std::fmt::Display for NonNegativeFiniteF64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<NonNegativeFiniteF64> for serde_json::Number {
    fn from(value: NonNegativeFiniteF64) -> Self {
        value.0
    }
}

impl TryFrom<serde_json::Number> for NonNegativeFiniteF64 {
    type Error = NonNegativeFiniteF64Error;

    /// The accepted `Number` is carried through unchanged, so no decode re-spells or rounds
    /// what the sender wrote.
    fn try_from(value: serde_json::Number) -> Result<Self, Self::Error> {
        let as_f64 = value.as_f64().ok_or_else(|| NonNegativeFiniteF64Error {
            message: format!("{value} is not representable as a finite number"),
        })?;
        Self::check_finite_non_negative(as_f64)?;
        if value
            .as_u64()
            .is_some_and(|integer| integer > MAX_EXACT_INTEGER)
        {
            return Err(NonNegativeFiniteF64Error {
                message: format!("{value} cannot be represented exactly as a finite number"),
            });
        }
        if as_f64 == 0.0 && as_f64.is_sign_negative() {
            return Self::new(0.0);
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonNegativeFiniteF64Error {
    pub message: String,
}

impl std::fmt::Display for NonNegativeFiniteF64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NonNegativeFiniteF64Error {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerationProjectionProvenance {
    stop_sequences_suppressed_by_protocol: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_cap: Option<NonZeroUsize>,
    /// Sampling temperature. Adapters emit it on wires that accept one and
    /// omit it on wires that do not; `None` leaves the endpoint default in
    /// place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<NonNegativeFiniteF64>,
    /// Sampling seed, a best-effort repeatability request. Carried by the
    /// OpenAI-compatible Chat Completions dialect and Google's
    /// `generationConfig`; wires without a seed field omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Literal sequences that terminate generation. Adapters emit these only
    /// on provider wires with a native stop-sequence field; streaming callers
    /// must still be prepared to stop locally when a provider cannot. A
    /// protocol whose grammar owns the response boundary may suppress this
    /// entire list; the resulting disposition is `SuppressedProtocolOwned`,
    /// not `Applied`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// In-process projection provenance. This is not caller generation intent
    /// and does not cross persistence or remote request boundaries.
    #[serde(skip)]
    pub projection_provenance: GenerationProjectionProvenance,
}

impl GenerationOptions {
    pub fn output_token_cap_u64(&self) -> Option<u64> {
        self.output_token_cap
            .map(NonZeroUsize::get)
            .map(|value| value as u64)
    }

    /// Suppress caller stop sequences because the protocol grammar owns the
    /// response boundary, retaining whether caller intent was displaced for
    /// disposition reporting.
    pub fn suppress_stop_sequences_for_protocol(&mut self) {
        self.projection_provenance
            .stop_sequences_suppressed_by_protocol = !self.stop_sequences.is_empty();
        self.stop_sequences.clear();
    }

    pub fn stop_sequences_suppressed_by_protocol(&self) -> bool {
        self.projection_provenance
            .stop_sequences_suppressed_by_protocol
    }

    /// Layer these options over `base`: an option this value sets wins, an
    /// option it leaves unset keeps the base's.
    ///
    /// Every option is independently optional, so replacing the struct
    /// wholesale would drop intent the caller never spoke about. This is the
    /// same per-field layering `resolve_generation_policy` already applies
    /// between a request and provider configuration.
    pub fn merged_over(&self, base: &Self) -> Self {
        Self {
            output_token_cap: self.output_token_cap.or(base.output_token_cap),
            temperature: self
                .temperature
                .clone()
                .or_else(|| base.temperature.clone()),
            seed: self.seed.or(base.seed),
            stop_sequences: if self.stop_sequences.is_empty() {
                base.stop_sequences.clone()
            } else {
                self.stop_sequences.clone()
            },
            projection_provenance: GenerationProjectionProvenance::default(),
        }
    }
}

/// What became of one caller-requested generation option on one request.
///
/// Adapters emit what their wire can express and omit the rest — a model that
/// pins sampling, extended thinking, or an endpoint with no seed field all
/// take an option away without failing the call. This names which happened so
/// a host can tell an honored request from a silently dropped one.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    Hash,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GenerationOptionOutcome {
    /// The caller expressed no preference, so there was nothing to apply.
    #[default]
    NotRequested,
    /// The caller asked for it and the request carries it.
    Applied,
    /// A protocol suppressed the caller's non-empty stop list because its
    /// grammar owns the response boundary. This is intentional protocol
    /// ownership, but the caller's requested value did not reach the wire.
    SuppressedProtocolOwned,
    /// The caller asked for it and it is not expressible here: the endpoint
    /// has no field for it, or this adapter's use of the endpoint does not
    /// send one. Codex declines request controls by policy even when its
    /// underlying dialect has related fields.
    OmittedUnsupported,
    /// The caller asked for it and sampling is pinned for this request, by the
    /// model's declared capability or by the thinking configuration in use.
    OmittedSamplingPinned,
    /// The caller asked for an output-token cap above what this model can
    /// produce, and the request carries the model's capacity instead. The
    /// bound the caller asked for still holds; the number on the wire is
    /// smaller than the one they named.
    ClampedToCapacity,
}

impl GenerationOptionOutcome {
    pub fn applied(requested: bool) -> Self {
        if requested {
            Self::Applied
        } else {
            Self::NotRequested
        }
    }

    /// Report an option this request cannot express — no field for it on the
    /// endpoint, or none this adapter sends.
    pub fn unsupported(requested: bool) -> Self {
        if requested {
            Self::OmittedUnsupported
        } else {
            Self::NotRequested
        }
    }

    /// Report an option dropped because sampling is pinned for this request.
    pub fn sampling_pinned(requested: bool) -> Self {
        if requested {
            Self::OmittedSamplingPinned
        } else {
            Self::NotRequested
        }
    }

    /// Whether a requested option was dropped rather than sent.
    pub fn is_omitted(self) -> bool {
        matches!(
            self,
            Self::SuppressedProtocolOwned | Self::OmittedUnsupported | Self::OmittedSamplingPinned
        )
    }

    /// Whether the request carries exactly what the caller asked for, if
    /// anything. False for a dropped option and for a clamped one.
    pub fn is_honored(self) -> bool {
        matches!(self, Self::NotRequested | Self::Applied)
    }
}

/// Adapter-reported fate of a request's generation and prompt-cache intent.
///
/// This is request-side, adapter-owned bookkeeping and deliberately separate
/// from [`ExecutionEvidence`], which carries only facts the provider reported
/// about the execution. A host that needs repeatability asserts
/// [`nothing_omitted`](Self::nothing_omitted) rather than trusting that a
/// session-wide temperature survived every model it ran against.
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
pub struct GenerationReceipt {
    #[serde(default)]
    pub output_token_cap: GenerationOptionOutcome,
    #[serde(default)]
    pub temperature: GenerationOptionOutcome,
    #[serde(default)]
    pub seed: GenerationOptionOutcome,
    #[serde(default)]
    pub stop_sequences: GenerationOptionOutcome,
    /// Fate of explicit prompt-cache breakpoints in the request.
    #[serde(default)]
    pub cache: GenerationOptionOutcome,
}

impl GenerationReceipt {
    /// Every requested control reached the wire, though an
    /// output-token cap may have reached it reduced to the model's capacity.
    /// Use [`fully_honored`](Self::fully_honored) to reject that too.
    pub fn nothing_omitted(&self) -> bool {
        !self.output_token_cap.is_omitted()
            && !self.temperature.is_omitted()
            && !self.seed.is_omitted()
            && !self.stop_sequences.is_omitted()
            && !self.cache.is_omitted()
    }

    /// Every requested control reached the wire unchanged.
    pub fn fully_honored(&self) -> bool {
        self.output_token_cap.is_honored()
            && self.temperature.is_honored()
            && self.seed.is_honored()
            && self.stop_sequences.is_honored()
            && self.cache.is_honored()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LlmRequest {
    /// Initial session instructions, separate from runtime feedback in messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Arc<str>>,
    pub model: String,
    pub messages: Vec<LlmMessage>,
    /// Request-local bytes resolved through the session guard for `Stored`
    /// sources. This materialization cache is never serialized and does not
    /// blur source ownership: adapters still inspect the original source and
    /// may only upload-cache entries whose source is `Stored`.
    #[serde(default, skip)]
    pub resolved_stored: HashMap<crate::AttachmentId, Vec<u8>>,
    pub tools: Arc<Vec<LlmToolSpec>>,
    pub tool_choice: LlmToolChoice,
    pub model_variant: crate::llm::capability::ReasoningSelection,
    #[serde(default)]
    pub model_capability: crate::llm::capability::ModelCapability,
    #[serde(default)]
    pub generation: GenerationOptions,
    pub scope: LlmRequestScope,
    pub output_spec: Option<LlmOutputSpec>,
    #[serde(default, skip)]
    pub stream_events: Option<LlmEventSender>,
    #[serde(default, skip)]
    pub provider_trace: Option<LlmProviderTraceSender>,
}

impl LlmRequest {
    /// Attachment sources in message order, derived from their owning blocks.
    pub fn attachments(&self) -> Vec<&AttachmentSource> {
        self.messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Attachment { source } => Some(source.as_ref()),
                _ => None,
            })
            .collect()
    }

    /// Remove opaque replay state that was not minted by the exact LLM
    /// Provider route serving this request.
    ///
    /// The immutable scan preserves shared prompt block allocations on the
    /// no-drop path. Only messages containing foreign or unstamped replay are
    /// copied. Reasoning keeps non-empty neutral text (falling back to its
    /// non-empty summary), tool-call content remains, and empty reasoning is
    /// removed instead of manufacturing an empty text block.
    pub fn drop_foreign_replay(
        &mut self,
        serving_route: &ProviderRouteIdentity,
    ) -> Vec<ProviderReplayDrop> {
        let mut drops = Vec::new();
        for message in &mut self.messages {
            if !message
                .blocks
                .iter()
                .any(|block| replay_drop_for_block(block, serving_route).is_some())
            {
                continue;
            }
            Arc::make_mut(&mut message.blocks).retain_mut(|block| {
                match block {
                    LlmContentBlock::Text { response_meta, .. } => {
                        let Some(meta) = response_meta.as_ref() else {
                            return true;
                        };
                        let Some(reason) = replay_drop_reason(meta.origin.as_ref(), serving_route)
                        else {
                            return true;
                        };
                        if meta.is_empty() {
                            return true;
                        }
                        drops.push(ProviderReplayDrop {
                            kind: ProviderReplayKind::ResponseText,
                            reason,
                            minting_route: meta.origin.clone(),
                            serving_route: serving_route.clone(),
                        });
                        *response_meta = None;
                    }
                    LlmContentBlock::Reasoning { text, replay } => {
                        let Some(meta) = replay.as_ref() else {
                            return true;
                        };
                        if meta.is_empty() {
                            return true;
                        }
                        let reason = replay_drop_reason(meta.origin.as_ref(), serving_route);
                        if let Some(reason) = reason {
                            let replay_summary = meta.summary.join("\n\n");
                            drops.push(ProviderReplayDrop {
                                kind: ProviderReplayKind::Reasoning,
                                reason,
                                minting_route: meta.origin.clone(),
                                serving_route: serving_route.clone(),
                            });
                            let mut neutral_text = std::mem::take(text);
                            if neutral_text.is_empty() {
                                neutral_text = replay_summary;
                            }
                            if neutral_text.is_empty() {
                                return false;
                            }
                            *block = LlmContentBlock::Text {
                                text: neutral_text.into(),
                                response_meta: None,
                                cache_breakpoint: false,
                            };
                        }
                    }
                    LlmContentBlock::ToolCall { replay, .. } => {
                        let Some(meta) = replay.as_ref() else {
                            return true;
                        };
                        if meta.is_empty() {
                            return true;
                        }
                        let reason = replay_drop_reason(meta.origin.as_ref(), serving_route);
                        if let Some(reason) = reason {
                            drops.push(ProviderReplayDrop {
                                kind: ProviderReplayKind::ToolCall,
                                reason,
                                minting_route: meta.origin.clone(),
                                serving_route: serving_route.clone(),
                            });
                            *replay = None;
                        }
                    }
                    LlmContentBlock::Attachment { .. } | LlmContentBlock::ToolResult { .. } => {}
                }
                true
            });
        }
        drops
    }

    /// Validate and apply the selected reasoning-history policy before a
    /// provider serializes the request. Native policies leave the HTTP history
    /// intact; only the explicit client-side fallback removes whole genuine
    /// user segments.
    pub fn reasoning_retention_safe_for<'a>(
        &'a self,
        serving_route: &ProviderRouteIdentity,
        provider_kind: &str,
        adapter_support: ProviderReasoningRetentionSupport,
    ) -> Result<std::borrow::Cow<'a, Self>, ReasoningRetentionValidationError> {
        self.model_capability.validate_reasoning_retention(
            &self.model,
            provider_kind,
            adapter_support,
        )?;

        let cutoff = match self.model_capability.reasoning_retention.selection {
            ReasoningRetentionSelection::ClientSideUserSegments { max_segments } => {
                let starts = self
                    .messages
                    .iter()
                    .enumerate()
                    .filter_map(|(index, message)| message.starts_user_segment.then_some(index))
                    .collect::<Vec<_>>();
                if starts.len() > max_segments.get() {
                    starts[starts.len() - max_segments.get()]
                } else {
                    0
                }
            }
            ReasoningRetentionSelection::ProviderDefault
            | ReasoningRetentionSelection::OpenAiContext { .. }
            | ReasoningRetentionSelection::AnthropicClearThinking { .. } => 0,
        };

        for (index, message) in self.messages.iter().enumerate() {
            if message.starts_user_segment
                && (message.role != LlmRole::User
                    || message
                        .blocks
                        .iter()
                        .any(|block| matches!(block, LlmContentBlock::ToolResult { .. })))
            {
                return Err(ReasoningRetentionValidationError {
                    category: ReasoningRetentionValidationCategory::InvalidHistory,
                    message: format!(
                        "LLM message {index} has an invalid genuine-user-segment marker."
                    ),
                });
            }
        }

        if cutoff != 0 {
            let mut retained_calls = std::collections::HashSet::new();
            for message in &self.messages[cutoff..] {
                for block in message.blocks.iter() {
                    match block {
                        LlmContentBlock::ToolCall { call_id, .. } => {
                            retained_calls.insert(call_id.as_str());
                        }
                        LlmContentBlock::ToolResult { call_id, .. }
                            if !retained_calls.contains(call_id.as_str()) =>
                        {
                            return Err(ReasoningRetentionValidationError {
                                category: ReasoningRetentionValidationCategory::InvalidHistory,
                                message: format!(
                                    "Client-side retention would orphan tool result `{call_id}`."
                                ),
                            });
                        }
                        LlmContentBlock::Text { .. }
                        | LlmContentBlock::Attachment { .. }
                        | LlmContentBlock::Reasoning { .. }
                        | LlmContentBlock::ToolResult { .. } => {}
                    }
                }
            }
        }

        let replay_drop_needed = self.messages.iter().any(|message| {
            message
                .blocks
                .iter()
                .any(|block| replay_drop_for_block(block, serving_route).is_some())
        });
        if cutoff == 0 && !replay_drop_needed {
            return Ok(std::borrow::Cow::Borrowed(self));
        }

        let mut safe = self.clone();
        if cutoff != 0 {
            safe.messages.drain(..cutoff);
        }
        safe.drop_foreign_replay(serving_route);
        Ok(std::borrow::Cow::Owned(safe))
    }

    pub fn attachment_bytes<'a>(&'a self, source: &'a AttachmentSource) -> Option<&'a [u8]> {
        match source {
            AttachmentSource::Inline { bytes, .. } => Some(bytes),
            AttachmentSource::Stored { attachment_ref } => self
                .resolved_stored
                .get(&attachment_ref.id)
                .map(Vec::as_slice),
            AttachmentSource::ExternalUrl { .. } | AttachmentSource::ProviderFile { .. } => None,
        }
    }

    pub fn session_id(&self) -> &str {
        self.scope.session_id.as_str()
    }

    pub fn agent_frame_id(&self) -> &str {
        self.scope.agent_frame_id.as_str()
    }

    pub fn request_id(&self) -> &str {
        self.scope.request_id.as_str()
    }

    pub fn continuation_key(&self) -> String {
        self.scope.continuation_key()
    }
}

fn replay_drop_for_block(
    block: &LlmContentBlock,
    serving_route: &ProviderRouteIdentity,
) -> Option<ProviderReplayDropReason> {
    match block {
        LlmContentBlock::Text {
            response_meta: Some(meta),
            ..
        } if !meta.is_empty() => replay_drop_reason(meta.origin.as_ref(), serving_route),
        LlmContentBlock::Reasoning {
            replay: Some(meta), ..
        } if !meta.is_empty() => replay_drop_reason(meta.origin.as_ref(), serving_route),
        LlmContentBlock::ToolCall {
            replay: Some(meta), ..
        } if !meta.is_empty() => replay_drop_reason(meta.origin.as_ref(), serving_route),
        _ => None,
    }
}

fn replay_drop_reason(
    origin: Option<&ProviderRouteIdentity>,
    serving_route: &ProviderRouteIdentity,
) -> Option<ProviderReplayDropReason> {
    match origin {
        Some(origin) if origin == serving_route => None,
        Some(_) => Some(ProviderReplayDropReason::ForeignRoute),
        None => Some(ProviderReplayDropReason::Unstamped),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

impl LlmUsage {
    pub fn total(&self) -> i64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_write_input_tokens
    }

    pub fn input_total(&self) -> i64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_write_input_tokens
    }
}

/// Empty metadata objects and non-numeric labels are not usage evidence, while an explicit
/// numeric zero is.
pub fn provider_usage_has_quantities(usage: &serde_json::Value) -> bool {
    match usage {
        serde_json::Value::Number(_) => true,
        serde_json::Value::Array(values) => values.iter().any(provider_usage_has_quantities),
        serde_json::Value::Object(fields) => fields.values().any(provider_usage_has_quantities),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {
            false
        }
    }
}

/// Which render lane a streamed output block belongs to.
///
/// A *block* is the unit a host renders: one OpenAI reasoning summary part,
/// one Anthropic `thinking`/`text` content block, or one ordinal run of
/// thought/text for providers with no native notion. Blocks name their owning
/// reasoning item or message item when the provider has one; several blocks
/// may share one item's replay material.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StreamBlockKind {
    /// Visible assistant prose.
    AssistantText,
    /// Reasoning summary ("thinking") text, kept separate from assistant
    /// response text.
    Reasoning,
}

/// Provider-minted identity of one streamed output block.
///
/// Minted by the provider adapter from provider facts: OpenAI
/// `(item_id, summary_index)`, Anthropic content-block index, or a
/// deterministic per-attempt ordinal for providers with no native notion.
/// The id is opaque to hosts; ordering comes from `ordinal`, never from
/// parsing `id`. Live and replayed streams emit identical identities.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct StreamBlockIdentity {
    /// Opaque provider-minted block key, stable within one response attempt.
    pub id: String,
    /// Per-attempt block ordinal assigned by the provider adapter.
    pub ordinal: u64,
    /// The reasoning or message item this block belongs to, when the provider
    /// has one. N blocks may share one item's replay material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

impl StreamBlockIdentity {
    pub fn new(id: impl Into<String>, ordinal: u64) -> Self {
        Self {
            id: id.into(),
            ordinal,
            item_id: None,
        }
    }

    pub fn with_item_id(mut self, item_id: Option<String>) -> Self {
        self.item_id = item_id.filter(|id| !id.is_empty());
        self
    }
}

#[derive(Clone, Debug)]
pub enum LlmStreamEvent {
    /// A retry is starting from the original request. Consumers must discard
    /// attempt-local accumulated parts, blocks, and usage before accepting
    /// new events.
    AttemptReset,
    /// A new assistant-text block opened in this attempt.
    TextBlockStart {
        block: StreamBlockIdentity,
    },
    /// Providers must send only the new suffix here; completed/cumulative message text belongs
    /// in `Part(Text)`.
    Delta {
        block: StreamBlockIdentity,
        text: String,
    },
    /// `block` closed. `text` is the block's authoritative text, so a consumer
    /// can correct drift from accumulated deltas.
    TextBlockEnd {
        block: StreamBlockIdentity,
        text: String,
    },
    /// A new reasoning block opened in this attempt.
    ReasoningBlockStart {
        block: StreamBlockIdentity,
    },
    /// Incremental reasoning-summary text within `block`, kept separate from
    /// assistant response text in [`Self::Delta`].
    ReasoningDelta {
        block: StreamBlockIdentity,
        text: String,
    },
    /// `block` closed. `text` is the block's authoritative text. A block with
    /// zero deltas is valid (redacted thinking, signed-empty blocks).
    ReasoningBlockEnd {
        block: StreamBlockIdentity,
        text: String,
    },
    /// Structured provider output state. Text parts reconcile final response
    /// state and replay metadata; they are not live-visible text deltas.
    Part(LlmOutputPart),
    Usage(LlmUsage),
    /// Adapter-owned request and provider evidence observed while the stream
    /// is live. The runtime retains the latest fields so a protocol abort can
    /// journal what was known before preemption.
    Evidence(LlmStreamEvidence),
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
}

#[derive(Clone, Debug, Default)]
pub struct LlmStreamEvidence {
    /// The provider transport has established a response for this attempt.
    ///
    /// This is attempt-local protocol state, independent of optional request
    /// diagnostics and response metadata. Once observed, merging later
    /// evidence cannot clear it.
    pub response_started: bool,
    pub provider_usage: Option<serde_json::Value>,
    pub request_body: Option<String>,
    pub http_summary: Option<String>,
    pub execution_evidence: Option<ExecutionEvidence>,
    pub generation_disposition: Option<GenerationReceipt>,
    /// Allowlisted response metadata available when this evidence event was
    /// emitted. Shipped HTTP adapters publish captured response headers once
    /// response establishment succeeds.
    pub response_metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

impl LlmStreamEvidence {
    pub fn merge(&mut self, next: Self) -> Result<(), ExecutionEvidenceMergeError> {
        let response_started = self.response_started || next.response_started;
        if next.execution_evidence.is_some() && !response_started {
            return Err(ExecutionEvidenceMergeError::BeforeResponseStart);
        }
        let mut execution_evidence = self.execution_evidence.clone();
        ExecutionEvidence::merge_optional(&mut execution_evidence, next.execution_evidence)?;
        self.response_started = response_started;
        if next.provider_usage.is_some() {
            self.provider_usage = next.provider_usage;
        }
        if next.request_body.is_some() {
            self.request_body = next.request_body;
        }
        if next.http_summary.is_some() {
            self.http_summary = next.http_summary;
        }
        self.execution_evidence = execution_evidence;
        if next.generation_disposition.is_some() {
            self.generation_disposition = next.generation_disposition;
        }
        self.response_metadata.extend(next.response_metadata);
        Ok(())
    }
}

/// Facts reported by the provider about the execution that produced a response.
///
/// These fields must never be filled from request intent. In particular,
/// `reasoning_output_tokens: Some(0)` is distinct from an unreported value.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutionEvidence {
    #[serde(default)]
    pub served_model: Option<String>,
    #[serde(default)]
    pub provider_response_id: Option<String>,
    /// Transport request identifier reported by the provider (for example,
    /// OpenRouter's `x-request-id`). This is distinct from the response's
    /// protocol-level identifier and may be present on failed attempts.
    #[serde(default)]
    pub provider_request_id: Option<String>,
    #[serde(default)]
    pub reasoning_output_tokens: Option<u64>,
    /// Provider-reported terminal reason in that provider's own vocabulary.
    /// Compatible gateways prefer their native reason over a normalized
    /// OpenAI-compatible `finish_reason` when both are present.
    #[serde(default)]
    pub provider_finish_reason: Option<String>,
    /// Why provider evidence is partial even though the Lash response is
    /// accepted. Present only when Lash deliberately preempted collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_interruption: Option<ExecutionEvidenceCollectionInterruption>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecutionEvidenceMergeError {
    BeforeResponseStart,
    IdentityConflict {
        field: &'static str,
        current: String,
        next: String,
    },
}

impl ExecutionEvidenceMergeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BeforeResponseStart => "stream_evidence_before_response_start",
            Self::IdentityConflict { .. } => "stream_evidence_identity_conflict",
        }
    }
}

impl std::fmt::Display for ExecutionEvidenceMergeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeResponseStart => {
                formatter.write_str("provider execution evidence preceded response establishment")
            }
            Self::IdentityConflict {
                field,
                current,
                next,
            } => write!(
                formatter,
                "provider execution evidence changed {field} from `{current}` to `{next}`"
            ),
        }
    }
}

impl std::error::Error for ExecutionEvidenceMergeError {}

impl ExecutionEvidence {
    pub fn merge_optional(
        accumulated: &mut Option<Self>,
        next: Option<Self>,
    ) -> Result<(), ExecutionEvidenceMergeError> {
        let Some(next) = next else { return Ok(()) };
        if next == Self::default() {
            return Ok(());
        }
        let mut merged = accumulated.clone().unwrap_or_default();
        merged.merge(next)?;
        *accumulated = Some(merged);
        Ok(())
    }

    pub fn merge(&mut self, next: Self) -> Result<(), ExecutionEvidenceMergeError> {
        fn merge_identity(
            current: &mut Option<String>,
            next: Option<String>,
            field: &'static str,
        ) -> Result<(), ExecutionEvidenceMergeError> {
            match (current.as_deref(), next) {
                (Some(existing), Some(next)) if existing != next => {
                    Err(ExecutionEvidenceMergeError::IdentityConflict {
                        field,
                        current: existing.to_string(),
                        next,
                    })
                }
                (None, Some(next)) => {
                    *current = Some(next);
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        let ExecutionEvidence {
            served_model,
            provider_response_id,
            provider_request_id,
            reasoning_output_tokens,
            provider_finish_reason,
            collection_interruption,
        } = next;
        let mut merged = self.clone();
        merge_identity(&mut merged.served_model, served_model, "served_model")?;
        merge_identity(
            &mut merged.provider_response_id,
            provider_response_id,
            "provider_response_id",
        )?;
        merge_identity(
            &mut merged.provider_request_id,
            provider_request_id,
            "provider_request_id",
        )?;
        merged.reasoning_output_tokens =
            match (merged.reasoning_output_tokens, reasoning_output_tokens) {
                (Some(current), Some(next)) => Some(current.max(next)),
                (current, next) => current.or(next),
            };
        merged.provider_finish_reason = provider_finish_reason.or(merged.provider_finish_reason);
        merged.collection_interruption = collection_interruption.or(merged.collection_interruption);
        *self = merged;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEvidenceCollectionInterruption {
    ProtocolAbort,
}

/// Lash-owned identity for one logical LLM call, spanning all transport
/// attempts made by the retry owner.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct LlmCallId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Completed,
    Failed,
    Aborted,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolPosition {
    NoResponse,
    ResponseObserved,
    OutputStarted,
    TerminalObserved,
}

/// A journal-safe projection of a provider/transport failure.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NormalizedError {
    pub class: String,
    /// The failure code, namespaced by who authored the spelling: `lash:` for
    /// workspace-authored codes, `provider:` for provider wire vocabulary,
    /// and a host's own namespace for host-authored codes. Rows written
    /// before the namespaced `code` field decode with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<crate::session_model::FailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<std::time::Duration>,
    /// Redacted, size-bounded diagnostic excerpt; never a raw response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetryDecision {
    pub scheduled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<std::time::Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Typed host risk-appetite decision when a retry would purchase a second
    /// generation without an idempotency or resume guarantee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_safety: Option<ChargeSafetyDecision>,
}

/// Why host charge-safety policy denied an otherwise transport-retryable
/// generation.
///
/// # Integrator class
///
/// Runtime and reporting implementors exhaustively render these host-policy
/// outcomes; provider adapters never construct them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeSafetyDenialReason {
    /// Host policy requires a provider idempotency or resume guarantee.
    GuaranteeRequired,
    /// The per-call unsafe retry allowance is exhausted.
    UnsafeRetryLimitExceeded,
    /// Provider-reported tokens exceed the host's duplicate-cost bound.
    DuplicateCostLimitExceeded,
    /// The provider's requested retry delay exceeds the host cap.
    RetryAfterExceedsCap,
}

/// Typed outcome of consulting host charge-safety policy for one prospective
/// unsafe retry.
///
/// # Integrator class
///
/// Runtime and reporting implementors consume this evidence. Host
/// applications configure `ChargeSafetyPolicy` instead of constructing an
/// outcome directly.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ChargeSafetyDecision {
    /// Host policy permits this otherwise unsafe retry.
    Authorized {
        /// Provider-reported tokens billed for the abandoned generation.
        tokens_at_stake: u64,
        /// One-based unsafe retry number within this logical LLM call.
        attempt_number: u8,
    },
    /// Host policy refuses this otherwise unsafe retry.
    Denied {
        /// Provider-reported tokens billed for the abandoned generation.
        tokens_at_stake: u64,
        /// One-based unsafe retry number within this logical LLM call.
        attempt_number: u8,
        /// Typed policy bound that refused the retry.
        reason: ChargeSafetyDenialReason,
    },
}

/// ADR 0031: absence means unreported and an explicit zero is information.
/// The disposition makes the reason for an absence part of the sealed record,
/// so a host summing cost can tell "the provider reported nothing" from "the
/// call was cut off before the provider's final usage chunk arrived" — the
/// latter is billed by the provider even though lash never saw the count.
///
/// A `Reported` disposition means provider usage was observed before the
/// attempt ended. Dialects that stream usage incrementally may have delivered
/// only an early partial; the record carries what was observed, never a guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptUsageDisposition {
    /// Provider-reported usage was observed; `usage` is `Some`. Records
    /// sealed before this disposition existed decode here.
    #[default]
    Reported,
    /// The attempt completed and the provider reported no usage at all.
    UnreportedByProvider,
    /// The attempt was aborted — a protocol-owned stream abort or an explicit
    /// cancellation — before the provider's usage arrived. The provider may
    /// still bill the generation; see `Provider::reconcile_usage`.
    UnreportedAfterAbort,
    /// The attempt failed or was interrupted before the provider's usage
    /// arrived.
    UnreportedAfterFailure,
}

impl AttemptUsageDisposition {
    /// Derive the disposition from what the attempt actually observed.
    pub fn for_attempt(outcome: AttemptOutcome, usage: Option<&LlmUsage>) -> Self {
        if usage.is_some() {
            return Self::Reported;
        }
        match outcome {
            AttemptOutcome::Completed => Self::UnreportedByProvider,
            AttemptOutcome::Aborted => Self::UnreportedAfterAbort,
            AttemptOutcome::Failed | AttemptOutcome::Interrupted => Self::UnreportedAfterFailure,
        }
    }

    /// True when the attempt ended early and its provider usage never arrived
    /// — the hole a usage ledger must show rather than silently sum as zero.
    pub fn is_unreported_after_interruption(self) -> bool {
        matches!(
            self,
            Self::UnreportedAfterAbort | Self::UnreportedAfterFailure
        )
    }

    pub fn is_reported(&self) -> bool {
        matches!(self, Self::Reported)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttemptRecord {
    pub ordinal: u32,
    /// Wall-clock epoch milliseconds read from the injected runtime clock.
    pub started_at: u64,
    pub duration: std::time::Duration,
    pub outcome: AttemptOutcome,
    pub protocol_position: ProtocolPosition,
    pub retry_budget_consumed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_decision: Option<RetryDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<NormalizedError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ExecutionEvidence>,
    /// Which of the caller's generation options this attempt's request
    /// carried, as reported by the adapter that built it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<GenerationReceipt>,
    /// Provider-reported usage only. Absence is not zero usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsage>,
    /// Why `usage` is present or absent. Additive: records sealed without it
    /// decode as [`AttemptUsageDisposition::Reported`].
    #[serde(default, skip_serializing_if = "AttemptUsageDisposition::is_reported")]
    pub usage_disposition: AttemptUsageDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCallRecord {
    pub call_id: LlmCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Semantic replay metadata removed before this call reached the wire.
    ///
    /// This is part of the sealed call record so evidence survives when
    /// provider-payload tracing is disabled and across durable runtime-effect
    /// boundaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replay_drops: Vec<ProviderReplayDrop>,
    pub attempts: Vec<AttemptRecord>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LlmResponse {
    pub parts: Vec<LlmOutputPart>,
    pub usage: LlmUsage,
    pub terminal_reason: LlmTerminalReason,
    pub terminal_diagnostic: Option<String>,
    pub provider_usage: Option<serde_json::Value>,
    pub request_body: Option<String>,
    pub http_summary: Option<String>,
    #[serde(default)]
    pub execution_evidence: Option<ExecutionEvidence>,
    /// Which of the caller's generation options the adapter put on this
    /// request's wire. `None` means the adapter does not report, which is
    /// distinct from a report that nothing was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<GenerationReceipt>,
    /// Wire observations captured by the provider driver.
    ///
    /// `header:<lowercased-name>` and `body:<json-pointer>` keys are
    /// allowlisted by host-supplied endpoint configuration and are absent
    /// unless explicitly requested. `gateway:meta` holds an OpenAI-compatible
    /// gateway's top-level `meta` block verbatim whenever the response carries
    /// one, so served-route provenance survives; lash never interprets it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub response_metadata: std::collections::BTreeMap<String, serde_json::Value>,
    /// Whether the caller asked the provider to surface reasoning. Providers
    /// stamp this from `ProviderOptions::expose_thinking`; reasoning parts
    /// stay in `parts` for multi-turn replay regardless, but the runtime only
    /// suppresses unstreamed reasoning blocks when a provider explicitly
    /// reports `Some(false)` — `None` (providers that do not stamp the flag,
    /// including third-party `Provider` impls) keeps the legacy publish
    /// behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_thinking: Option<bool>,
}

impl LlmResponse {
    /// Project the visible assistant prose from the response parts.
    ///
    /// The parts are the sole response-content authority. In particular, the
    /// projection applies provider phase visibility before concatenating text.
    pub fn full_text(&self) -> String {
        crate::turn_driver::visible_response_text_from_parts(&self.parts)
    }

    /// Stamp LLM Provider-owned replay state at the capture boundary without
    /// ever overwriting an existing, contradictory origin.
    pub fn stamp_replay_origin(
        &mut self,
        route: &ProviderRouteIdentity,
    ) -> Result<(), ProviderReplayOriginConflict> {
        if let Some(conflict) = self
            .parts
            .iter()
            .find_map(|part| part.replay_origin_conflict(route))
        {
            return Err(conflict);
        }
        for part in &mut self.parts {
            part.stamp_replay_origin(route)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ModelSelection {
    pub model: &'static str,
    pub variant: Option<&'static str>,
}

#[cfg(test)]
#[path = "types/types_contract_tests.rs"]
mod types_contract_tests;
