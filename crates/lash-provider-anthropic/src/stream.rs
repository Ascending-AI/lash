//! SSE stream parsing and finalization: accumulate Anthropic's
//! `message_start` / `content_block_*` / `message_delta` events into a
//! [`StreamState`], then collapse it into output parts, usage, and a terminal
//! reason.

use crate::support::*;

/// One `content_block_*` slot, keyed by the block type announced at
/// `content_block_start`. Each variant carries only the state its deltas can
/// legally write; a delta that does not match the slot's kind is a stream
/// error. `Unknown` keeps a slot for block types we do not model so later
/// indexes still align.
#[derive(Clone, Debug)]
pub(crate) enum StreamBlock {
    Text {
        /// Accumulated visible text.
        text: String,
    },
    Thinking {
        text: String,
        /// `signature_delta` payload preserved so the block replays intact on
        /// the next turn.
        signature: String,
    },
    /// `signature` carries the opaque `data` payload announced at block start;
    /// `text` holds the fixed redacted placeholder.
    RedactedThinking {
        text: String,
        signature: String,
    },
    ToolUse {
        /// Streaming buffer for `input_json_delta` partial JSON.
        input_buffer: String,
        call_id: String,
        name: String,
        /// Initial `input` payload from `content_block_start`.
        initial_input: Value,
    },
    Unknown,
}

impl StreamBlock {
    fn tool_call_part(&self) -> Option<LlmOutputPart> {
        let Self::ToolUse {
            input_buffer,
            call_id,
            name,
            initial_input,
        } = self
        else {
            return None;
        };
        if name.is_empty() {
            return None;
        }
        let input_json = if !input_buffer.is_empty() {
            input_buffer.clone()
        } else if initial_input.is_object() {
            serde_json::to_string(initial_input).unwrap_or_else(|_| "{}".to_string())
        } else {
            "{}".to_string()
        };
        Some(LlmOutputPart::ToolCall {
            call_id: call_id.clone(),
            tool_name: name.clone(),
            input_json,
            replay: None,
        })
    }

