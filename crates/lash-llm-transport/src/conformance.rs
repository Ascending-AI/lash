//! Backend-agnostic conformance suite for LLM provider response normalization.
//!
//! Every provider crate converts its own wire format into the same normalized
//! shapes — `LlmOutputPart`, `LlmUsage`, `LlmTerminalReason` — and the same
//! streamed assembly. This suite pins that contract: each provider implements
//! [`ProviderNormalizer`] (supplying its own wire encoding of each canonical
//! [`Scenario`]), and [`provider_conformance`] runs every scenario and asserts
//! the normalized output matches the canonical expectation. A provider whose
//! mapping diverges (e.g. a different cache-token convention, or a finish
//! reason that lands in the wrong terminal bucket) fails the suite.
//!
//! Gated behind the `testing` feature. Provider crates implement the trait in
//! their own test modules — internals stay private — and run the suite from a
//! `#[test]`/`#[tokio::test]`.
//! The suite's >=2-event rules constrain fixture construction, not parser
//! implementations; adapters put the real streaming path under test by
//! implementing [`ProviderNormalizer::assemble_stream`].

use lash_sansio::llm::types::{LlmMessage, LlmOutputPart, LlmTerminalReason, LlmUsage};
use lash_sansio::session_model::{
    Message, MessageRole, Part, PartKind, PruneState, render_prompt, shared_parts,
};
use serde_json::Value;

/// The canonical scenarios every provider must normalize identically. Each
/// names a single logical model response; the provider supplies its own wire
/// bytes for it via [`ProviderNormalizer::wire_for`], and the suite asserts the
/// normalized result equals this scenario's canonical expectation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// A plain completed text turn. Usage: base input/output, no cache, no
    /// reasoning. Terminal reason: `Stop`.
    PlainTextStop,
    /// A turn that stopped because it hit the output-token cap. Terminal
    /// reason: `OutputLimit`.
    OutputCapped,
    /// A non-streaming response that ended by emitting a tool call. Terminal
    /// reason: `ToolUse`, and normalized parts contain the tool call.
    NonStreamingToolUse,
    /// A streamed response whose assistant text arrives across multiple chunks
    /// and must assemble into one visible assistant text stream.
    StreamingTextAssembly,
    /// A streamed response whose tool-call arguments arrive split across
    /// multiple chunks and must reassemble into one valid `input_json`.
    StreamingToolArgumentMerge,
    /// A turn stopped by the provider's content filter. Terminal reason:
    /// `ContentFilter`.
    ContentFilter,
    /// Usage with a cache hit: some prompt tokens were served from cache.
    /// `input_tokens` must reflect uncached input and
    /// `cache_read_input_tokens` must reflect the cached portion.
    UsageCacheHit,
    /// Usage with reasoning tokens reported separately.
    UsageReasoning,
    /// A turn whose reasoning/thinking output is surfaced as an
    /// `LlmOutputPart::Reasoning` (distinct from assistant text).
    ReasoningExtraction,
    /// A streamed reasoning item carrying opaque provider replay state. The
    /// scenario normalizes the stream, commits the result to durable history,
    /// projects the next LLM request, and asks the provider to serialize it.
    /// Losing the opaque field is invisible in the response turn itself, but
    /// makes the *next* request invalid or semantically discontinuous because
    /// the provider cannot verify or resume its earlier reasoning item.
    ReasoningReplayRoundTrip,
    /// Usage tokens that arrive split across two streaming events (input in
    /// one, output in a later one). The assembled usage must carry both —
    /// i.e. the provider merges incremental usage rather than overwriting it.
    StreamingUsageMerge,
}

impl Scenario {
    pub const ALL: &'static [Scenario] = &[
        Scenario::PlainTextStop,
        Scenario::OutputCapped,
        Scenario::NonStreamingToolUse,
        Scenario::StreamingTextAssembly,
        Scenario::StreamingToolArgumentMerge,
        Scenario::ContentFilter,
        Scenario::UsageCacheHit,
        Scenario::UsageReasoning,
        Scenario::ReasoningExtraction,
        Scenario::ReasoningReplayRoundTrip,
        Scenario::StreamingUsageMerge,
    ];
}

