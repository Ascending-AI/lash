use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::{AttachmentRef, MediaType, SchemaContract};

pub use crate::llm::capability::{
    CacheControlDialect, ModelCapability, ModelEffortValidationCategory,
    ModelEffortValidationError, ReasoningCapability, ReasoningDisableEncoding, ReasoningEncoding,
    ReasoningSelection, SamplingCapability, StreamTermination,
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
/// `Unknown` doubles as the forward-compatibility catch-all: envelopes
/// persisted by a newer runtime with a kind this build does not know decode
/// as `Unknown` instead of failing.
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
    #[serde(other)]
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

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResponseTextMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Opaque provider replay phase tag. Provider crates own the wire
    /// vocabulary (e.g. OpenAI Responses `"commentary"`/`"final_answer"`);
    /// the kernel treats it as an opaque string and round-trips it verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Provider-owned payload needed to replay this text part on a future
    /// request. The kernel stores it opaquely and providers decide whether it
    /// is valid for their next wire request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_payload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_model: Option<String>,
}

impl ResponseTextMeta {
    pub fn phase_is(&self, expected: &str) -> bool {
        self.phase
            .as_deref()
            .is_some_and(|phase| phase.eq_ignore_ascii_case(expected))
    }

    pub fn is_final_answer_phase(&self) -> bool {
        self.phase_is("final_answer")
    }

    pub fn is_commentary_phase(&self) -> bool {
        self.phase_is("commentary")
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LlmRole {
    User,
    Assistant,
    System,
}

/// A structured content block inside an `LlmMessage`. Mirrors pi-mono's
/// per-provider block types and maps cleanly onto each wire format so the
/// adapters can emit the right shape without re-coalescing flat messages.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LlmContentBlock {
    Text {
        text: Arc<str>,
        response_meta: Option<ResponseTextMeta>,
        cache_breakpoint: bool,
    },
    /// Index into the enclosing `LlmRequest.attachments` vector. Provider
    /// adapters dispatch on the attachment's MIME family and source.
    Attachment { attachment_idx: usize },
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
    pub blocks: Arc<Vec<LlmContentBlock>>,
}

impl LlmMessage {
    pub fn new(role: LlmRole, blocks: Vec<LlmContentBlock>) -> Self {
        Self {
            role,
            blocks: Arc::new(blocks),
        }
    }

    /// Convenience constructor for a single-text-block message.
    pub fn text(role: LlmRole, text: impl Into<Arc<str>>) -> Self {
        Self {
            role,
            blocks: Arc::new(vec![LlmContentBlock::Text {
                text: text.into(),
                response_meta: None,
                cache_breakpoint: false,
            }]),
        }
    }

