pub mod message;
pub mod prompt;

pub use message::{
    BaseRenderCache, Message, MessageRole, MessageSequence, Part, PartAttachment, PartKind,
    PruneState, RenderedPrompt, append_rendered_prompt, messages_are_prompt_resume_safe,
    render_prompt, render_transcript_prompt, shared_parts,
};
pub use prompt::{
    MAIN_AGENT_INTRO, PromptBuiltin, PromptLayer, PromptSlot, PromptSlotLayer, PromptTemplate,
    PromptTemplateEntry, PromptTemplateSection, ResolvedPromptLayer, default_prompt_template,
    resolve_prompt_layers,
};

use std::sync::Arc;

/// Per-turn budget: the maximum number of protocol iterations (model calls) a
/// single turn may run before a clean MaxTurns stop is scheduled.
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
    /// Construct a finite per-turn iteration budget.
    ///
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
/// Unlike [`TurnBudget`], this budget has a bounded default: an absent value
/// is a host that never considered the stall, and the safe reading of silence
/// is the bound rather than the loop. `Unbounded` remains available as an
/// explicit host opt-in.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NoProgressBudget {
    Bounded(std::num::NonZeroUsize),
    Unbounded,
}

impl NoProgressBudget {
    /// Consecutive unproductive attempts allowed when a host expresses none.
    ///
    /// Judged runbook traffic repairs a bad cell within a handful of attempts;
    /// twelve leaves that headroom untouched while turning the measured
    /// 1,223-call stall into twelve calls.
    pub const DEFAULT_MAX_ATTEMPTS: usize = 12;

    /// Construct a finite bound on consecutive unproductive attempts.
    ///
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

    /// Whether `attempts` consecutive unproductive attempts exhaust the bound.
    pub fn is_exhausted_by(self, attempts: usize) -> bool {
        self.max_attempts()
            .is_some_and(|max_attempts| attempts >= max_attempts)
    }
}

impl Default for NoProgressBudget {
    fn default() -> Self {
        Self::bounded(Self::DEFAULT_MAX_ATTEMPTS)
    }
}

use crate::MessageOrigin;
use crate::ToolDefinition;
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub role: MessageRole,
    pub parts: Arc<Vec<Part>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
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
        }
    }

    pub fn to_message(&self) -> Message {
        Message {
            id: self.id.clone(),
            role: self.role,
            parts: Arc::clone(&self.parts),
            origin: self.origin.clone(),
        }
    }
}

/// Token usage statistics from an LLM call.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenUsageOverflow {
    counter: &'static str,
}

impl TokenUsageOverflow {
    pub fn counter(self) -> &'static str {
        self.counter
    }
}