/// Checked-in inventory of acknowledged conformance gaps. Validation is
/// bidirectional: adding or removing a declaration requires editing this list.
const UNSUPPORTED_DECLARATION_INVENTORY: &[(&str, Scenario)] = &[
    ("google-gemini", Scenario::StreamingToolArgumentMerge),
    ("google-gemini", Scenario::StreamingUsageMerge),
];

/// Canonical token counts each provider must report for a usage scenario.
/// The provider's wire encoding may name and nest these differently; the
/// normalizer must map them to exactly these values.
pub struct CanonicalUsage;

impl CanonicalUsage {
    pub const BASE_INPUT: i64 = 100;
    pub const BASE_OUTPUT: i64 = 20;
    /// For `UsageCacheHit`: of `BASE_INPUT` total prompt tokens, this many
    /// were cache reads.
    pub const CACHED_INPUT: i64 = 40;
    /// For `UsageReasoning`: reasoning tokens reported alongside output.
    pub const REASONING: i64 = 15;
    /// `output_tokens` includes reasoning tokens; reasoning is reported as a
    /// subset, not as an extra additive bucket.
    pub const OUTPUT_WITH_REASONING: i64 = Self::BASE_OUTPUT + Self::REASONING;
}

/// Explicit provider-specific unsupported scenario list.
///
/// Most providers should use [`ProviderConformanceSpec::strict`]. A provider
/// that cannot express a canonical scenario must list it here with a stable,
/// human-readable reason. Returning `None` from `wire_for` without declaring
/// the scenario unsupported is a conformance failure.
#[derive(Clone, Copy, Debug)]
pub struct ProviderConformanceSpec {
    unsupported: &'static [(Scenario, &'static str)],
}

impl ProviderConformanceSpec {
    pub const fn strict() -> Self {
        Self { unsupported: &[] }
    }

    pub const fn with_unsupported(unsupported: &'static [(Scenario, &'static str)]) -> Self {
        Self { unsupported }
    }

    fn unsupported_reason(&self, scenario: Scenario) -> Option<&'static str> {
        self.unsupported
            .iter()
            .find_map(|(candidate, reason)| (*candidate == scenario).then_some(*reason))
    }

    fn validate(&self, provider: &str) {
        let mut seen = Vec::new();
        for &(scenario, reason) in self.unsupported {
            assert!(
                Scenario::ALL.contains(&scenario),
                "[{provider}] unsupported scenario {scenario:?} is not part of Scenario::ALL"
            );
            assert!(
                !reason.trim().is_empty(),
                "[{provider}] unsupported scenario {scenario:?} must include a non-empty reason"
            );
            assert!(
                seen.iter().all(|existing| *existing != scenario),
                "[{provider}] unsupported scenario {scenario:?} is listed more than once"
            );
            assert!(
                UNSUPPORTED_DECLARATION_INVENTORY.contains(&(provider, scenario)),
                "[{provider}] unsupported scenario {scenario:?} is missing from the checked-in \
                 declaration inventory"
            );
            seen.push(scenario);
        }
        for &(_, scenario) in UNSUPPORTED_DECLARATION_INVENTORY
            .iter()
            .filter(|(candidate, _)| *candidate == provider)
        {
            assert!(
                seen.contains(&scenario),
                "[{provider}] checked-in unsupported scenario {scenario:?} has no matching \
                 declaration"
            );
        }
    }
}

