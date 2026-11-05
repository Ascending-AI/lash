//! Streaming and response parsing: extract usage, text, and tool-call parts
//! from Cloud Code (Gemini) events, accumulate streamed text, and map the
//! `finishReason` to a normalized terminal reason.

use crate::support::*;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

#[derive(Default)]
enum OpenStreamRun {
    #[default]
    None,
    Text {
        block: StreamBlockIdentity,
        text: String,
    },
    Reasoning {
        block: StreamBlockIdentity,
        text: String,
        replay: ProviderReasoningReplay,
    },
}

impl GoogleOAuthProvider {
    pub(crate) fn usage_from_event(event: &Value) -> LlmUsage {
        let meta = event
            .get("response")
            .and_then(|r| r.get("usageMetadata"))
            .unwrap_or(&Value::Null);
        let prompt_tokens = parse_i64(
            meta.get("promptTokenCount")
                .or_else(|| meta.get("inputTokenCount"))
                .or_else(|| meta.get("inputTokens")),
        );
        let cache_read = parse_i64(
            meta.get("cachedContentTokenCount")
                .or_else(|| meta.get("cachedPromptTokenCount"))
                .or_else(|| meta.get("cachedInputTokenCount")),
        );
        let candidate_tokens = parse_i64(
            meta.get("candidatesTokenCount")
                .or_else(|| meta.get("outputTokenCount"))
                .or_else(|| meta.get("outputTokens")),
        );
        let reasoning = parse_i64(
            meta.get("thoughtsTokenCount")
                .or_else(|| meta.get("reasoningTokenCount"))
                .or_else(|| meta.get("reasoningTokens")),
        );
        LlmUsage {
            input_tokens: prompt_tokens.saturating_sub(cache_read).max(0),
            output_tokens: candidate_tokens.saturating_add(reasoning),
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: reasoning,
            unknown_calls: 0,
        }
    }