impl TokenUsage {
    pub fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_input_tokens == 0
            && self.cache_write_input_tokens == 0
            && self.reasoning_output_tokens == 0
    }

    pub fn total(&self) -> i64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_write_input_tokens
    }

    /// Bare prompt-side sum, valid only for counters already admitted through
    /// a checked seam. Use [`Self::checked_input_total`] on any value that
    /// still carries raw provider or durable input.
    pub fn input_total(&self) -> i64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_write_input_tokens
    }

    /// Returns a new usage value with every counter added atomically.
    ///
    /// `reasoning_output_tokens` is checked as a counter but excluded from
    /// `total_tokens` because it is a subset of `output_tokens`.
    pub fn checked_add(&self, other: &TokenUsage) -> Result<Self, TokenUsageOverflow> {
        let merged = Self {
            input_tokens: self.input_tokens.checked_add(other.input_tokens).ok_or(
                TokenUsageOverflow {
                    counter: "input_tokens",
                },
            )?,
            output_tokens: self.output_tokens.checked_add(other.output_tokens).ok_or(
                TokenUsageOverflow {
                    counter: "output_tokens",
                },
            )?,
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .checked_add(other.cache_read_input_tokens)
                .ok_or(TokenUsageOverflow {
                    counter: "cache_read_input_tokens",
                })?,
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .checked_add(other.cache_write_input_tokens)
                .ok_or(TokenUsageOverflow {
                    counter: "cache_write_input_tokens",
                })?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .checked_add(other.reasoning_output_tokens)
                .ok_or(TokenUsageOverflow {
                    counter: "reasoning_output_tokens",
                })?,
        };
        merged.checked_total()?;
        Ok(merged)
    }

    pub fn checked_total(&self) -> Result<i64, TokenUsageOverflow> {
        self.input_tokens
            .checked_add(self.output_tokens)
            .and_then(|total| total.checked_add(self.cache_read_input_tokens))
            .and_then(|total| total.checked_add(self.cache_write_input_tokens))
            .ok_or(TokenUsageOverflow {
                counter: "total_tokens",
            })
    }

    /// Checked prompt-side subtotal, the value context-window policy compares
    /// against a model's window.
    ///
    /// [`Self::checked_total`] does not subsume it: counters are signed, so a
    /// negative `output_tokens` can hold the canonical total in range while the
    /// prompt-side counters alone overflow. Both aggregations are validated
    /// wherever raw counters are admitted.
    pub fn checked_input_total(&self) -> Result<i64, TokenUsageOverflow> {
        self.input_tokens
            .checked_add(self.cache_read_input_tokens)
            .and_then(|total| total.checked_add(self.cache_write_input_tokens))
            .ok_or(TokenUsageOverflow {
                counter: "input_total_tokens",
            })
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
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
// justification: this public streaming DTO stays inline to avoid per-event allocation and preserve consumer pattern matching.
#[allow(clippy::large_enum_variant)]
pub enum SessionStreamEvent {
    #[serde(rename = "text_delta")]
    TextDelta { content: String },
    /// Streaming update for the model's reasoning summary ("thinking"), kept
    /// separate from assistant response text and never fed back to the model
    /// on subsequent turns.
    #[serde(rename = "reasoning_delta")]
    ReasoningDelta { content: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: crate::ToolCallOutput,
        duration_ms: u64,
    },
    #[serde(rename = "tool_call_start")]
    ToolCallStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
    },
    #[serde(rename = "message")]
    Message { text: String, kind: String },
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
        duration_ms: u64,
    },
    #[serde(rename = "token_usage")]
    TokenUsage {
        protocol_iteration: usize,
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    #[serde(rename = "child_token_usage")]
    ChildTokenUsage {
        session_id: String,
        source: String,
        model: String,
        protocol_iteration: usize,
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    #[serde(rename = "retry_status")]
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ErrorEnvelope>,
    },
    #[serde(rename = "injected_turn_input_accepted")]
    InjectedTurnInputAccepted {
        inputs: Vec<AcceptedInjectedTurnInput>,
        checkpoint: CheckpointKind,
    },
    #[serde(rename = "injected_messages_committed")]
    InjectedMessagesCommitted {
        messages: Vec<PluginMessage>,
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
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ErrorEnvelope>,
    },
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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
}