    /// True if every block is a `Text` whose content is whitespace-only.
    pub fn is_blank(&self) -> bool {
        self.blocks.iter().all(|b| match b {
            LlmContentBlock::Text { text, .. } => text.trim().is_empty(),
            _ => false,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmRequestScope {
    /// Logical Lash session.
    pub session_id: String,
    /// Durable agent frame/branch inside the session. Providers must use this
    /// when caching continuation state so frame switches do not inherit each
    /// other's provider-local response ids.
    pub agent_frame_id: String,
    /// One provider call, suitable for request correlation/idempotency.
    pub request_id: String,
}

impl LlmRequestScope {
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

    /// Validates the number in place. The accepted `Number` is carried through
    /// unchanged, so no decode re-spells or rounds what the sender wrote.
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
}

impl GenerationOptions {
    pub fn output_token_cap_u64(&self) -> Option<u64> {
        self.output_token_cap
            .map(NonZeroUsize::get)
            .map(|value| value as u64)
    }
}

/// What became of one caller-requested generation option on one request.
///
/// Adapters emit what their wire can express and omit the rest — a model that
/// pins sampling, extended thinking, or an endpoint with no seed field all
/// take an option away without failing the call. This names which happened so
/// a host can tell an honored request from a silently dropped one.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum GenerationOptionDisposition {
    /// The caller expressed no preference, so there was nothing to apply.
    #[default]
    NotRequested,
    /// The caller asked for it and the request carries it.
    Applied,
    /// The caller asked for it and this endpoint has no field for it.
    OmittedUnsupported,
    /// The caller asked for it and sampling is pinned for this request, by the
    /// model's declared capability or by the thinking configuration in use.
    OmittedSamplingPinned,
}

impl GenerationOptionDisposition {
    /// Report an option the wire carries whenever it is requested.
    pub fn applied(requested: bool) -> Self {
        if requested {
            Self::Applied
        } else {
            Self::NotRequested
        }
    }

    /// Report an option this endpoint has no field for.
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
        matches!(self, Self::OmittedUnsupported | Self::OmittedSamplingPinned)
    }
}

/// Adapter-reported fate of a request's [`GenerationOptions`].
///
/// This is request-side, adapter-owned bookkeeping and deliberately separate
/// from [`ExecutionEvidence`], which carries only facts the provider reported
/// about the execution. A host that needs repeatability asserts
/// [`nothing_omitted`](Self::nothing_omitted) rather than trusting that a
/// session-wide temperature survived every model it ran against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GenerationDisposition {
    #[serde(default)]
    pub output_token_cap: GenerationOptionDisposition,
    #[serde(default)]
    pub temperature: GenerationOptionDisposition,
    #[serde(default)]
    pub seed: GenerationOptionDisposition,
}

impl GenerationDisposition {
    /// Every option the caller asked for reached the wire.
    pub fn nothing_omitted(&self) -> bool {
        !self.output_token_cap.is_omitted()
            && !self.temperature.is_omitted()
            && !self.seed.is_omitted()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<LlmMessage>,
    pub attachments: Vec<AttachmentSource>,
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

#[derive(Clone, Debug)]
pub enum LlmStreamEvent {
    /// A retry is starting from the original request. Consumers must discard
    /// attempt-local accumulated parts and usage before accepting new events.
    AttemptReset,
    /// Append-only visible assistant text. Providers must send only the new
    /// suffix here; completed/cumulative message text belongs in `Part(Text)`.
    Delta(String),
    /// Incremental reasoning-summary text. Kept separate from `Delta` so
    /// the UI can render it in a distinct muted/italic style rather than
    /// mixing it into the assistant's final text.
    ReasoningDelta(String),
    /// Structured provider output state. Text parts reconcile final response
    /// state and replay metadata; they are not live-visible text deltas.
    Part(LlmOutputPart),
    Usage(LlmUsage),
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
}

#[derive(Clone)]
pub struct LlmEventSender(Arc<dyn Fn(LlmStreamEvent) + Send + Sync>);

impl LlmEventSender {
    pub fn new<F>(send: F) -> Self
    where
        F: Fn(LlmStreamEvent) + Send + Sync + 'static,
    {
        Self(Arc::new(send))
    }

    pub fn send(&self, event: LlmStreamEvent) {
        (self.0)(event);
    }
}

#[derive(Clone, Debug)]
pub struct LlmProviderTraceEvent {
    pub provider: &'static str,
    pub event_name: String,
    pub raw: String,
}

const PROVIDER_REQUEST_EVENT_PREFIX: &str = "\0lash.provider_request:";

impl LlmProviderTraceEvent {
    /// Construct an internal trace message for an outbound provider request.
    ///
    /// Request traces share the provider trace channel with response events,
    /// while the reserved event-name prefix lets the runtime persist them as
    /// a distinct durable trace event without wrapping or changing `raw`.
    pub fn request(provider: &'static str, endpoint: &str, body: String) -> Self {
        Self {
            provider,
            event_name: format!("{PROVIDER_REQUEST_EVENT_PREFIX}{endpoint}"),
            raw: body,
        }
    }

    pub fn request_endpoint(&self) -> Option<&str> {
        self.event_name.strip_prefix(PROVIDER_REQUEST_EVENT_PREFIX)
    }
}

#[derive(Clone)]
pub struct LlmProviderTraceSender(Arc<dyn Fn(LlmProviderTraceEvent) + Send + Sync>);

impl LlmProviderTraceSender {
    pub fn new<F>(send: F) -> Self
    where
        F: Fn(LlmProviderTraceEvent) + Send + Sync + 'static,
    {
        Self(Arc::new(send))
    }

    pub fn send(&self, event: LlmProviderTraceEvent) {
        (self.0)(event);
    }
}

impl std::fmt::Debug for LlmProviderTraceSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmProviderTraceSender")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for LlmEventSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmEventSender").finish_non_exhaustive()
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
    #[serde(default)]
    pub provider_finish_reason: Option<String>,
}

/// Lash-owned identity for one logical LLM call, spanning all transport
/// attempts made by the retry owner.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
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
    pub generation_disposition: Option<GenerationDisposition>,
    /// Provider-reported usage only. Absence is not zero usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsage>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCallRecord {
    pub call_id: LlmCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub attempts: Vec<AttemptRecord>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LlmResponse {
    pub full_text: String,
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
    pub generation_disposition: Option<GenerationDisposition>,
    /// Allowlisted wire observations captured by the provider driver
    /// (`header:<lowercased-name>` and `body:<json-pointer>` keys). Population is
    /// host-supplied endpoint configuration; empty unless explicitly requested.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub response_metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct ModelSelection {
    pub model: &'static str,
    pub variant: Option<&'static str>,
}

#[cfg(test)]
mod generation_disposition_tests {
    use super::*;