    pub(crate) fn execution_evidence_from_value(value: &Value) -> Option<ExecutionEvidence> {
        let response = value.get("response").unwrap_or(value);
        let usage = response.get("usageMetadata").unwrap_or(&Value::Null);
        let evidence = ExecutionEvidence {
            served_model: response
                .get("modelVersion")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            provider_response_id: response
                .get("responseId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            reasoning_output_tokens: usage
                .get("thoughtsTokenCount")
                .or_else(|| usage.get("reasoningTokenCount"))
                .or_else(|| usage.get("reasoningTokens"))
                .and_then(Value::as_u64),
            provider_finish_reason: Self::finish_reason_str(value).map(str::to_string),
            ..ExecutionEvidence::default()
        };
        (evidence != ExecutionEvidence::default()).then_some(evidence)
    }

    /// Every candidate part of one streamed event, in wire order.
    fn wire_parts(event: &Value) -> impl Iterator<Item = &Value> {
        event
            .get("response")
            .and_then(|r| r.get("candidates"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|candidate| {
                candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(Value::as_array)
            })
            .flatten()
    }

    /// The text or thought piece one streamed part carries, as
    /// `(text, signature, is_thought)`.
    fn text_piece(part: &Value) -> Option<(String, Option<String>, bool)> {
        let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
        let is_thought = part.get("thought").and_then(Value::as_bool) == Some(true);
        let signature = Self::thought_signature(part);
        // A thought part may carry only a `thoughtSignature` (no text
        // or empty text). Keep it so the replay payload survives; the
        // reasoning accumulator turns it into an empty-text part. A
        // `functionCall` part is excluded: `tool_call_part` already
        // carries that signature on the ToolCall replay, so keeping it
        // here too would duplicate the signature across two parts.
        let signature_only_thought =
            is_thought && signature.is_some() && part.get("functionCall").is_none();
        if text.is_empty() && !signature_only_thought {
            return None;
        }
        Some((text.to_string(), signature, is_thought))
    }

    fn thought_signature(part: &Value) -> Option<String> {
        part.get("thoughtSignature")
            .and_then(Value::as_str)
            .filter(|signature| !signature.is_empty())
            .map(str::to_string)
    }

    fn tool_call_part(&self, part: &Value, origin_model: Option<&str>) -> Option<LlmOutputPart> {
        let function_call = part.get("functionCall")?;
        let name = function_call.get("name").and_then(Value::as_str)?;
        let input_json = function_call
            .get("args")
            .map(Value::to_string)
            .unwrap_or_else(|| "{}".to_string());
        Some(LlmOutputPart::ToolCall {
            call_id: function_call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default(),
            tool_name: name.to_string(),
            input_json,
            replay: Self::thought_signature(part).map(|opaque| ProviderReplayMeta {
                item_id: None,
                opaque: Some(opaque),
                origin: origin_model.map(|model| self.route_identity_for_model(model)),
            }),
        })
    }

    fn reasoning_replay(
        &self,
        signature: Option<String>,
        origin_model: Option<&str>,
    ) -> Option<ProviderReasoningReplay> {
        signature.map(|signature| ProviderReasoningReplay {
            item_id: None,
            encrypted_content: None,
            signature: Some(signature),
            redacted: false,
            summary: Vec::new(),
            origin: origin_model.map(|model| self.route_identity_for_model(model)),
        })
    }

    /// The non-empty `finishReason` carried by the first candidate of an event,
    /// honouring the streaming `response.candidates` wrapper as well as the
    /// unwrapped top-level shape.
    fn finish_reason_str(value: &Value) -> Option<&str> {
        value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("finishReason"))
            .or_else(|| {
                value
                    .get("response")
                    .and_then(|response| response.get("candidates"))
                    .and_then(Value::as_array)
                    .and_then(|candidates| candidates.first())
                    .and_then(|candidate| candidate.get("finishReason"))
            })
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
    }

    pub(crate) fn response_parts_from_value(
        &self,
        value: &Value,
        origin_model: Option<&str>,
    ) -> Vec<LlmOutputPart> {
        let mut parts = Vec::new();
        let Some(candidates) = value.get("candidates").and_then(|c| c.as_array()) else {
            return parts;
        };
        for candidate in candidates {
            let Some(items) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            else {
                continue;
            };
            for item in items {
                let signature = Self::thought_signature(item);
                let is_thought = item
                    .get("thought")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                // Same exclusion as the streaming extractor: a signature on a
                // `functionCall` part belongs to the ToolCall replay only.
                let signature_only_thought =
                    signature.is_some() && item.get("functionCall").is_none();
                if is_thought && (!text.is_empty() || signature_only_thought) {
                    // Gemini flags reasoning text with `thought: true`.
                    // Route those into Reasoning so downstream code
                    // doesn't show them as assistant prose. Signature
                    // lives on the same part, and a signature-only part
                    // (no text or empty text) still has to survive so the
                    // replay payload reaches the next request.
                    parts.push(LlmOutputPart::Reasoning {
                        text: text.to_string(),
                        replay: self.reasoning_replay(signature.clone(), origin_model),
                    });
                } else if !text.is_empty() {
                    parts.push(LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: signature.clone().map(|signature| ResponseTextMeta {
                            provider_payload: Some(signature),
                            origin: origin_model.map(|model| self.route_identity_for_model(model)),
                            ..ResponseTextMeta::default()
                        }),
                    });
                }
                if let Some(tool_call) = self.tool_call_part(item, origin_model) {
                    parts.push(tool_call);
                }
            }
        }
        parts
    }

    pub(crate) fn terminal_reason_from_value(
        value: &Value,
        parts: &[LlmOutputPart],
    ) -> LlmTerminalReason {
        let finish = Self::finish_reason_str(value).unwrap_or("");
        match finish {
            // Gemini uses STOP for both prose completion and a completed
            // function-call candidate. Preserve the structured outcome when
            // the payload actually contains a tool call.
            "STOP" => terminal_reason_from_parts(parts),
            "MAX_TOKENS" => LlmTerminalReason::OutputLimit,
            "SAFETY"
            | "RECITATION"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "IMAGE_SAFETY"
            | "IMAGE_PROHIBITED_CONTENT"
            | "IMAGE_RECITATION"
            | "IMAGE_OTHER"
            | "LANGUAGE" => LlmTerminalReason::ContentFilter,
            "MALFORMED_FUNCTION_CALL"
            | "UNEXPECTED_TOOL_CALL"
            | "FINISH_REASON_UNSPECIFIED"
            | "OTHER"
            | "NO_IMAGE" => LlmTerminalReason::ProviderError,
            "" => terminal_reason_from_parts(parts),
            _ => LlmTerminalReason::ProviderError,
        }
    }
}

