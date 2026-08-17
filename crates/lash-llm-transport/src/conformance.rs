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

use lash_sansio::llm::types::{
    LlmMessage, LlmOutputPart, LlmStreamEvent, LlmTerminalReason, LlmUsage,
};
use lash_sansio::session_model::{Message, MessageRole, Part, render_prompt, shared_parts};
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
    /// A stream prefix that ends immediately after the adapter parses a
    /// complete tool call, before provider terminal evidence. Core synthesizes
    /// a plugin-aborted turn entirely from the events emitted for this prefix;
    /// its tool-call parts must equal the adapter's parsed partial state.
    StreamingToolCallAbortEquivalence,
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
    /// Streamed reasoning items carrying opaque provider replay state. The
    /// scenario normalizes the stream, commits the result to durable history,
    /// projects the next LLM request, and asks the provider to serialize it.
    /// Losing the opaque field is invisible in the response turn itself, but
    /// makes the *next* request invalid or semantically discontinuous because
    /// the provider cannot verify or resume its earlier reasoning item. The
    /// fixture carries ≥2 reasoning items with distinct payloads, so a provider
    /// that preserves the first and drops the rest fails.
    ReasoningReplayRoundTrip,
    /// Streamed tool calls carrying opaque provider replay state (e.g. Gemini's
    /// `functionCall` `thoughtSignature`, the Responses API's `fc_…` item id
    /// that chains a call to its sibling reasoning item). Same obligation as
    /// [`Scenario::ReasoningReplayRoundTrip`], on the tool-call seam: normalize
    /// the stream, commit to durable history, project and serialize the next
    /// request, and find each payload byte-faithfully at its pointer.
    ToolCallReplayRoundTrip,
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
        Scenario::StreamingToolCallAbortEquivalence,
        Scenario::ContentFilter,
        Scenario::UsageCacheHit,
        Scenario::UsageReasoning,
        Scenario::ReasoningExtraction,
        Scenario::ReasoningReplayRoundTrip,
        Scenario::ToolCallReplayRoundTrip,
        Scenario::StreamingUsageMerge,
    ];
}