/// One provider's wire-format encoding of a [`Scenario`], plus the few facts
/// the suite needs to drive assertions. The provider produces this; the suite
/// owns what "correct" normalization means.
pub struct ProviderWire {
    /// The non-streaming response body (provider's own JSON shape) the suite
    /// feeds to [`ProviderNormalizer::parts_from_wire`] /
    /// [`ProviderNormalizer::terminal_from_wire`] / `usage_from_wire`.
    pub body: Value,
    /// For `Scenario::StreamingToolArgumentMerge`: the SSE event strings
    /// (provider's own format) that stream the tool call, deliberately split so
    /// the call's arguments arrive across >=2 chunks. `None` for non-streaming
    /// scenarios.
    pub tool_call_sse: Option<Vec<String>>,
    /// For `Scenario::StreamingTextAssembly`: the SSE event strings
    /// (provider's own format) that stream assistant text across >=2 chunks.
    pub text_sse: Option<Vec<String>>,
    /// The expected concatenated assistant text for `StreamingTextAssembly`.
    pub expected_stream_text: Option<String>,
    /// The expected reassembled tool-call `input_json` (canonical, e.g.
    /// `{"q":"x"}`) for `Scenario::StreamingToolArgumentMerge`. The provider
    /// states what its fixtures encode so the suite can compare regardless of
    /// key formatting.
    pub expected_tool_input_json: Option<Value>,
    /// The tool name encoded in the tool-use fixtures.
    pub expected_tool_name: Option<String>,
    /// For `Scenario::ReasoningExtraction`: the reasoning text the fixtures
    /// encode. The suite asserts an `LlmOutputPart::Reasoning` carrying it.
    pub expected_reasoning_text: Option<String>,
    /// For `Scenario::ReasoningReplayRoundTrip`: the provider stream carrying
    /// a reasoning item with opaque replay state. Streaming providers must use
    /// this path rather than relying on their batch parser.
    pub reasoning_replay_sse: Option<Vec<String>>,
    /// The exact opaque replay bytes represented as a wire string.
    pub expected_replay_payload: Option<String>,
    /// JSON Pointer locating that payload in the serialized next request.
    pub request_replay_json_pointer: Option<String>,
    /// For `Scenario::StreamingUsageMerge`: SSE events that carry usage split
    /// across ≥2 chunks (e.g. input in one, output in a later one). The suite
    /// feeds them to [`ProviderNormalizer::assemble_stream`] and asserts the
    /// assembled usage carries both halves.
    pub usage_merge_sse: Option<Vec<String>>,
}

impl ProviderWire {
    pub fn body(body: Value) -> Self {
        Self {
            body,
            tool_call_sse: None,
            text_sse: None,
            expected_stream_text: None,
            expected_tool_input_json: None,
            expected_tool_name: None,
            expected_reasoning_text: None,
            reasoning_replay_sse: None,
            expected_replay_payload: None,
            request_replay_json_pointer: None,
            usage_merge_sse: None,
        }
    }

    pub fn with_tool_call_stream(
        mut self,
        sse: Vec<String>,
        tool_name: impl Into<String>,
        expected_input_json: Value,
    ) -> Self {
        self.tool_call_sse = Some(sse);
        self.expected_tool_name = Some(tool_name.into());
        self.expected_tool_input_json = Some(expected_input_json);
        self
    }

    pub fn with_text_stream(mut self, sse: Vec<String>, expected_text: impl Into<String>) -> Self {
        self.text_sse = Some(sse);
        self.expected_stream_text = Some(expected_text.into());
        self
    }

    /// The expected reasoning text for a `ReasoningExtraction` fixture.
    pub fn with_reasoning_text(mut self, text: impl Into<String>) -> Self {
        self.expected_reasoning_text = Some(text.into());
        self
    }

    /// Configure the streamed reasoning-replay round-trip fixture and the
    /// provider-dialect request location where its opaque value must reappear.
    pub fn with_reasoning_replay_round_trip(
        mut self,
        sse: Vec<String>,
        expected_payload: impl Into<String>,
        request_json_pointer: impl Into<String>,
    ) -> Self {
        self.reasoning_replay_sse = Some(sse);
        self.expected_replay_payload = Some(expected_payload.into());
        self.request_replay_json_pointer = Some(request_json_pointer.into());
        self
    }