/// One Cloud Code SSE response and its mutually exclusive open text or reasoning run.
/// Events, partial responses and final responses use the same ordered parts.
#[derive(Default)]
pub(crate) struct GoogleStreamState {
    pub(crate) stream_observer: Option<lash_protocol_types::llm::types::ProviderAttemptControl>,
    pub full: String,
    pub usage: LlmUsage,
    pub provider_usage: Option<Value>,
    pub execution_evidence: Option<ExecutionEvidence>,
    /// Text, reasoning and tool-call parts in wire order: the one sequence the
    /// completed response, a partial response and the live stream all follow.
    output_parts: Vec<LlmOutputPart>,
    pub finish_event: Option<Value>,
    open_run: OpenStreamRun,
    next_block_ordinal: u64,
    /// Stamped from the request's recorded `expose_thinking` default at state construction
    /// so the assembled `LlmResponse` carries the visibility policy forward
    /// for the runtime's reasoning republication gate.
    pub expose_thinking: bool,
}

/// What one [`GoogleStreamState::push_event`] call produced: the event's own
/// visible and reasoning deltas, and its stream emissions in wire order (text
/// and reasoning block events, reasoning `Part` closes and tool-call `Part`s).
/// Publication filters reasoning emissions with [`is_reasoning_emission`].
#[derive(Default)]
pub(crate) struct EventDeltas {
    pub text_deltas: Vec<String>,
    pub reasoning_deltas: Vec<String>,
    pub events: Vec<LlmStreamEvent>,
}

/// Whether a stream emission belongs to a reasoning block, which is published
/// only when the request exposes thinking.
pub(crate) fn is_reasoning_emission(event: &LlmStreamEvent) -> bool {
    match event {
        LlmStreamEvent::Block(
            StreamBlockEvent::Started { kind, .. }
            | StreamBlockEvent::Delta { kind, .. }
            | StreamBlockEvent::Completed { kind, .. },
        ) => *kind == StreamBlockKind::Reasoning,
        LlmStreamEvent::Part(part) => matches!(part, LlmOutputPart::Reasoning { .. }),
        _ => false,
    }
}