    #[test]
    fn only_requested_options_can_be_omitted() {
        // An adapter reports every option, not only the dropped ones, so a
        // wire that carries a control the caller never set is not mistaken
        // for an honored request.
        let untouched = GenerationDisposition {
            output_token_cap: GenerationOptionDisposition::applied(false),
            temperature: GenerationOptionDisposition::sampling_pinned(false),
            seed: GenerationOptionDisposition::unsupported(false),
        };
        assert_eq!(untouched, GenerationDisposition::default());
        assert!(untouched.nothing_omitted());

        let dropped = GenerationDisposition {
            output_token_cap: GenerationOptionDisposition::applied(true),
            temperature: GenerationOptionDisposition::sampling_pinned(true),
            seed: GenerationOptionDisposition::unsupported(true),
        };
        assert_eq!(
            dropped.output_token_cap,
            GenerationOptionDisposition::Applied
        );
        assert!(!dropped.output_token_cap.is_omitted());
        assert!(dropped.temperature.is_omitted());
        assert!(dropped.seed.is_omitted());
        assert!(!dropped.nothing_omitted());
        assert_eq!(
            serde_json::to_value(dropped).expect("serialize disposition"),
            serde_json::json!({
                "output_token_cap": "applied",
                "temperature": "omitted_sampling_pinned",
                "seed": "omitted_unsupported",
            })
        );
    }
}

#[cfg(test)]
mod attempt_record_tests {
    use super::*;

    #[test]
    fn attempt_contract_round_trips_closed_outcomes_and_preserves_optional_zero() {
        for (outcome, position) in [
            (
                AttemptOutcome::Completed,
                ProtocolPosition::TerminalObserved,
            ),
            (AttemptOutcome::Failed, ProtocolPosition::ResponseObserved),
            (AttemptOutcome::Aborted, ProtocolPosition::OutputStarted),
            (AttemptOutcome::Interrupted, ProtocolPosition::NoResponse),
        ] {
            let record = LlmCallRecord {
                call_id: LlmCallId("call-1".to_string()),
                label: Some("test".to_string()),
                attempts: vec![AttemptRecord {
                    ordinal: 1,
                    started_at: 42,
                    duration: std::time::Duration::from_millis(7),
                    outcome,
                    protocol_position: position,
                    retry_budget_consumed: true,
                    retry_decision: None,
                    error: None,
                    evidence: Some(ExecutionEvidence {
                        reasoning_output_tokens: Some(0),
                        ..ExecutionEvidence::default()
                    }),
                    generation_disposition: Some(GenerationDisposition {
                        output_token_cap: GenerationOptionDisposition::Applied,
                        temperature: GenerationOptionDisposition::OmittedSamplingPinned,
                        seed: GenerationOptionDisposition::OmittedUnsupported,
                    }),
                    usage: None,
                }],
            };
            let decoded: LlmCallRecord =
                serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
            assert_eq!(decoded, record);
            assert_eq!(
                decoded.attempts[0]
                    .evidence
                    .as_ref()
                    .unwrap()
                    .reasoning_output_tokens,
                Some(0)
            );
        }

        let absent = ExecutionEvidence::default();
        assert_eq!(absent.reasoning_output_tokens, None);
    }
}

#[cfg(test)]
mod attachment_source_tests {
    use super::*;

    #[test]
    fn provider_file_media_type_is_optional_and_omitted_when_absent() {
        let scope = ProviderFileScope::new("anthropic", "credential");
        let without_hint = AttachmentSource::provider_file(scope.clone(), "file-1", None);
        let without_hint_json = serde_json::to_value(&without_hint).unwrap();
        assert_eq!(
            without_hint_json,
            serde_json::json!({
                "source": "provider_file",
                "provider_scope": {
                    "provider": "anthropic",
                    "credential_scope": "credential"
                },
                "id": "file-1"
            })
        );
        assert_eq!(
            serde_json::from_value::<AttachmentSource>(without_hint_json).unwrap(),
            without_hint
        );

        let with_hint = AttachmentSource::provider_file(
            scope,
            "file-2",
            Some(MediaType::parse("image/png").unwrap()),
        );
        let with_hint_json = serde_json::to_value(&with_hint).unwrap();
        assert_eq!(with_hint_json["media_type"], "image/png");
        assert_eq!(with_hint.media_type().unwrap().as_str(), "image/png");
    }
}