/// Checked-in inventory of acknowledged conformance gaps. Validation is
/// bidirectional: adding or removing a declaration requires editing this list.
const UNSUPPORTED_DECLARATION_INVENTORY: &[(&str, Scenario)] = &[
    ("anthropic", Scenario::ToolCallReplayRoundTrip),
    ("codex-rlm-history", Scenario::ToolCallReplayRoundTrip),
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

/// One replayed item's opaque payload plus the provider-dialect JSON Pointer
/// where that payload must reappear in the serialized next request. The replay
/// scenarios take an ordered list of these — one per replayed item — so a
/// provider that round-trips the first item and drops later ones fails.
#[derive(Clone, Debug)]
pub struct ReplayItemExpectation {
    /// The exact opaque replay bytes, as the wire string carrying them.
    pub payload: String,
    /// JSON Pointer locating that payload in the serialized next request.
    pub request_json_pointer: String,
    /// The exact JSON value the dialect must place at `request_json_pointer`.
    /// The fixture DECLARES this shape rather than letting the gate infer it
    /// from whatever it finds: a dialect either carries the opaque bytes as a
    /// JSON string (Anthropic's `signature`, Gemini's `thoughtSignature`) or
    /// replays the JSON document those bytes encode (OpenAI Chat Completions
    /// re-emits a whole `reasoning.encrypted` detail object in
    /// `reasoning_details`). Inferring the mode from the found value made the
    /// two indistinguishable whenever the payload is itself JSON text, so a
    /// builder that shipped an object as its JSON-string serialization passed.
    pub replayed_wire_value: Value,
    /// Optional sibling fact — a JSON Pointer and the string it must hold —
    /// proving the payload landed with the item it belongs to. Pointer equality
    /// alone would still pass if a builder emitted both payloads in the right
    /// slots but reordered the calls they sign.
    pub request_association: Option<(String, String)>,
}

impl ReplayItemExpectation {
    /// `replayed_wire_value` is the wire shape the dialect must emit at
    /// `request_json_pointer` — `Value::String(payload)` for dialects that carry
    /// the opaque bytes as text, or the JSON document those bytes encode for
    /// dialects that replay it structurally.
    pub fn new(
        payload: impl Into<String>,
        request_json_pointer: impl Into<String>,
        replayed_wire_value: Value,
    ) -> Self {
        Self {
            payload: payload.into(),
            request_json_pointer: request_json_pointer.into(),
            replayed_wire_value,
            request_association: None,
        }
    }

    /// Pin the identity of the item this payload replays alongside, e.g. the
    /// `call_id` of the tool call the signature belongs to.
    pub fn associated_with(
        mut self,
        request_json_pointer: impl Into<String>,
        expected: impl Into<String>,
    ) -> Self {
        self.request_association = Some((request_json_pointer.into(), expected.into()));
        self
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
    /// For `Scenario::StreamingToolCallAbortEquivalence`: the provider-native
    /// stream prefix through the event that makes one tool call complete, with
    /// no provider terminal event after it.
    pub abort_tool_call_sse: Option<Vec<String>>,
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
    /// ≥2 reasoning items with opaque replay state. Streaming providers must use
    /// this path rather than relying on their batch parser.
    pub reasoning_replay_sse: Option<Vec<String>>,
    /// The reasoning items that stream encodes, in emission order.
    pub reasoning_replay_expectations: Vec<ReplayItemExpectation>,
    /// For `Scenario::ToolCallReplayRoundTrip`: the provider stream carrying
    /// tool calls with opaque replay state.
    pub tool_call_replay_sse: Option<Vec<String>>,
    /// The tool calls that stream encodes, in emission order.
    pub tool_call_replay_expectations: Vec<ReplayItemExpectation>,
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
            abort_tool_call_sse: None,
            text_sse: None,
            expected_stream_text: None,
            expected_tool_input_json: None,
            expected_tool_name: None,
            expected_reasoning_text: None,
            reasoning_replay_sse: None,
            reasoning_replay_expectations: Vec::new(),
            tool_call_replay_sse: None,
            tool_call_replay_expectations: Vec::new(),
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

    pub fn with_aborted_tool_call_stream(
        mut self,
        sse: Vec<String>,
        tool_name: impl Into<String>,
        expected_input_json: Value,
    ) -> Self {
        self.abort_tool_call_sse = Some(sse);
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

    /// Configure the streamed reasoning-replay round-trip fixture and, in
    /// emission order, the provider-dialect request location where each item's
    /// opaque value must reappear.
    pub fn with_reasoning_replay_round_trip(
        mut self,
        sse: Vec<String>,
        expectations: Vec<ReplayItemExpectation>,
    ) -> Self {
        self.reasoning_replay_sse = Some(sse);
        self.reasoning_replay_expectations = expectations;
        self
    }

    /// Configure the streamed tool-call-replay round-trip fixture and, in
    /// emission order, the provider-dialect request location where each call's
    /// opaque value must reappear.
    pub fn with_tool_call_replay_round_trip(
        mut self,
        sse: Vec<String>,
        expectations: Vec<ReplayItemExpectation>,
    ) -> Self {
        self.tool_call_replay_sse = Some(sse);
        self.tool_call_replay_expectations = expectations;
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
    /// Core-internal stream events emitted by the real adapter path while
    /// parsing the supplied provider-native events.
    pub stream_events: Vec<LlmStreamEvent>,
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
    /// request builder. The replay scenarios then inspect the returned wire
    /// value at their provider-specific JSON Pointers.
    ///
    /// `scenario` is supplied so a provider serving several request dialects
    /// (OpenAI speaks both Chat Completions and Responses) builds the dialect
    /// the scenario's fixture belongs to, chosen by scenario identity exactly as
    /// [`ProviderNormalizer::assemble_stream`] chooses its parser.
    fn build_next_request(&self, scenario: Scenario, messages: Vec<LlmMessage>) -> Value;
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
            } => history_parts.push(Part::prose(
                format!("{assistant_id}.p{}", history_parts.len()),
                text.clone(),
                response_meta.clone(),
            )),
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
            } => history_parts.push(Part::tool_call(
                format!("{assistant_id}.p{}", history_parts.len()),
                input_json.clone(),
                call_id.clone(),
                tool_name.clone(),
                replay.clone(),
            )),
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
            parts: shared_parts(vec![Part::text(
                "conformance.user.p0".to_string(),
                "continue".to_string(),
                None,
            )]),
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
        Scenario::StreamingToolCallAbortEquivalence => {
            let sse = wire
                .abort_tool_call_sse
                .as_ref()
                .unwrap_or_else(|| panic!("[{who}] {scenario:?}: must supply abort_tool_call_sse"));
            assert!(
                !sse.is_empty(),
                "[{who}] {scenario:?}: abort prefix must contain provider-native events"
            );
            let assembled = n.assemble_stream(scenario, sse);
            let parsed_tool_calls = assembled
                .parts
                .iter()
                .filter(|part| is_tool_call(part))
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                parsed_tool_calls.len(),
                1,
                "[{who}] {scenario:?}: fixture must end after exactly one parsed tool call, got {:?}",
                assembled.parts
            );

            let aborted = lash_core::testing::response_synthesized_from_aborted_stream(
                &assembled.stream_events,
            );
            let aborted_tool_calls = aborted
                .parts
                .iter()
                .filter(|part| is_tool_call(part))
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                aborted_tool_calls, parsed_tool_calls,
                "[{who}] {scenario:?}: a turn aborted after the adapter parsed its tool call must preserve exactly the same tool-call parts from core's accumulator; emitted events were {:?}",
                assembled.stream_events
            );

            let (tool_name, input_json) = as_tool_call(&aborted_tool_calls[0])
                .expect("aborted response contains one tool call");
            if let Some(expected_name) = &wire.expected_tool_name {
                assert_eq!(&tool_name, expected_name);
            }
            let got_json: Value = serde_json::from_str(&input_json)
                .unwrap_or_else(|err| panic!("[{who}] abort-path tool input is invalid: {err}"));
            assert_eq!(
                Some(&got_json),
                wire.expected_tool_input_json.as_ref(),
                "[{who}] {scenario:?}: abort-path tool input changed"
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
            assert_replay_round_trip(
                n,
                scenario,
                sse,
                &wire.reasoning_replay_expectations,
                // A single reasoning item cannot distinguish "kept the payload"
                // from "kept the first payload and dropped the rest".
                2,
                "Reasoning",
                reasoning_replay_payload,
            );
        }
        Scenario::ToolCallReplayRoundTrip => {
            let sse = wire.tool_call_replay_sse.as_ref().unwrap_or_else(|| {
                panic!("[{who}] {scenario:?}: streaming providers must supply tool_call_replay_sse")
            });
            assert_replay_round_trip(
                n,
                scenario,
                sse,
                &wire.tool_call_replay_expectations,
                // Same floor as reasoning: one call cannot distinguish "kept the
                // payload" from "kept the first and dropped the rest", and every
                // dialect that carries tool-call replay at all can carry two.
                2,
                "ToolCall",
                tool_call_replay_payload,
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

/// The shared round-trip check behind both replay scenarios: normalize the
/// provider stream, assert every item's opaque payload survives normalization
/// in emission order, then project and serialize the next request and assert
/// each payload reappears byte-faithfully at *its own* pointer. Checking one
/// pointer would leave a provider that keeps the first payload and drops the
/// rest passing green.
fn assert_replay_round_trip(
    n: &dyn ProviderNormalizer,
    scenario: Scenario,
    sse: &[String],
    expectations: &[ReplayItemExpectation],
    minimum_items: usize,
    part_kind: &str,
    payload_of: fn(&LlmOutputPart) -> Option<String>,
) {
    let who = n.name();
    assert!(
        sse.len() >= 2,
        "[{who}] {scenario:?}: replay must traverse a meaningful stream of at least two events, \
         got {}",
        sse.len()
    );
    assert!(
        expectations.len() >= minimum_items,
        "[{who}] {scenario:?}: fixture must declare at least {minimum_items} replayed item(s), got \
         {}",
        expectations.len()
    );
    for (index, expectation) in expectations.iter().enumerate() {
        assert_declared_shape_carries_payload(who, scenario, index, expectation);
        assert!(
            expectations[..index]
                .iter()
                .all(|earlier| earlier.payload != expectation.payload),
            "[{who}] {scenario:?}: item {index} repeats an earlier payload; fixture payloads must \
             be distinct or a dropped item is masked by its identical sibling"
        );
    }

    let assembled = n.assemble_stream(scenario, sse);
    let parts = assembled.parts;
    let normalized = parts.iter().filter_map(payload_of).collect::<Vec<_>>();
    let expected = expectations
        .iter()
        .map(|expectation| expectation.payload.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        normalized, expected,
        "[{who}] {scenario:?}: streaming normalization must attach every opaque replay payload \
         byte-faithfully, in order, to its own {part_kind} part; got {parts:?}"
    );

    // Parts the adapter emitted live carry the same obligation: a turn aborted
    // mid-stream commits exactly these, so replay missing here is lost durably
    // even when the end-of-stream assembly would have carried it. An adapter
    // that emits no parts for this fixture is not asserted against.
    let emitted_parts = assembled
        .stream_events
        .iter()
        .filter_map(|event| match event {
            LlmStreamEvent::Part(part) => Some(part),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !emitted_parts.is_empty() {
        let emitted = emitted_parts
            .iter()
            .filter_map(|part| payload_of(part))
            .collect::<Vec<_>>();
        assert_eq!(
            emitted, expected,
            "[{who}] {scenario:?}: parts emitted while streaming must carry the same replay \
             payloads as the assembled {part_kind} parts; got {emitted_parts:?}"
        );
    }

    let request = n.build_next_request(scenario, standard_next_request_messages(&parts));
    for (index, expectation) in expectations.iter().enumerate() {
        let pointer = expectation.request_json_pointer.as_str();
        assert_eq!(
            request.pointer(pointer),
            Some(&expectation.replayed_wire_value),
            "[{who}] {scenario:?}: replay payload for item {index} must survive durable history \
             and reappear at next-request path {pointer} byte-faithfully AND in the wire shape the \
             fixture declares — a matching payload in the wrong JSON shape (a string where the \
             dialect requires a document, or the reverse) is a wire corruption, not a pass; \
             request = {request}"
        );
        if let Some((association_pointer, expected_identity)) = &expectation.request_association {
            let found_identity = request.pointer(association_pointer).and_then(Value::as_str);
            assert_eq!(
                found_identity,
                Some(expected_identity.as_str()),
                "[{who}] {scenario:?}: item {index}'s payload must be replayed alongside the item \
                 it belongs to; {association_pointer} identifies a different item; \
                 request = {request}"
            );
        }
    }
}

/// A fixture's declared wire shape must be the same bytes the replayed part
/// carries: either the payload string itself, or the JSON document that string
/// encodes. This keeps the declaration honest about shape without letting it
/// drift into asserting a payload the part never carried.
fn assert_declared_shape_carries_payload(
    who: &str,
    scenario: Scenario,
    index: usize,
    expectation: &ReplayItemExpectation,
) {
    match &expectation.replayed_wire_value {
        Value::String(declared) => assert_eq!(
            declared, &expectation.payload,
            "[{who}] {scenario:?}: item {index}'s declared wire string must be the replay payload \
             itself"
        ),
        document => {
            let encoded = serde_json::from_str::<Value>(&expectation.payload).ok();
            assert_eq!(
                encoded.as_ref(),
                Some(document),
                "[{who}] {scenario:?}: item {index} declares a JSON document wire shape, so its \
                 replay payload must be exactly the bytes that document encodes"
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

fn tool_call_replay_payload(part: &LlmOutputPart) -> Option<String> {
    let LlmOutputPart::ToolCall {
        replay: Some(replay),
        ..
    } = part
    else {
        return None;
    };
    // Providers carry tool-call replay either as an opaque blob (Gemini's
    // `thoughtSignature`) or as the provider item id that chains the call to
    // its sibling reasoning item (Responses `fc_…`).
    replay.opaque.clone().or_else(|| replay.item_id.clone())
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
