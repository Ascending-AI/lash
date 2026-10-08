pub mod failure;
pub mod message;

pub use crate::llm::types::{LlmUsage, TokenUsageOverflow};

pub use failure::{
    FailureCode, HostNamespace, InvalidNamespace, Namespace, TurnFailureCode, TurnFailureKind,
};
pub use message::{
    BaseRenderCache, InternalPartKind, InvalidPartCombination, Message, MessageRole,
    MessageSequence, Part, PartAttachment, PartKind, RenderedPrompt, append_rendered_prompt,
    messages_are_prompt_resume_safe, render_prompt, render_transcript_prompt, shared_parts,
};

use std::sync::Arc;

/// Per-turn budget: the maximum number of protocol iterations (model calls) a
/// single turn may run before it finishes with `Stopped(MaxTurns)`.
///
/// Hosts must choose a finite limit or opt into unlimited execution explicitly.
/// `Bounded` uses a non-zero value so a turn always has an opportunity to run
/// at least one iteration.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnBudget {
    Bounded(std::num::NonZeroUsize),
    Unbounded,
}

impl TurnBudget {
    /// # Panics
    ///
    /// Panics when `max_turns` is zero. In a const context, a literal zero is
    /// rejected during compilation.
    pub const fn bounded(max_turns: usize) -> Self {
        match std::num::NonZeroUsize::new(max_turns) {
            Some(max_turns) => Self::Bounded(max_turns),
            None => {
                panic!("turn budget must be non-zero; use TurnBudget::Unbounded to opt out")
            }
        }
    }

    pub fn max_turns(self) -> Option<usize> {
        match self {
            Self::Bounded(max_turns) => Some(max_turns.get()),
            Self::Unbounded => None,
        }
    }
}

/// The number of tool calls one execution may make: the host's required
/// decision, with no default and no built-in ceiling (FIG-4546).
///
/// What it counts depends on the execution. A cell — one code cell of a turn,
/// or one tool step of a protocol without cells — may make at most this many
/// tool calls in total. A process may hold at most this many at once: a call
/// counts from its acceptance until the process no longer depends on it, so
/// a long-running process is bounded in what it retains, never in what it
/// does over its life. The call that passes the limit fails with
/// [`ToolCallLimitExceeded`]; every call under it runs untouched. The limit
/// paces nothing: rate limits and throttling are the host's own.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct MaxToolCalls(std::num::NonZeroUsize);

impl MaxToolCalls {
    /// # Panics
    ///
    /// Panics when `max_tool_calls` is zero: an execution that may call no
    /// tool is expressed by its tool access, not by this limit. In a const
    /// context, a literal zero is rejected during compilation.
    pub const fn new(max_tool_calls: usize) -> Self {
        match std::num::NonZeroUsize::new(max_tool_calls) {
            Some(max_tool_calls) => Self(max_tool_calls),
            None => panic!("max_tool_calls must be non-zero"),
        }
    }

    /// The limit.
    pub const fn get(self) -> usize {
        self.0.get()
    }

    /// The limit, as the non-zero count a wire mirror carries.
    pub const fn non_zero(self) -> std::num::NonZeroUsize {
        self.0
    }
}

impl From<std::num::NonZeroUsize> for MaxToolCalls {
    fn from(max_tool_calls: std::num::NonZeroUsize) -> Self {
        Self(max_tool_calls)
    }
}

impl std::fmt::Display for MaxToolCalls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Which execution a [`MaxToolCalls`] limit refused.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallLimitScope {
    /// A cell: the limit is the total it may make.
    Cell,
    /// A process: the limit is what it may hold at once.
    Process,
}