    /// The SSE events for a `StreamingUsageMerge` fixture (usage split across
    /// chunks).
    pub fn with_usage_merge_stream(mut self, sse: Vec<String>) -> Self {
        self.usage_merge_sse = Some(sse);
        self
    }
}

/// What a provider's streaming parser produced after consuming a sequence of
/// SSE events. Hides each provider's concrete stream-state type.
#[derive(Default)]
pub struct StreamAssembly {
    pub parts: Vec<LlmOutputPart>,
    pub usage: LlmUsage,
}

/// A provider's adapter into the conformance suite. Implementations wrap the
/// crate's existing (often private) parsers — they do not reimplement parsing.
pub trait ProviderNormalizer {
    /// Human-readable provider name, used in assertion messages.
    fn name(&self) -> &str;

    /// This provider's wire encoding of `scenario`, or `None` if the provider
    /// explicitly declares the scenario unsupported in
    /// [`ProviderNormalizer::conformance_spec`].
    fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire>;

    /// Provider-specific unsupported scenarios. Defaults to strict: every
    /// canonical scenario must be supplied by [`ProviderNormalizer::wire_for`].
    fn conformance_spec(&self) -> ProviderConformanceSpec {
        ProviderConformanceSpec::strict()
    }

    /// Normalize a non-streaming response body into output parts.
    fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart>;

    /// Normalize usage from a response body.
    fn usage_from_wire(&self, body: &Value) -> LlmUsage;

    /// Normalize the terminal reason from a response body + its parts.
    fn terminal_from_wire(&self, body: &Value, parts: &[LlmOutputPart]) -> LlmTerminalReason;

    /// Feed a sequence of raw SSE event strings through the provider's stream
    /// parser and return the assembled parts. Hides the provider's state type.
    fn assemble_stream(&self, scenario: Scenario, sse_events: &[String]) -> StreamAssembly;

    /// Commit normalized response parts to this path's durable history,
    /// project the next LLM request, and serialize it with the real provider
    /// request builder. The replay scenario then inspects the returned wire
    /// value at its provider-specific JSON Pointer.
    fn build_next_request(&self, messages: Vec<LlmMessage>) -> Value;
}

