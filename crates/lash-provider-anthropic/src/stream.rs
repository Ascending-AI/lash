//! SSE stream parsing and finalization: accumulate Anthropic's
//! `message_start` / `content_block_*` / `message_delta` events into a
//! [`StreamState`], then collapse it into output parts, usage, and a terminal
//! reason.

use crate::support::*;

#[derive(Clone, Debug, Default)]
pub(crate) struct StreamBlock {
    pub(crate) kind: BlockKind,
    /// Accumulated visible text (for text/thinking blocks).
    pub(crate) text: String,
    /// `signature_delta` payload preserved for thinking blocks so we can
    /// replay them intact on the next turn. Empty for other block types.
    pub(crate) thinking_signature: String,
    /// Streaming buffer for tool_use input JSON.
    pub(crate) input_buffer: String,
    /// tool_use metadata.
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    /// Initial input payload from `content_block_start` for tool_use.
    pub(crate) tool_initial_input: Value,
    /// Flags redacted thinking blocks; signature carries opaque payload.
    pub(crate) redacted: bool,
}

impl StreamBlock {
    fn tool_call_part(&self) -> Option<LlmOutputPart> {
        if self.kind != BlockKind::ToolUse || self.tool_name.is_empty() {
            return None;
        }
        let input_json = if !self.input_buffer.is_empty() {
            self.input_buffer.clone()
        } else if self.tool_initial_input.is_object() {
            serde_json::to_string(&self.tool_initial_input).unwrap_or_else(|_| "{}".to_string())
        } else {
            "{}".to_string()
        };
        Some(LlmOutputPart::ToolCall {
            call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            input_json,
            replay: None,
        })
    }