/// A tool call refused by the session's [`MaxToolCalls`]: the program asked
/// for more tool calls than its recorded limit admits.
///
/// It is the program's failure, never the host's: the limit is recorded
/// config and the count is a fact of the program's own history, so a replay
/// or a redrive refuses the same call. Nothing of the refused calls is
/// journaled or dispatched.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ToolCallLimitExceeded {
    pub scope: ToolCallLimitScope,
    /// The recorded `max_tool_calls` the execution runs under.
    pub limit: MaxToolCalls,
    /// The calls counted against the limit before the refused ones: made so
    /// far by a cell, held now by a process.
    pub counted: usize,
    /// The calls refused together: one for a single call, an aggregate's
    /// unique tool calls for an aggregate.
    pub requested: usize,
}

impl ToolCallLimitExceeded {
    /// The stable code a refused call's tool failure carries.
    pub const CODE: &'static str = "max_tool_calls_exceeded";
}

impl std::fmt::Display for ToolCallLimitExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            scope,
            limit,
            counted,
            requested,
        } = self;
        match scope {
            ToolCallLimitScope::Cell => write!(
                formatter,
                "tool call limit exceeded (max_tool_calls = {limit}): one cell or step may make \
                 at most {limit} tool calls; this one has made {counted} and asked for \
                 {requested} more. None of the {requested} ran. Make fewer tool calls per cell \
                 or step."
            ),
            ToolCallLimitScope::Process => write!(
                formatter,
                "tool call limit exceeded (max_tool_calls = {limit}): a process may hold at \
                 most {limit} tool calls at once; this one holds {counted} and asked for \
                 {requested} more. None of the {requested} ran. Await the calls it holds \
                 before starting more."
            ),
        }
    }
}

impl std::error::Error for ToolCallLimitExceeded {}

/// Per-turn bound on *consecutive unproductive* provider attempts: model calls
/// that committed no successful execution to the turn.
///
/// [`TurnBudget`] bounds how much work a turn may do; this bounds how long a
/// turn may fail to do any. A model that answers with an unreadable cell, or
/// with a cell that only ever raises, re-enters the protocol loop without
/// leaving a committed node behind, and a turn budget large enough for real
/// work is far too large to stop that cheaply. Any attempt that commits a
/// successful execution resets the count, so ordinary repair traffic — a model
/// that mis-writes a cell and then fixes it — never approaches the bound.
///
/// The host must choose a non-zero bound or explicitly opt out.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NoProgressBudget {
    Bounded(std::num::NonZeroUsize),
    Unbounded,
}

impl NoProgressBudget {
    /// # Panics
    ///
    /// Panics when `max_attempts` is zero — a turn must always be allowed at
    /// least one attempt. In a const context, a literal zero is rejected
    /// during compilation.
    pub const fn bounded(max_attempts: usize) -> Self {
        match std::num::NonZeroUsize::new(max_attempts) {
            Some(max_attempts) => Self::Bounded(max_attempts),
            None => panic!(
                "no-progress budget must be non-zero; use NoProgressBudget::Unbounded to opt out"
            ),
        }
    }

    /// The finite bound, or `None` when the host opted out of the bound.
    pub fn max_attempts(self) -> Option<usize> {
        match self {
            Self::Bounded(max_attempts) => Some(max_attempts.get()),
            Self::Unbounded => None,
        }
    }

    pub fn is_exhausted_by(self, attempts: usize) -> bool {
        self.max_attempts()
            .is_some_and(|max_attempts| attempts >= max_attempts)
    }
}

use crate::MessageOrigin;
use crate::ToolDefinition;
use crate::TurnReply;
use crate::llm::types::LlmToolSpec;
use crate::plugin::{CheckpointKind, PluginMessage, PluginRuntimeEvent};

/// Durable protocol payload stored in session history.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProtocolEvent {
    pub plugin_id: String,
    pub payload: serde_json::Value,
}

impl ProtocolEvent {
    pub fn typed<T>(plugin_id: impl Into<String>, event: T) -> Result<Self, serde_json::Error>
    where
        T: serde::Serialize,
    {
        Ok(Self {
            plugin_id: plugin_id.into(),
            payload: serde_json::to_value(event)?,
        })
    }