/// Commit a normalized response to Standard-protocol history and project the
/// messages for the next request. Provider conformance adapters pass these
/// messages to their real request builders; keeping this bridge in the shared
/// suite prevents adapters from hand-copying replay metadata around the
/// history seam the scenario is meant to protect.
fn standard_next_request_messages(parts: &[LlmOutputPart]) -> Vec<LlmMessage> {
    let assistant_id = "conformance.assistant";
    let mut history_parts = Vec::new();
    for part in parts {
        match part {
            LlmOutputPart::Text {
                text,
                response_meta,
            } => history_parts.push(Part {
                id: format!("{assistant_id}.p{}", history_parts.len()),
                kind: PartKind::Prose,
                content: text.clone(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: response_meta.clone(),
            }),
            LlmOutputPart::Reasoning { text, replay } => {
                history_parts.push(lash_sansio::reasoning_part(
                    assistant_id,
                    history_parts.len(),
                    text.clone(),
                    replay.clone(),
                ))
            }
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => history_parts.push(Part {
                id: format!("{assistant_id}.p{}", history_parts.len()),
                kind: PartKind::ToolCall,
                content: input_json.clone(),
                attachment: None,
                tool_call_id: Some(call_id.clone()),
                tool_name: Some(tool_name.clone()),
                tool_replay: replay.clone(),
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }),
        }
    }
    let history = vec![
        Message {
            id: assistant_id.to_string(),
            role: MessageRole::Assistant,
            parts: shared_parts(history_parts),
            origin: None,
        },
        Message {
            id: "conformance.user".to_string(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part {
                id: "conformance.user.p0".to_string(),
                kind: PartKind::Text,
                content: "continue".to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        },
    ];
    let encoded = serde_json::to_string(&history).expect("conformance history must serialize");
    let durable_history: Vec<Message> =
        serde_json::from_str(&encoded).expect("conformance history must deserialize");
    render_prompt(&durable_history).messages
}

/// Build a trim/truncation-sensitive opaque fixture payload of roughly 2 KiB.
pub fn strong_replay_payload(provider: &str) -> String {
    format!(
        " \n{provider}: replay-é\n{}\n ",
        "0123456789abcdef".repeat(128)
    )
}

/// Run every [`Scenario`] against `n` and assert the normalized output matches
/// the canonical expectation. Panics on the first divergence.
pub fn provider_conformance(n: &dyn ProviderNormalizer) {
    let who = n.name();
    let spec = n.conformance_spec();
    spec.validate(who);
    for &scenario in Scenario::ALL {
        if let Some(reason) = spec.unsupported_reason(scenario) {
            assert!(
                n.wire_for(scenario).is_none(),
                "[{}] {scenario:?}: scenario is declared unsupported ({reason}) but wire_for \
                 returned a fixture",
                n.name()
            );
            println!("[{who}] {scenario:?}: SKIPPED — {reason}");
            continue;
        };
        let Some(wire) = n.wire_for(scenario) else {
            panic!(
                "[{}] {scenario:?}: supported scenario returned None; either supply a fixture or \
                 add it to ProviderConformanceSpec with a reason",
                n.name()
            );
        };
        check_scenario(n, scenario, wire);
    }
}

fn check_scenario(n: &dyn ProviderNormalizer, scenario: Scenario, wire: ProviderWire) {
    let who = n.name();

    match scenario {
        Scenario::PlainTextStop => {
            let parts = n.parts_from_wire(&wire.body);
            assert_terminal(n, &wire, &parts, LlmTerminalReason::Stop, who, scenario);
            assert_usage(
                n,
                &wire,
                who,
                scenario,
                CanonicalUsage::BASE_INPUT,
                CanonicalUsage::BASE_OUTPUT,
                0,
                0,
                0,
            );
        }
        Scenario::OutputCapped => {
            let parts = n.parts_from_wire(&wire.body);
            assert_terminal(
                n,
                &wire,
                &parts,
                LlmTerminalReason::OutputLimit,
                who,
                scenario,
            );
        }
        Scenario::ContentFilter => {
            let parts = n.parts_from_wire(&wire.body);
            assert_terminal(
                n,
                &wire,
                &parts,
                LlmTerminalReason::ContentFilter,
                who,
                scenario,
            );
        }
        Scenario::NonStreamingToolUse => {
            let parts = n.parts_from_wire(&wire.body);
            assert_terminal(n, &wire, &parts, LlmTerminalReason::ToolUse, who, scenario);
            assert!(
                parts.iter().any(is_tool_call),
                "[{who}] {scenario:?}: non-streaming parts must contain a tool call, got {parts:?}"
            );
        }
        Scenario::StreamingTextAssembly => {
            let sse = wire
                .text_sse
                .as_ref()
                .unwrap_or_else(|| panic!("[{who}] {scenario:?}: must supply text_sse"));
            assert!(
                sse.len() >= 2,
                "[{who}] {scenario:?}: text SSE must split assistant text across ≥2 chunks to be \
                 a meaningful assembly test, got {} chunk(s)",
                sse.len()
            );
            let assembled = n.assemble_stream(scenario, sse);
            let text = assembled
                .parts
                .iter()
                .filter_map(as_text)
                .collect::<String>();
            let expected = wire.expected_stream_text.as_ref().unwrap_or_else(|| {
                panic!("[{who}] {scenario:?}: must supply expected_stream_text")
            });
            assert_eq!(
                &text, expected,
                "[{who}] {scenario:?}: streamed assistant text must assemble in order from deltas, \
                 got parts {:?}",
                assembled.parts
            );
            assert!(
                assembled.parts.iter().all(|part| !is_tool_call(part)),
                "[{who}] {scenario:?}: plain text streaming must not synthesize tool calls, got {:?}",
                assembled.parts
            );
        }
        Scenario::StreamingToolArgumentMerge => {
            let sse = wire
                .tool_call_sse
                .as_ref()
                .unwrap_or_else(|| panic!("[{who}] {scenario:?}: must supply tool_call_sse"));
            assert!(
                sse.len() >= 2,
                "[{who}] {scenario:?}: tool-call SSE must split arguments across ≥2 chunks to be \
                 a meaningful assembly test, got {} chunk(s)",
                sse.len()
            );
            let assembled = n.assemble_stream(scenario, sse);
            let tool = assembled
                .parts
                .iter()
                .find_map(as_tool_call)
                .unwrap_or_else(|| {
                    panic!(
                        "[{who}] {scenario:?}: streamed assembly produced no tool call, got {:?}",
                        assembled.parts
                    )
                });
            let (tool_name, input_json) = tool;
            if let Some(expected_name) = &wire.expected_tool_name {
                assert_eq!(
                    &tool_name, expected_name,
                    "[{who}] {scenario:?}: streamed tool name mismatch"
                );
            }
            let expected_json = wire
                .expected_tool_input_json
                .as_ref()
                .expect("ToolUse supplies expected_tool_input_json");
            let got_json: Value = serde_json::from_str(&input_json).unwrap_or_else(|err| {
                panic!(
                    "[{who}] {scenario:?}: reassembled tool input_json is not valid JSON: {err}; \
                     raw = {input_json:?}"
                )
            });
            assert_eq!(
                &got_json, expected_json,
                "[{who}] {scenario:?}: tool-call arguments must reassemble identically across \
                 chunk boundaries"
            );
        }
        Scenario::UsageCacheHit => {
            assert_usage(
                n,
                &wire,
                who,
                scenario,
                CanonicalUsage::BASE_INPUT - CanonicalUsage::CACHED_INPUT,
                CanonicalUsage::BASE_OUTPUT,
                CanonicalUsage::CACHED_INPUT,
                0,
                0,
            );
        }
        Scenario::UsageReasoning => {
            assert_usage(
                n,
                &wire,
                who,
                scenario,
                CanonicalUsage::BASE_INPUT,
                CanonicalUsage::OUTPUT_WITH_REASONING,
                0,
                0,
                CanonicalUsage::REASONING,
            );
        }
        Scenario::ReasoningExtraction => {
            let parts = n.parts_from_wire(&wire.body);
            let expected = wire.expected_reasoning_text.as_ref().unwrap_or_else(|| {
                panic!("[{who}] {scenario:?}: must supply expected_reasoning_text")
            });
            let reasoning_text = parts.iter().find_map(|part| match part {
                LlmOutputPart::Reasoning { text, .. } => Some(text.clone()),
                _ => None,
            });
            let got = reasoning_text.unwrap_or_else(|| {
                panic!(
                    "[{who}] {scenario:?}: reasoning must surface as an LlmOutputPart::Reasoning, \
                     got {parts:?}"
                )
            });
            assert_eq!(
                &got, expected,
                "[{who}] {scenario:?}: reasoning text must round-trip into the Reasoning part"
            );
            // Reasoning must not also leak into an assistant Text part.
            assert!(
                !parts.iter().any(|part| matches!(
                    part,
                    LlmOutputPart::Text { text, .. } if text == expected
                )),
                "[{who}] {scenario:?}: reasoning text must not also appear as assistant Text"
            );
        }
        Scenario::ReasoningReplayRoundTrip => {
            let sse = wire.reasoning_replay_sse.as_ref().unwrap_or_else(|| {
                panic!("[{who}] {scenario:?}: streaming providers must supply reasoning_replay_sse")
            });
            assert!(
                sse.len() >= 2,
                "[{who}] {scenario:?}: reasoning replay must traverse a meaningful stream of at \
                 least two events, got {}",
                sse.len()
            );
            let parts = n.assemble_stream(scenario, sse).parts;
            let expected = wire.expected_replay_payload.as_deref().unwrap_or_else(|| {
                panic!("[{who}] {scenario:?}: must supply expected_replay_payload")
            });
            let normalized_payload = parts.iter().find_map(reasoning_replay_payload);
            assert_eq!(
                normalized_payload.as_deref(),
                Some(expected),
                "[{who}] {scenario:?}: streaming normalization must attach the opaque replay \
                 payload byte-faithfully to the Reasoning part; got {parts:?}"
            );

            let request = n.build_next_request(standard_next_request_messages(&parts));
            let pointer = wire
                .request_replay_json_pointer
                .as_deref()
                .unwrap_or_else(|| {
                    panic!("[{who}] {scenario:?}: must supply request_replay_json_pointer")
                });
            let request_payload = request.pointer(pointer).and_then(Value::as_str);
            assert_eq!(
                request_payload,
                Some(expected),
                "[{who}] {scenario:?}: normalized replay payload must survive durable history \
                 and reappear byte-faithfully at next-request path {pointer}; request = {request}"
            );
        }
        Scenario::StreamingUsageMerge => {
            let sse = wire
                .usage_merge_sse
                .as_ref()
                .unwrap_or_else(|| panic!("[{who}] {scenario:?}: must supply usage_merge_sse"));
            assert!(
                sse.len() >= 2,
                "[{who}] {scenario:?}: usage must be split across ≥2 events to test merging, got {} \
                 event(s)",
                sse.len()
            );
            let assembled = n.assemble_stream(scenario, sse);
            assert_eq!(
                assembled.usage.input_tokens,
                CanonicalUsage::BASE_INPUT,
                "[{who}] {scenario:?}: input tokens from an early event must survive the merge"
            );
            assert_eq!(
                assembled.usage.output_tokens,
                CanonicalUsage::BASE_OUTPUT,
                "[{who}] {scenario:?}: output tokens from a later event must merge in, not overwrite \
                 the earlier input count"
            );
        }
    }
}

fn reasoning_replay_payload(part: &LlmOutputPart) -> Option<String> {
    let LlmOutputPart::Reasoning {
        replay: Some(replay),
        ..
    } = part
    else {
        return None;
    };
    replay
        .encrypted_content
        .clone()
        .or_else(|| replay.signature.clone())
}

#[allow(clippy::too_many_arguments)]
fn assert_usage(
    n: &dyn ProviderNormalizer,
    wire: &ProviderWire,
    who: &str,
    scenario: Scenario,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
) {
    let usage = n.usage_from_wire(&wire.body);
    let expected = LlmUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_output_tokens: reasoning,
    };
    assert_eq!(
        usage, expected,
        "[{who}] {scenario:?}: usage must normalize to the canonical token counts"
    );
}

fn assert_terminal(
    n: &dyn ProviderNormalizer,
    wire: &ProviderWire,
    parts: &[LlmOutputPart],
    expected: LlmTerminalReason,
    who: &str,
    scenario: Scenario,
) {
    let got = n.terminal_from_wire(&wire.body, parts);
    assert_eq!(
        got, expected,
        "[{who}] {scenario:?}: terminal reason must normalize to {expected:?}"
    );
}

fn is_tool_call(part: &LlmOutputPart) -> bool {
    matches!(part, LlmOutputPart::ToolCall { .. })
}

fn as_tool_call(part: &LlmOutputPart) -> Option<(String, String)> {
    match part {
        LlmOutputPart::ToolCall {
            tool_name,
            input_json,
            ..
        } => Some((tool_name.clone(), input_json.clone())),
        _ => None,
    }
}

fn as_text(part: &LlmOutputPart) -> Option<&str> {
    match part {
        LlmOutputPart::Text { text, .. } => Some(text.as_str()),
        _ => None,
    }
}