    fn reasoning_part(&self) -> Option<LlmOutputPart> {
        if self.kind != BlockKind::Thinking
            || (self.text.is_empty() && self.thinking_signature.is_empty())
        {
            return None;
        }
        let replay = (!self.thinking_signature.is_empty()).then(|| ProviderReasoningReplay {
            item_id: None,
            encrypted_content: None,
            signature: Some(self.thinking_signature.clone()),
            redacted: self.redacted,
            summary: Vec::new(),
            origin: None,
        });
        Some(LlmOutputPart::Reasoning {
            text: self.text.clone(),
            replay,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum BlockKind {
    #[default]
    Unknown,
    Text,
    Thinking,
    ToolUse,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StreamState {
    pub(crate) blocks: Vec<StreamBlock>,
    pub(crate) usage: LlmUsage,
    /// Raw provider `usage` JSON sidecar. Anthropic splits it across the wire:
    /// `message_start` carries the full input/cache buckets and `message_delta`
    /// carries the cumulative output counters, so the raw blocks are
    /// shallow-merged rather than last-wins.
    pub(crate) provider_usage: Option<Value>,
    /// Provider-reported identity, served model, reasoning-token presence,
    /// and terminal reason accumulated across split SSE events.
    pub(crate) execution_evidence: Option<ExecutionEvidence>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) message_started: bool,
    pub(crate) message_stopped: bool,
}

/// Overlay the keys of `next` (a raw wire `usage` object) onto the captured
/// sidecar, keeping earlier keys that a later event does not repeat.
fn merge_raw_usage(provider_usage: &mut Option<Value>, next: &Value) {
    match (provider_usage.as_mut().and_then(Value::as_object_mut), next) {
        (Some(existing), Value::Object(next)) => {
            for (key, value) in next {
                existing.insert(key.clone(), value.clone());
            }
        }
        _ => *provider_usage = Some(next.clone()),
    }
}

fn merge_execution_evidence(
    accumulated: &mut Option<ExecutionEvidence>,
    next: ExecutionEvidence,
) -> Result<(), LlmTransportError> {
    ExecutionEvidence::merge_optional(accumulated, Some(next)).map_err(|error| {
        LlmTransportError::new(format!("Anthropic stream {error}"))
            .with_kind(ProviderFailureKind::Stream)
            .with_code(error.code())
    })
}

fn reasoning_output_tokens(usage: &Value) -> Option<u64> {
    usage
        .get("output_tokens_details")
        .and_then(|details| details.get("thinking_tokens"))
        .and_then(Value::as_u64)
}

fn parse_event(raw: &str) -> Option<Value> {
    serde_json::from_str::<Value>(raw).ok()
}

fn is_retryable_error_event(event: &Value) -> bool {
    let error_type = event
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    matches!(
        error_type,
        "overloaded_error" | "api_error" | "rate_limit_error"
    )
}

impl AnthropicProvider {
    pub(crate) fn parse_usage(usage: &Value) -> LlmUsage {
        let input = usage
            .get("input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let cache_write = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let reasoning = usage
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        LlmUsage {
            // Anthropic reports ordinary input, cache reads, and cache
            // creation separately. Lash keeps those billing/context buckets
            // separate too.
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            reasoning_output_tokens: reasoning,
        }
    }

    pub(crate) fn process_sse_event(
        raw: &str,
        state: &mut StreamState,
        stream_events: Option<&LlmEventSender>,
        expose_thinking: bool,
    ) -> Result<(), LlmTransportError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(());
        }
        let event = parse_event(raw).ok_or_else(|| {
            LlmTransportError::new("Invalid Anthropic SSE payload")
                .with_raw(raw.to_string())
                .retryable(false)
        })?;
        let kind = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match kind.as_str() {
            "message_start" => {
                state.message_started = true;
                let message = event.get("message").unwrap_or(&Value::Null);
                let usage = message.get("usage");
                merge_execution_evidence(
                    &mut state.execution_evidence,
                    ExecutionEvidence {
                        served_model: message
                            .get("model")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                        provider_response_id: message
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                        reasoning_output_tokens: usage.and_then(reasoning_output_tokens),
                        ..ExecutionEvidence::default()
                    },
                )?;
                if let Some(usage) = usage {
                    state.usage = Self::parse_usage(usage);
                    merge_raw_usage(&mut state.provider_usage, usage);
                    if let Some(tx) = stream_events
                        && state.usage != LlmUsage::default()
                    {
                        tx.send(LlmStreamEvent::Usage(state.usage.clone()));
                    }
                }
                if let Some(tx) = stream_events {
                    tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                        provider_usage: state.provider_usage.clone(),
                        execution_evidence: state.execution_evidence.clone(),
                        ..Default::default()
                    }));
                }
            }
            "content_block_start" => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                while state.blocks.len() <= index {
                    state.blocks.push(StreamBlock::default());
                }
                let block_meta = event.get("content_block").cloned().unwrap_or_default();
                let block_type = block_meta
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let slot = &mut state.blocks[index];
                match block_type {
                    "text" => {
                        slot.kind = BlockKind::Text;
                    }
                    "thinking" => {
                        slot.kind = BlockKind::Thinking;
                    }
                    "redacted_thinking" => {
                        slot.kind = BlockKind::Thinking;
                        slot.redacted = true;
                        if let Some(data) = block_meta.get("data").and_then(|v| v.as_str()) {
                            slot.thinking_signature = data.to_string();
                        }
                        slot.text = "[Reasoning redacted]".to_string();
                    }
                    "tool_use" => {
                        slot.kind = BlockKind::ToolUse;
                        if let Some(id) = block_meta.get("id").and_then(|v| v.as_str()) {
                            slot.tool_call_id = id.to_string();
                        }
                        if let Some(name) = block_meta.get("name").and_then(|v| v.as_str()) {
                            slot.tool_name = name.to_string();
                        }
                        if let Some(input) = block_meta.get("input") {
                            slot.tool_initial_input = input.clone();
                        }
                    }
                    _ => {
                        slot.kind = BlockKind::Unknown;
                    }
                }
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if index >= state.blocks.len() {
                    return Ok(());
                }
                let delta = event.get("delta").cloned().unwrap_or_default();
                let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let slot = &mut state.blocks[index];
                match delta_type {
                    "text_delta" => {
                        let piece = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !piece.is_empty() {
                            slot.text.push_str(piece);
                            if let Some(tx) = stream_events {
                                tx.send(LlmStreamEvent::Delta(piece.to_string()));
                            }
                        }
                    }
                    "thinking_delta" => {
                        let piece = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                        if !piece.is_empty() {
                            slot.text.push_str(piece);
                            if let Some(tx) = stream_events
                                && expose_thinking
                            {
                                tx.send(LlmStreamEvent::ReasoningDelta(piece.to_string()));
                            }
                        }
                    }
                    "signature_delta" => {
                        let piece = delta
                            .get("signature")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !piece.is_empty() {
                            slot.thinking_signature.push_str(piece);
                        }
                    }
                    "input_json_delta" => {
                        let piece = delta
                            .get("partial_json")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !piece.is_empty() {
                            slot.input_buffer.push_str(piece);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(tx) = stream_events
                    && let Some(part) = state
                        .blocks
                        .get(index)
                        .and_then(|block| block.tool_call_part().or_else(|| block.reasoning_part()))
                {
                    tx.send(LlmStreamEvent::Part(part));
                }
            }
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    merge_raw_usage(&mut state.provider_usage, usage);
                    let new_usage = Self::parse_usage(usage);
                    let mut merged = state.usage.clone();
                    merge_usage(&mut merged, &new_usage);
                    if merged != state.usage {
                        state.usage = merged;
                        if let Some(tx) = stream_events {
                            tx.send(LlmStreamEvent::Usage(state.usage.clone()));
                        }
                    }
                    merge_execution_evidence(
                        &mut state.execution_evidence,
                        ExecutionEvidence {
                            reasoning_output_tokens: reasoning_output_tokens(usage),
                            ..ExecutionEvidence::default()
                        },
                    )?;
                }
                if let Some(stop) = event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str())
                {
                    state.stop_reason = Some(stop.to_string());
                    merge_execution_evidence(
                        &mut state.execution_evidence,
                        ExecutionEvidence {
                            provider_finish_reason: Some(stop.to_string()),
                            ..ExecutionEvidence::default()
                        },
                    )?;
                }
                if let Some(tx) = stream_events {
                    tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                        provider_usage: state.provider_usage.clone(),
                        execution_evidence: state.execution_evidence.clone(),
                        ..Default::default()
                    }));
                }
            }
            "message_stop" => {
                if state.message_started {
                    state.message_stopped = true;
                }
            }
            "ping" => {}
            "error" => {
                let msg = event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Anthropic stream error")
                    .to_string();
                return Err(
                    LlmTransportError::new(format!("Anthropic stream error: {msg}"))
                        .with_raw(raw.to_string())
                        .retryable(is_retryable_error_event(&event)),
                );
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finalize(
        state: StreamState,
        _origin_model: &str,
    ) -> (Vec<LlmOutputPart>, String, LlmUsage, LlmTerminalReason) {
        let mut parts: Vec<LlmOutputPart> = Vec::new();
        let mut full_text = String::new();
        let stop_reason = state.stop_reason.clone();
        for block in state.blocks {
            match block.kind {
                BlockKind::Text => {
                    if !block.text.is_empty() {
                        full_text.push_str(&block.text);
                        parts.push(LlmOutputPart::Text {
                            text: block.text,
                            response_meta: None,
                        });
                    }
                }
                BlockKind::Thinking => {
                    if let Some(part) = block.reasoning_part() {
                        parts.push(part);
                    }
                }
                BlockKind::ToolUse => {
                    if let Some(part) = block.tool_call_part() {
                        parts.push(part);
                    }
                }
                BlockKind::Unknown => {}
            }
        }
        let terminal_reason = match stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence") => LlmTerminalReason::Stop,
            Some("tool_use") => LlmTerminalReason::ToolUse,
            Some("max_tokens") => LlmTerminalReason::OutputLimit,
            Some("pause_turn") => LlmTerminalReason::Stop,
            Some("refusal" | "safety" | "sensitive") => LlmTerminalReason::ContentFilter,
            Some(_) => LlmTerminalReason::ProviderError,
            None => terminal_reason_from_parts(&parts),
        };
        (parts, full_text, state.usage, terminal_reason)
    }
}