    pub fn decode<T>(&self, expected_plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        if self.plugin_id != expected_plugin_id {
            return Ok(None);
        }
        serde_json::from_value(self.payload.clone()).map(Some)
    }
}

/// Typed node accepted at session-graph append boundaries.
///
/// Its semantic fields are projected by Lash's versioned append-request
/// identity encoder. Adding or changing a variant or nested semantic field
/// requires an identity encoding version bump and replacement golden corpus;
/// serde representation itself is deliberately not the identity format.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// justification: append nodes are public durable DTOs kept inline to preserve their established Rust construction API.
#[allow(clippy::large_enum_variant)]
pub enum SessionAppendNode {
    Message {
        message: PluginMessage,
    },
    ProtocolEvent {
        event: ProtocolEvent,
    },
    Plugin {
        plugin_type: String,
        #[serde(default)]
        body: serde_json::Value,
    },
}

impl SessionAppendNode {
    pub fn message(message: PluginMessage) -> Self {
        Self::Message { message }
    }

    pub fn plugin(plugin_type: impl Into<String>, body: serde_json::Value) -> Self {
        Self::Plugin {
            plugin_type: plugin_type.into(),
            body,
        }
    }

    pub fn protocol_event(event: ProtocolEvent) -> Self {
        Self::ProtocolEvent { event }
    }
}

/// Durable semantic history stored in the session graph and replayed into
/// future prompts. Unlike [`SessionStreamEvent`], these records are committed
/// state rather than transient UI/progress signals.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
// justification: the generic protocol event is caller-defined durable state and must retain the public inline history shape.
#[allow(clippy::large_enum_variant)]
pub enum SessionHistoryRecord<PE = ()> {
    Conversation(ConversationRecord),
    Protocol(PE),
}

/// Whether `existing` and `value` are the same history record, for
/// [`crate::AppendVec::push_adopting`]: a read state that appends a record
/// another holder of its buffer already appended adopts that slot.
pub fn same_history_record(
    existing: &SessionHistoryRecord<ProtocolEvent>,
    value: &SessionHistoryRecord<ProtocolEvent>,
) -> bool {
    match (existing, value) {
        (
            SessionHistoryRecord::Conversation(existing),
            SessionHistoryRecord::Conversation(value),
        ) => message::message_content_equal(existing, value),
        (SessionHistoryRecord::Protocol(existing), SessionHistoryRecord::Protocol(value)) => {
            existing == value
        }
        _ => false,
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub role: MessageRole,
    pub parts: Arc<Vec<Part>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_marker: Option<TurnReply>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AcceptedInjectedTurnInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub message: PluginMessage,
}

impl ConversationRecord {
    pub fn from_message(message: Message) -> Self {
        Self {
            id: message.id,
            role: message.role,
            parts: message.parts,
            origin: message.origin,
            reply_marker: message.reply_marker,
        }
    }

    pub fn to_message(&self) -> Message {
        Message {
            id: self.id.clone(),
            role: self.role,
            parts: Arc::clone(&self.parts),
            origin: self.origin.clone(),
            reply_marker: self.reply_marker.clone(),
        }
    }
}

/// Structured error payload carried on [`SessionStreamEvent::Error`] (and
/// [`SessionStreamEvent::RetryStatus`]).
///
/// Durability: this type appears inside persisted session snapshots and turn
/// checkpoints, so every field added after the initial shape must stay
/// additive — `#[serde(default)]` on decode and
/// `#[serde(skip_serializing_if = "Option::is_none")]` on encode — to keep
/// old snapshots decodable and new snapshots readable by older readers.
/// Transient runtime-stream signal for live consumers.
///
/// These events may be partial, duplicated, or display-only and are not proof
/// of durable session history. Persisted semantic history uses
/// [`SessionHistoryRecord`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorEnvelope {
    /// Typed origin of the failure. Serializes as the same snake_case string
    /// the field carried when it was a bare `String`, and an unrecognized
    /// spelling decodes as [`TurnFailureKind::Unknown`] rather than failing.
    pub kind: TurnFailureKind,
    /// Namespaced failure code: `lash:` codes are this workspace's
    /// [`TurnFailureCode`] vocabulary, everything else is a foreign spelling
    /// carried verbatim and never reinterpreted. A bare spelling decodes as
    /// the pre-cutover form — `lash` when it parses as a `TurnFailureCode`
    /// arm, `provider` otherwise — so old snapshots keep decoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<FailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<crate::llm::types::LlmTerminalReason>,
    pub user_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Whether the failing operation is safe to retry, when the source
    /// carries a typed signal (provider transports classify retryability).
    /// `None` means the source did not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Typed provider-failure classification, set only when the error came
    /// from an LLM provider/transport failure whose kind was classified
    /// (an unclassified `Unknown` kind is surfaced as `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_failure_kind: Option<crate::llm::types::ProviderFailureKind>,
}