impl GoogleStreamState {
    /// Fold one raw SSE payload into the stream state, returning just this
    /// event's deltas. `provider` supplies route-identity stamping on replay
    /// metadata; `origin_model` is the request's model.
    pub(crate) fn push_event(
        &mut self,
        provider: &GoogleOAuthProvider,
        raw: &str,
        origin_model: Option<&str>,
    ) -> Result<EventDeltas, LlmTransportError> {
        let mut deltas = EventDeltas::default();
        if raw.trim().is_empty() || raw.trim() == "[DONE]" {
            return Ok(deltas);
        }
        let event: Value = serde_json::from_str(raw).map_err(|e| {
            LlmTransportError::new(format!("Invalid Cloud Code SSE payload: {e}"))
                .with_retry_verdict(TransportRetryVerdict::NotRetryable)
        })?;
        if let Some(observer) = &self.stream_observer {
            use lash_transport_failure::PartialOutputKind as K;
            let mut kinds = std::collections::BTreeSet::new();
            for part in GoogleOAuthProvider::wire_parts(&event) {
                if part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
                    || part.get("thoughtSignature").is_some()
                {
                    if part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                    {
                        kinds.insert(
                            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                                K::Reasoning
                            } else {
                                K::Text
                            },
                        );
                    }
                    if part.get("thoughtSignature").is_some() {
                        kinds.insert(K::Reasoning);
                    }
                }
                if part.get("functionCall").is_some() {
                    kinds.insert(K::ToolArguments);
                }
            }
            observer.observe_output_kinds(kinds);
        }
        ExecutionEvidence::merge_optional(
            &mut self.execution_evidence,
            GoogleOAuthProvider::execution_evidence_from_value(&event),
        )
        .map_err(|error| {
            LlmTransportError::new(format!("Google stream {error}"))
                .with_kind(ProviderFailureKind::Stream)
                .with_lash_code(TurnFailureCode::from_wire(error.code()))
        })?;
        let new_usage = GoogleOAuthProvider::usage_from_event(&event);
        if new_usage.input_tokens > 0
            || new_usage.output_tokens > 0
            || new_usage.cache_read_input_tokens > 0
            || new_usage.cache_write_input_tokens > 0
            || new_usage.reasoning_output_tokens > 0
        {
            self.usage = new_usage;
            // Keep the raw `usageMetadata` block alongside the normalized
            // counters, under the same non-zero guard so a trailing empty
            // block cannot clobber the captured sidecar.
            self.provider_usage = event
                .get("response")
                .and_then(|response| response.get("usageMetadata"))
                .cloned();
        }
        let mut saw_thought_in_event = false;
        for part in GoogleOAuthProvider::wire_parts(&event) {
            if let Some(piece) = GoogleOAuthProvider::text_piece(part) {
                self.push_text_piece(
                    provider,
                    piece,
                    &mut saw_thought_in_event,
                    &mut deltas,
                    origin_model,
                );
            }
            if let Some(tool_call) = provider.tool_call_part(part, origin_model) {
                self.close_run(&mut deltas.events);
                deltas.events.push(LlmStreamEvent::Part(tool_call.clone()));
                self.output_parts.push(tool_call);
            }
        }
        // Capture the last event carrying a non-empty `finishReason` so the
        // streaming finalizer can derive the terminal reason exactly like the
        // non-streaming path instead of hardcoding Stop.
        if GoogleOAuthProvider::finish_reason_str(&event).is_some() {
            if matches!(self.open_run, OpenStreamRun::Reasoning { .. }) {
                self.close_run(&mut deltas.events);
            }
            self.finish_event = Some(event);
        }
        Ok(deltas)
    }

    /// Fold one text or thought piece into the open reasoning or text run.
    fn push_text_piece(
        &mut self,
        provider: &GoogleOAuthProvider,
        (piece, signature, is_thought): (String, Option<String>, bool),
        saw_thought_in_event: &mut bool,
        deltas: &mut EventDeltas,
        origin_model: Option<&str>,
    ) {
        if is_thought {
            let incoming = provider.reasoning_replay(signature, origin_model);
            let continue_run = !*saw_thought_in_event
                && match &self.open_run {
                    OpenStreamRun::Reasoning { replay, .. } => !matches!(
                        (replay.signature.as_deref(), incoming.as_ref().and_then(|r| r.signature.as_deref())),
                        (Some(existing), Some(next)) if existing != next
                    ),
                    _ => false,
                };
            if !continue_run {
                self.close_run(&mut deltas.events);
                let mut block = self.mint_block("reasoning");
                block.item_id = Some(block.id.clone());
                let mut replay = incoming.clone().unwrap_or_default();
                replay.item_id = block.item_id.clone();
                deltas
                    .events
                    .push(LlmStreamEvent::Block(StreamBlockEvent::Started {
                        kind: StreamBlockKind::Reasoning,
                        block: block.clone(),
                    }));
                self.open_run = OpenStreamRun::Reasoning {
                    block,
                    text: String::new(),
                    replay,
                };
            }
            if let OpenStreamRun::Reasoning {
                block,
                text,
                replay,
            } = &mut self.open_run
            {
                let delta = if continue_run && piece.starts_with(text.as_str()) {
                    piece[text.len()..].to_string()
                } else if continue_run && text.starts_with(&piece) {
                    String::new()
                } else {
                    piece
                };
                if !delta.is_empty() {
                    text.push_str(&delta);
                    deltas.reasoning_deltas.push(delta.clone());
                    deltas
                        .events
                        .push(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                            kind: StreamBlockKind::Reasoning,
                            block: block.clone(),
                            text: delta,
                        }));
                }
                if let Some(incoming) = incoming {
                    if replay.signature.is_none() {
                        replay.signature = incoming.signature;
                    }
                    if replay.origin.is_none() {
                        replay.origin = incoming.origin;
                    }
                }
            }
            *saw_thought_in_event = true;
            return;
        }
        if matches!(self.open_run, OpenStreamRun::Reasoning { .. }) {
            self.close_run(&mut deltas.events);
        }
        let Some(delta) = Self::apply_stream_piece(&mut self.full, &mut deltas.text_deltas, &piece)
        else {
            return;
        };
        if matches!(self.open_run, OpenStreamRun::None) {
            let block = self.mint_block("text");
            deltas
                .events
                .push(LlmStreamEvent::Block(StreamBlockEvent::Started {
                    kind: StreamBlockKind::AssistantText,
                    block: block.clone(),
                }));
            self.open_run = OpenStreamRun::Text {
                block,
                text: String::new(),
            };
        }
        if let OpenStreamRun::Text { block, text } = &mut self.open_run {
            text.push_str(&delta);
            deltas
                .events
                .push(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: block.clone(),
                    text: delta.clone(),
                }));
        }
        self.output_parts.push(LlmOutputPart::Text {
            text: delta,
            response_meta: signature.map(|signature| ResponseTextMeta {
                provider_payload: Some(signature),
                origin: origin_model.map(|model| provider.route_identity_for_model(model)),
                ..ResponseTextMeta::default()
            }),
        });
    }

    fn mint_block(&mut self, prefix: &str) -> StreamBlockIdentity {
        let ordinal = self.next_block_ordinal;
        self.next_block_ordinal += 1;
        StreamBlockIdentity::new(format!("{prefix}:{ordinal}"), ordinal)
    }

    /// Include the open reasoning run in partial responses without giving up its owner.
    pub(crate) fn output_parts(&self) -> Vec<LlmOutputPart> {
        let mut parts = self.output_parts.clone();
        if let OpenStreamRun::Reasoning { text, replay, .. } = &self.open_run {
            parts.push(LlmOutputPart::Reasoning {
                text: text.clone(),
                replay: Some(replay.clone()),
            });
        }
        parts
    }

    pub(crate) fn finish_run(&mut self) -> Vec<LlmStreamEvent> {
        let mut events = Vec::new();
        self.close_run(&mut events);
        events
    }

    fn close_run(&mut self, events: &mut Vec<LlmStreamEvent>) {
        match std::mem::take(&mut self.open_run) {
            OpenStreamRun::None => {}
            OpenStreamRun::Text { block, text } => {
                events.push(LlmStreamEvent::Block(StreamBlockEvent::Completed {
                    kind: StreamBlockKind::AssistantText,
                    block,
                    text,
                }));
            }
            OpenStreamRun::Reasoning {
                block,
                text,
                replay,
            } => {
                events.push(LlmStreamEvent::Block(StreamBlockEvent::Completed {
                    kind: StreamBlockKind::Reasoning,
                    block,
                    text: text.clone(),
                }));
                let part = LlmOutputPart::Reasoning {
                    text,
                    replay: Some(replay),
                };
                events.push(LlmStreamEvent::Part(part.clone()));
                self.output_parts.push(part);
            }
        }
    }

    fn apply_stream_piece(
        full: &mut String,
        text_deltas: &mut Vec<String>,
        piece: &str,
    ) -> Option<String> {
        if piece.is_empty() {
            return None;
        }
        if piece.starts_with(full.as_str()) {
            let delta = &piece[full.len()..];
            if !delta.is_empty() {
                full.push_str(delta);
                text_deltas.push(delta.to_string());
                return Some(delta.to_string());
            }
            return None;
        }
        full.push_str(piece);
        text_deltas.push(piece.to_string());
        Some(piece.to_string())
    }
}