    fn reasoning_part(&self, index: usize) -> Option<LlmOutputPart> {
        let (text, signature, redacted) = match self {
            Self::Thinking { text, signature } => (text, signature, false),
            Self::RedactedThinking { text, signature } => (text, signature, true),
            _ => return None,
        };
        if text.is_empty() && signature.is_empty() {
            return None;
        }
        let replay = (!signature.is_empty()).then(|| ProviderReasoningReplay {
            // The content block is the provider's reasoning item; indexing it
            // lets the runtime fold the streamed block back into this part.
            item_id: Some(format!("content_block:{index}")),
            encrypted_content: None,
            signature: Some(signature.clone()),
            redacted,
            summary: Vec::new(),
            origin: None,
        });
        Some(LlmOutputPart::Reasoning {
            text: text.clone(),
            replay,
        })
    }
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
            .with_adapter_code(TurnFailureCode::from_wire(error.code()))
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

fn retry_verdict_for_error_event(event: &Value) -> TransportRetryVerdict {
    let error_type = event
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match error_type {
        "rate_limit_error" | "overloaded_error" => {
            TransportRetryVerdict::RetryableThrottle { retry_after: None }
        }
        "api_error" => TransportRetryVerdict::RetryableTransient,
        "authentication_error" | "permission_error" | "invalid_request_error" => {
            TransportRetryVerdict::Forbidden
        }
        _ => TransportRetryVerdict::NotRetryable,
    }
}

impl AnthropicProvider {
    /// Anthropic's native block identity is the content-block index; it is
    /// both the block's id and the item id its replay material attaches to.
    fn block_identity(index: usize, block_id: &str) -> StreamBlockIdentity {
        StreamBlockIdentity::new(block_id.to_string(), index as u64)
            .with_item_id(Some(block_id.to_string()))
    }

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
                .with_retry_verdict(TransportRetryVerdict::NotRetryable)
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
                    state.blocks.push(StreamBlock::Unknown);
                }
                // Anthropic's native block identity is the content-block
                // index; it is both the block id and the item the block's
                // replay material belongs to.
                let block_id = format!("content_block:{index}");
                let block_meta = event.get("content_block").cloned().unwrap_or_default();
                let block_type = block_meta
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let slot = &mut state.blocks[index];
                match block_type {
                    "text" => {
                        *slot = StreamBlock::Text {
                            text: String::new(),
                        };
                        if let Some(tx) = stream_events {
                            tx.send(LlmStreamEvent::TextBlockStart {
                                block: Self::block_identity(index, &block_id),
                            });
                        }
                    }
                    "thinking" => {
                        *slot = StreamBlock::Thinking {
                            text: String::new(),
                            signature: String::new(),
                        };
                        if let Some(tx) = stream_events
                            && expose_thinking
                        {
                            tx.send(LlmStreamEvent::ReasoningBlockStart {
                                block: Self::block_identity(index, &block_id),
                            });
                        }
                    }
                    "redacted_thinking" => {
                        *slot = StreamBlock::RedactedThinking {
                            text: "[Reasoning redacted]".to_string(),
                            signature: block_meta
                                .get("data")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        };
                        if let Some(tx) = stream_events
                            && expose_thinking
                        {
                            tx.send(LlmStreamEvent::ReasoningBlockStart {
                                block: Self::block_identity(index, &block_id),
                            });
                        }
                    }
                    "tool_use" => {
                        *slot = StreamBlock::ToolUse {
                            input_buffer: String::new(),
                            call_id: block_meta
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            name: block_meta
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            initial_input: block_meta.get("input").cloned().unwrap_or(Value::Null),
                        };
                    }
                    _ => {
                        *slot = StreamBlock::Unknown;
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
                match (delta_type, slot) {
                    ("text_delta", StreamBlock::Text { text }) => {
                        let piece = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !piece.is_empty() {
                            text.push_str(piece);
                            if let Some(tx) = stream_events {
                                tx.send(LlmStreamEvent::Delta {
                                    block: Self::block_identity(
                                        index,
                                        &format!("content_block:{index}"),
                                    ),
                                    text: piece.to_string(),
                                });
                            }
                        }
                    }
                    (
                        "thinking_delta",
                        StreamBlock::Thinking { text, .. }
                        | StreamBlock::RedactedThinking { text, .. },
                    ) => {
                        let piece = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                        if !piece.is_empty() {
                            text.push_str(piece);
                            if let Some(tx) = stream_events
                                && expose_thinking
                            {
                                tx.send(LlmStreamEvent::ReasoningDelta {
                                    block: Self::block_identity(
                                        index,
                                        &format!("content_block:{index}"),
                                    ),
                                    text: piece.to_string(),
                                });
                            }
                        }
                    }
                    (
                        "signature_delta",
                        StreamBlock::Thinking { signature, .. }
                        | StreamBlock::RedactedThinking { signature, .. },
                    ) => {
                        let piece = delta
                            .get("signature")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !piece.is_empty() {
                            signature.push_str(piece);
                        }
                    }
                    ("input_json_delta", StreamBlock::ToolUse { input_buffer, .. }) => {
                        let piece = delta
                            .get("partial_json")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !piece.is_empty() {
                            input_buffer.push_str(piece);
                        }
                    }
                    (
                        "text_delta" | "thinking_delta" | "signature_delta" | "input_json_delta",
                        StreamBlock::Unknown,
                    ) => {}
                    (
                        "text_delta" | "thinking_delta" | "signature_delta" | "input_json_delta",
                        _,
                    ) => {
                        return Err(LlmTransportError::new(format!(
                            "Anthropic stream delta `{delta_type}` does not match content block {index}"
                        ))
                        .with_raw(raw.to_string())
                        .with_kind(ProviderFailureKind::Stream)
                        .with_retry_verdict(TransportRetryVerdict::NotRetryable));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let block_id = format!("content_block:{index}");
                if let Some(tx) = stream_events {
                    match state.blocks.get(index) {
                        Some(StreamBlock::Text { text }) => {
                            tx.send(LlmStreamEvent::TextBlockEnd {
                                block: Self::block_identity(index, &block_id),
                                text: text.clone(),
                            });
                        }
                        Some(
                            StreamBlock::Thinking { text, .. }
                            | StreamBlock::RedactedThinking { text, .. },
                        ) if expose_thinking => {
                            tx.send(LlmStreamEvent::ReasoningBlockEnd {
                                block: Self::block_identity(index, &block_id),
                                text: text.clone(),
                            });
                        }
                        _ => {}
                    }
                    if let Some(part) = state.blocks.get(index).and_then(|block| {
                        block
                            .tool_call_part()
                            .or_else(|| block.reasoning_part(index))
                    }) {
                        tx.send(LlmStreamEvent::Part(part));
                    }
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
                        .with_retry_verdict(retry_verdict_for_error_event(&event)),
                );
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finalize(
        state: StreamState,
        _origin_model: &str,
    ) -> (Vec<LlmOutputPart>, LlmUsage, LlmTerminalReason) {
        let mut parts: Vec<LlmOutputPart> = Vec::new();
        let stop_reason = state.stop_reason.clone();
        for (index, block) in state.blocks.into_iter().enumerate() {
            match block {
                StreamBlock::Text { text } => {
                    if !text.is_empty() {
                        parts.push(LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        });
                    }
                }
                block @ (StreamBlock::Thinking { .. } | StreamBlock::RedactedThinking { .. }) => {
                    if let Some(part) = block.reasoning_part(index) {
                        parts.push(part);
                    }
                }
                block @ StreamBlock::ToolUse { .. } => {
                    if let Some(part) = block.tool_call_part() {
                        parts.push(part);
                    }
                }
                StreamBlock::Unknown => {}
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
        (parts, state.usage, terminal_reason)
    }
}

#[cfg(test)]
mod retry_verdict_tests {
    use super::*;

    #[test]
    fn capacity_errors_are_throttles_and_api_errors_are_transient() {
        assert_eq!(
            retry_verdict_for_error_event(
                &serde_json::json!({"error": {"type": "overloaded_error"}})
            ),
            TransportRetryVerdict::RetryableThrottle { retry_after: None }
        );
        assert_eq!(
            retry_verdict_for_error_event(&serde_json::json!({"error": {"type": "api_error"}})),
            TransportRetryVerdict::RetryableTransient
        );
    }
}