/// A failure reported to hosts: the single payload both host lanes carry for
/// it (`SessionStreamEvent::Error` and the turn activity's `Error`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReportedFailure {
    pub message: String,
    /// The failure's typed kind, code, terminal reason, retryability and
    /// provider classification, when its source classified it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<ErrorEnvelope>,
}

/// A retry that is about to wait: the single payload both host lanes carry
/// for it (`SessionStreamEvent::RetryStatus` and the turn activity's
/// `RetryStatus`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RetryProgress {
    pub wait_seconds: u64,
    pub attempt: usize,
    pub max_attempts: usize,
    pub reason: String,
    /// The failure being retried, when its source classified it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<ErrorEnvelope>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
// justification: this public streaming DTO stays inline to avoid per-event allocation and preserve consumer pattern matching.
#[allow(clippy::large_enum_variant)]
pub enum SessionStreamEvent {
    /// One step of a streamed assistant-text or reasoning block: the same
    /// payload the provider stream, turn activity and the trace carry.
    /// Reasoning summary text is kept separate from assistant response text
    /// and is never fed back to the model on subsequent turns.
    #[serde(rename = "stream_block")]
    StreamBlock(crate::llm::types::StreamBlockEvent),
    #[serde(rename = "tool_call")]
    ToolCall {
        call_id: crate::ToolCallId,
        /// The model provider's id for the call, when a model issued it:
        /// correlation only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: crate::ToolCallOutput,
    },
    /// Typed accounting for host tool records beyond the bounded turn view.
    #[serde(rename = "tool_calls_omitted")]
    ToolCallsOmitted { summary: crate::OmittedToolCalls },
    #[serde(rename = "tool_call_start")]
    ToolCallStart {
        call_id: crate::ToolCallId,
        /// See [`SessionStreamEvent::ToolCall`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: serde_json::Value,
    },
    #[serde(rename = "message")]
    Message {
        text: String,
        kind: StreamMessageKind,
    },
    #[serde(rename = "llm_request")]
    LlmRequest {
        protocol_iteration: usize,
        message_count: usize,
        tool_list: String,
    },
    #[serde(rename = "llm_response")]
    LlmResponse {
        protocol_iteration: usize,
        content: String,
    },
    #[serde(rename = "token_usage")]
    LlmUsage {
        protocol_iteration: usize,
        usage: LlmUsage,
        cumulative: LlmUsage,
    },
    #[serde(rename = "retry_status")]
    RetryStatus(RetryProgress),
    #[serde(rename = "injected_turn_input_accepted")]
    InjectedTurnInputAccepted {
        inputs: Vec<AcceptedInjectedTurnInput>,
        checkpoint: CheckpointKind,
    },
    #[serde(rename = "plugin_event")]
    PluginEvent {
        plugin_id: String,
        event: PluginRuntimeEvent,
    },
    /// Semantic result for a completed turn. `Done` remains the machine
    /// lifecycle marker emitted after this event.
    #[serde(rename = "turn_outcome")]
    TurnOutcome { outcome: TurnOutcome },
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "error")]
    Error(ReportedFailure),
}