impl TurnCancellationEvidence {
    /// Evidence for a cancellation lash originated itself: no host cancel
    /// request exists, so the request id is namespaced `internal:`.
    pub fn internal(subject: impl std::fmt::Display) -> Self {
        Self {
            request_id: format!("internal:{subject}"),
            origin: None,
            reason: None,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnTerminationPolicyState {
    turn_limit_final_scheduled: bool,
}

impl Default for TurnTerminationPolicyState {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnTerminationPolicyState {
    pub fn new() -> Self {
        Self {
            turn_limit_final_scheduled: false,
        }
    }

    pub fn should_force_exit_after_grace_turn(&self) -> bool {
        self.turn_limit_final_scheduled
    }

    pub fn turn_limit_final_to_schedule(
        &self,
        protocol_iteration: usize,
        protocol_run_offset: usize,
        turn_budget: TurnBudget,
    ) -> Option<usize> {
        if self.turn_limit_final_scheduled {
            return None;
        }
        let max = turn_budget.max_turns()?;
        if protocol_iteration < protocol_run_offset + max {
            return None;
        }
        Some(max)
    }

    pub fn mark_turn_limit_final_scheduled(&mut self) {
        self.turn_limit_final_scheduled = true;
    }
}

pub fn make_error_envelope(
    kind: &str,
    code: Option<&str>,
    terminal_reason: Option<crate::llm::types::LlmTerminalReason>,
    user_message: impl Into<String>,
    raw: Option<String>,
) -> ErrorEnvelope {
    let user_message = user_message.into();
    ErrorEnvelope {
        kind: kind.to_string(),
        code: code.map(str::to_string),
        terminal_reason,
        user_message,
        raw: raw.map(|s| truncate_raw_error(s.trim())),
        retryable: None,
        provider_failure_kind: None,
    }
}

pub fn make_error_event(
    kind: &str,
    code: Option<&str>,
    user_message: impl Into<String>,
    raw: Option<String>,
) -> SessionStreamEvent {
    let user_message = user_message.into();
    SessionStreamEvent::Error {
        message: user_message.clone(),
        envelope: Some(make_error_envelope(kind, code, None, user_message, raw)),
    }
}

pub fn truncate_raw_error(s: &str) -> String {
    const MAX_RAW: usize = 4000;
    let raw_len = s.chars().count();
    if raw_len <= MAX_RAW {
        return s.to_string();
    }
    let keep = MAX_RAW / 2;
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
        part.id = format!("{message_id}.p{idx}");
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
mod tests {
    use super::{
        ErrorEnvelope, NoProgressBudget, SessionStreamEvent, TokenUsage, TurnBudget, TurnOutcome,
        TurnTerminationPolicyState,
    };
    use crate::llm::types::{LlmTerminalReason, ProviderFailureKind};

    #[test]
    fn bounded_turn_budget_exhausts_exactly_at_n_iterations() {
        let state = TurnTerminationPolicyState::new();
        let budget = TurnBudget::bounded(3);

        assert_eq!(state.turn_limit_final_to_schedule(6, 4, budget), None);
        assert_eq!(state.turn_limit_final_to_schedule(7, 4, budget), Some(3));
    }

    #[test]
    #[should_panic(expected = "turn budget must be non-zero; use TurnBudget::Unbounded to opt out")]
    fn bounded_turn_budget_rejects_zero() {
        let _ = TurnBudget::bounded(0);
    }

    /// The no-progress budget is exhausted *at* its bound, not past it: the
    /// nth consecutive unproductive attempt is the last one bought.
    #[test]
    fn a_bounded_no_progress_budget_is_exhausted_at_its_bound() {
        let budget = NoProgressBudget::bounded(3);

        assert!(!budget.is_exhausted_by(0));
        assert!(!budget.is_exhausted_by(2));
        assert!(budget.is_exhausted_by(3));
        assert!(budget.is_exhausted_by(4));
        assert_eq!(budget.max_attempts(), Some(3));
    }

    /// Unlike the turn budget, silence resolves to the bound. A default that
    /// loops is the bug this budget exists to close.
    #[test]
    fn an_absent_no_progress_budget_is_bounded() {
        assert_eq!(
            NoProgressBudget::default().max_attempts(),
            Some(NoProgressBudget::DEFAULT_MAX_ATTEMPTS)
        );
        assert!(NoProgressBudget::default().is_exhausted_by(usize::MAX));
        assert!(!NoProgressBudget::Unbounded.is_exhausted_by(usize::MAX));
        assert_eq!(NoProgressBudget::Unbounded.max_attempts(), None);
    }

    #[test]
    #[should_panic(
        expected = "no-progress budget must be non-zero; use NoProgressBudget::Unbounded to opt out"
    )]
    fn a_bounded_no_progress_budget_rejects_zero() {
        let _ = NoProgressBudget::bounded(0);
    }

    #[test]
    fn unbounded_turn_budget_never_schedules_a_limit_stop() {
        let state = TurnTerminationPolicyState::new();

        for iteration in [0, 1, 10_000, usize::MAX] {
            assert_eq!(
                state.turn_limit_final_to_schedule(iteration, 0, TurnBudget::Unbounded),
                None
            );
        }
    }

    #[test]
    fn checked_token_usage_add_is_atomic_and_reasoning_is_not_additive_total() {
        let existing = TokenUsage {
            input_tokens: 1,
            output_tokens: i64::MAX,
            reasoning_output_tokens: i64::MAX,
            ..TokenUsage::default()
        };
        let overflow = existing
            .checked_add(&TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            })
            .expect_err("canonical total must be checked");
        assert_eq!(overflow.counter(), "total_tokens");
        assert_eq!(existing.input_tokens, 1);

        let reasoning_subset = TokenUsage {
            output_tokens: i64::MAX,
            reasoning_output_tokens: i64::MAX,
            ..TokenUsage::default()
        };
        assert_eq!(reasoning_subset.checked_total(), Ok(i64::MAX));
    }

    #[test]
    fn checked_input_total_is_not_subsumed_by_the_canonical_total() {
        // Counters are signed, so a negative `output_tokens` keeps the
        // canonical total in range while the prompt-side counters overflow.
        let prompt_overflow = TokenUsage {
            input_tokens: i64::MAX,
            output_tokens: i64::MIN,
            cache_read_input_tokens: i64::MAX,
            ..TokenUsage::default()
        };
        assert_eq!(prompt_overflow.checked_total(), Ok(i64::MAX - 1));
        assert_eq!(
            prompt_overflow
                .checked_input_total()
                .expect_err("the prompt-side subtotal must be checked separately")
                .counter(),
            "input_total_tokens"
        );

        let in_range = TokenUsage {
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
    fn error_envelope_decodes_legacy_snapshot_without_retryability_fields() {
        let legacy = r#"{
            "kind":"llm_provider",
            "code":"429",
            "terminal_reason":"provider_error",
            "user_message":"LLM error: rate limited",
            "raw":"{\"error\":\"rate_limited\"}"
        }"#;
        let envelope: ErrorEnvelope = serde_json::from_str(legacy).expect("legacy envelope");
        assert_eq!(envelope.kind, "llm_provider");
        assert_eq!(envelope.retryable, None);
        assert_eq!(envelope.provider_failure_kind, None);

        // The legacy shape embedded in a persisted `SessionStreamEvent::Error`
        // record decodes the same way.
        let legacy_event = r#"{
            "type":"error",
            "message":"LLM error: rate limited",
            "envelope":{"kind":"llm_provider","user_message":"LLM error: rate limited"}
        }"#;
        let event: SessionStreamEvent = serde_json::from_str(legacy_event).expect("legacy event");
        match event {
            SessionStreamEvent::Error { envelope, .. } => {
                let envelope = envelope.expect("envelope");
                assert_eq!(envelope.retryable, None);
                assert_eq!(envelope.provider_failure_kind, None);
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[test]
    fn error_envelope_roundtrips_retryability_fields() {
        let envelope = ErrorEnvelope {
            kind: "llm_provider".to_string(),
            code: Some("429".to_string()),
            terminal_reason: Some(LlmTerminalReason::ProviderError),
            user_message: "LLM error: rate limited".to_string(),
            raw: None,
            retryable: Some(true),
            provider_failure_kind: Some(ProviderFailureKind::Quota),
        };
        let json = serde_json::to_value(&envelope).expect("serialize envelope");
        assert_eq!(json["retryable"], serde_json::json!(true));
        assert_eq!(json["provider_failure_kind"], serde_json::json!("quota"));
        let decoded: ErrorEnvelope = serde_json::from_value(json).expect("decode envelope");
        assert_eq!(decoded.retryable, Some(true));
        assert_eq!(
            decoded.provider_failure_kind,
            Some(ProviderFailureKind::Quota)
        );
    }

    #[test]
    fn error_envelope_omits_unset_retryability_fields_on_the_wire() {
        let envelope = ErrorEnvelope {
            kind: "plugin".to_string(),
            code: Some("plugin_abort".to_string()),
            terminal_reason: None,
            user_message: "stopped".to_string(),
            raw: None,
            retryable: None,
            provider_failure_kind: None,
        };
        let json = serde_json::to_value(&envelope).expect("serialize envelope");
        let object = json.as_object().expect("object");
        assert!(!object.contains_key("retryable"));
        assert!(!object.contains_key("provider_failure_kind"));
    }

    #[test]
    fn provider_failure_kind_decodes_unknown_future_codes() {
        // Forward compatibility: a snapshot written by a newer runtime with a
        // kind this build does not know decodes as `Unknown`.
        let decoded: ProviderFailureKind =
            serde_json::from_value(serde_json::json!("some_future_kind")).expect("future kind");
        assert_eq!(decoded, ProviderFailureKind::Unknown);
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

    #[test]
    fn agent_frame_switch_decodes_event_without_initial_nodes() {
        let frame_key =
            crate::FrameKey::from_caller_material("frame-2").expect("non-empty caller material");
        let event_json = format!(
            r#"{{
            "type":"turn_outcome",
            "outcome":{{
                "agent_frame_switch":{{
                    "frame_key":"{}",
                    "task":"continue"
                }}
            }}
        }}"#,
            frame_key.as_str()
        );
        let event: SessionStreamEvent =
            serde_json::from_str(&event_json).expect("frame switch event");
        match event {
            SessionStreamEvent::TurnOutcome {
                outcome:
                    TurnOutcome::AgentFrameSwitch {
                        frame_key: decoded_frame_key,
                        task,
                        initial_nodes,
                    },
            } => {
                assert_eq!(decoded_frame_key, frame_key);
                assert_eq!(task, "continue");
                assert!(initial_nodes.is_empty());
            }
            other => panic!("expected agent-frame switch event, got {other:?}"),
        }
    }
}