/// Discriminator of a [`SessionStreamEvent::Message`]: the closed set of
/// display-only messages Lash and its protocols put on the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum StreamMessageKind {
    /// Source of a code-mode program about to execute, in the session's
    /// dialect, shown above the tool activity it produces.
    #[serde(rename = "code")]
    Code,
}

impl StreamMessageKind {
    /// The wire spelling serde writes for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Finished(TurnFinish),
    AgentFrameSwitch {
        frame_key: crate::FrameKey,
        task: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        initial_nodes: Vec<SessionAppendNode>,
    },
    Stopped(TurnStop),
}

impl TurnOutcome {
    /// Durable cancellation evidence, present exactly when this outcome is a
    /// cancelled stop. Cancellation evidence has no other home.
    pub fn cancellation(&self) -> Option<&TurnCancellationEvidence> {
        match self {
            Self::Stopped(TurnStop::Cancelled { evidence }) => Some(evidence),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFinish {
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStop {
    /// The turn was cancelled. The evidence that settled the cancellation
    /// rides the variant, so a cancelled outcome can never be stated without
    /// saying which request produced it.
    Cancelled {
        evidence: TurnCancellationEvidence,
    },
    Incomplete,
    InvalidInput,
    MaxTurns,
    ToolFailure,
    ProviderError,
    /// The model refused the request because the assembled context exceeded
    /// the model's window. Distinct from [`TurnStop::ProviderError`] because
    /// the cause is known and recoverable: a host or plugin that observes this
    /// outcome can compact the session context and continue instead of
    /// restarting. The kernel states the outcome and chooses no policy.
    ContextOverflow,
    PluginAbort,
    RuntimeError,
    /// The durable follow-on reached its chain's configured frame-switch
    /// bound. No model call ran for this input.
    AgentFrameSwitchLimit,
    SubmittedError {
        value: serde_json::Value,
    },
    ToolError {
        tool_name: String,
        value: serde_json::Value,
    },
}

/// What cancellation does with active-turn input the cancelled turn did not
/// deliver.
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
pub enum TurnCancelUndeliveredInputPolicy {
    #[default]
    Defer,
    Drop,
}

impl TurnCancelUndeliveredInputPolicy {
    pub(crate) fn is_defer(&self) -> bool {
        *self == Self::Defer
    }
}

/// When a turn-cancel request is honoured.
///
/// `Immediate` is the compatibility default for requests and evidence decoded
/// from durable records written before this field existed: the cooperative
/// token fires as soon as the request is observed and uncommitted work
/// backtracks to the last checkpoint. `AfterStep` is honoured only at the
/// step boundary that closes a protocol iteration, after that iteration's
/// checkpoint commit; nothing in flight is interrupted and nothing backtracks.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancelMode {
    #[default]
    Immediate,
    AfterStep,
}

impl TurnCancelMode {
    pub fn is_immediate(&self) -> bool {
        *self == Self::Immediate
    }

    /// `Immediate` outranks `AfterStep`; a request in a stronger mode
    /// escalates an address that already holds a weaker durable request.
    pub fn is_stronger_than(self, other: Self) -> bool {
        matches!((self, other), (Self::Immediate, Self::AfterStep))
    }
}

/// Durable evidence that a turn was cancelled.
///
/// Minted either from a host turn-cancel request, which supplies the
/// `request_id` and `origin` verbatim, or, when lash itself originates the
/// cancellation, from [`TurnCancellationEvidence::internal`]. It is carried
/// only by [`TurnStop::Cancelled`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnCancellationEvidence {
    pub request_id: String,
    /// Opaque host-domain data. Lash records and returns it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Applied policy for active-turn input this turn did not deliver.
    #[serde(
        default,
        skip_serializing_if = "TurnCancelUndeliveredInputPolicy::is_defer"
    )]
    pub undelivered: TurnCancelUndeliveredInputPolicy,
    /// Mode of the request that produced this evidence.
    #[serde(default, skip_serializing_if = "TurnCancelMode::is_immediate")]
    pub mode: TurnCancelMode,
    /// The protocol iteration whose closing step boundary honoured an
    /// after-step request. `None` for immediate evidence and for an
    /// after-step request refused at the start gate before any step ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub honoured_after_step: Option<usize>,
}

impl TurnCancellationEvidence {
    /// The request-id namespace of a cancellation lash originated itself.
    pub const INTERNAL_REQUEST_ID_PREFIX: &'static str = "internal:";

    /// Evidence for a cancellation lash originated itself: no host cancel
    /// request exists, so the request id is namespaced `internal:`.
    pub fn internal(subject: impl std::fmt::Display) -> Self {
        Self {
            request_id: format!("{}{subject}", Self::INTERNAL_REQUEST_ID_PREFIX),
            origin: None,
            reason: None,
            undelivered: TurnCancelUndeliveredInputPolicy::Defer,
            mode: TurnCancelMode::Immediate,
            honoured_after_step: None,
        }
    }

    /// Whether lash originated this cancellation itself (a host-local stop,
    /// a provider's cancelled transport): no host request chose its
    /// undelivered-input policy.
    pub fn is_internal(&self) -> bool {
        self.request_id
            .starts_with(Self::INTERNAL_REQUEST_ID_PREFIX)
    }
}

/// Character cuts for the runtime's readable transcript copies. Whole values
/// remain on the outcome; these cuts do not grant or limit output retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeOutputCuts {
    pub value_reply_max_chars: usize,
    pub raw_error_max_chars: usize,
}
impl RuntimeOutputCuts {
    /// 16,384 value characters and 4,000 raw-error characters, plus an omission
    /// marker. These inherited presentation choices have no measurement backing.
    pub const fn standard() -> Self {
        Self {
            value_reply_max_chars: 16 * 1024,
            raw_error_max_chars: 4000,
        }
    }
}
impl Default for RuntimeOutputCuts {
    fn default() -> Self {
        Self::standard()
    }
}

pub fn make_error_envelope(
    kind: TurnFailureKind,
    code: Option<FailureCode>,
    terminal_reason: Option<crate::llm::types::LlmTerminalReason>,
    user_message: impl Into<String>,
    raw: Option<String>,
    cuts: RuntimeOutputCuts,
) -> ErrorEnvelope {
    let user_message = user_message.into();
    ErrorEnvelope {
        kind,
        code,
        terminal_reason,
        user_message,
        raw: raw.map(|s| truncate_raw_error(s.trim(), cuts.raw_error_max_chars)),
        retryable: None,
        provider_failure_kind: None,
    }
}

pub fn make_error_event(
    kind: TurnFailureKind,
    code: Option<FailureCode>,
    user_message: impl Into<String>,
    raw: Option<String>,
    cuts: RuntimeOutputCuts,
) -> SessionStreamEvent {
    let user_message = user_message.into();
    SessionStreamEvent::Error(ReportedFailure {
        message: user_message.clone(),
        envelope: Some(make_error_envelope(
            kind,
            code,
            None,
            user_message,
            raw,
            cuts,
        )),
    })
}

pub fn truncate_raw_error(s: &str, max_chars: usize) -> String {
    let raw_len = s.chars().count();
    if raw_len <= max_chars {
        return s.to_string();
    }
    let keep = max_chars / 2;
    let head = s.chars().take(keep).collect::<String>();
    let tail = s
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let omitted = raw_len.saturating_sub(keep * 2);
    format!("{head}\n\n... ({omitted} chars omitted) ...\n\n{tail}")
}

pub fn reassign_part_ids(message_id: &str, parts: &mut [Part]) {
    for (idx, part) in parts.iter_mut().enumerate() {
        *part.id_mut() = format!("{message_id}.p{idx}");
    }
}

pub fn model_tool_specs_iter<'a>(
    tools: impl IntoIterator<Item = &'a ToolDefinition>,
) -> Vec<LlmToolSpec> {
    tools
        .into_iter()
        .map(|tool| {
            let model_tool = tool.model_tool();
            LlmToolSpec {
                name: model_tool.name,
                description: model_tool.description,
                input_schema: model_tool.input_schema,
                output_schema: model_tool.output_schema,
            }
        })
        .collect()
}

pub fn model_tool_specs(tools: &[ToolDefinition]) -> Vec<LlmToolSpec> {
    model_tool_specs_iter(tools.iter())
}

#[cfg(test)]
mod stream_event_tests;

#[cfg(test)]
mod tests {
    use super::LlmUsage;
    use crate::llm::types::ProviderFailureKind;

    #[test]
    fn checked_token_usage_add_is_atomic_and_reasoning_is_not_additive_total() {
        let existing = LlmUsage {
            input_tokens: 1,
            output_tokens: i64::MAX,
            reasoning_output_tokens: i64::MAX,
            ..LlmUsage::default()
        };
        let overflow = existing
            .checked_add(&LlmUsage {
                input_tokens: 1,
                ..LlmUsage::default()
            })
            .expect_err("canonical total must be checked");
        assert_eq!(overflow.counter(), "total_tokens");
        assert_eq!(existing.input_tokens, 1);

        let reasoning_subset = LlmUsage {
            output_tokens: i64::MAX,
            reasoning_output_tokens: i64::MAX,
            ..LlmUsage::default()
        };
        assert_eq!(reasoning_subset.checked_total(), Ok(i64::MAX));
    }

    #[test]
    fn checked_input_total_is_not_subsumed_by_the_canonical_total() {
        // Counters are signed, so a negative `output_tokens` keeps the
        // canonical total in range while the prompt-side counters overflow.
        let prompt_overflow = LlmUsage {
            input_tokens: i64::MAX,
            output_tokens: i64::MIN,
            cache_read_input_tokens: i64::MAX,
            ..LlmUsage::default()
        };
        assert_eq!(prompt_overflow.checked_total(), Ok(i64::MAX - 1));
        assert_eq!(
            prompt_overflow
                .checked_input_total()
                .expect_err("the prompt-side subtotal must be checked separately")
                .counter(),
            "input_total_tokens"
        );

        let in_range = LlmUsage {
            input_tokens: 7,
            output_tokens: 3,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 2,
            reasoning_output_tokens: 1,
        };
        assert_eq!(in_range.checked_input_total(), Ok(in_range.input_total()));
    }

    // ─── ErrorEnvelope durable-snapshot compatibility ──────────────────
    //
    // `ErrorEnvelope` is persisted inside session snapshots and turn
    // checkpoints. The retryability fields added after the initial shape
    // must decode from legacy JSON (absent fields → `None`) and must not
    // appear on the wire when unset, so old readers keep decoding new
    // snapshots too.

    #[test]
    fn provider_failure_kind_refuses_unknown_future_codes() {
        assert!(
            serde_json::from_value::<ProviderFailureKind>(serde_json::json!("some_future_kind"))
                .is_err()
        );
        for kind in [
            ProviderFailureKind::Transport,
            ProviderFailureKind::Timeout,
            ProviderFailureKind::Http,
            ProviderFailureKind::Stream,
            ProviderFailureKind::Auth,
            ProviderFailureKind::Validation,
            ProviderFailureKind::Quota,
            ProviderFailureKind::Unsupported,
            ProviderFailureKind::Unknown,
        ] {
            let json = serde_json::to_value(kind).expect("serialize kind");
            assert_eq!(json, serde_json::json!(kind.code()));
            let round: ProviderFailureKind = serde_json::from_value(json).expect("decode kind");
            assert_eq!(round, kind);
        }
    }
}

/// The record kind and diagnostic retained when durable data cannot be decoded.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[error("stored {record_kind} data is corrupt: {message}")]
pub struct StoredDataCorruption {
    pub record_kind: String,
    pub message: String,
}
