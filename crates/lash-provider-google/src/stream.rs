//! Streaming and response parsing: extract usage, text, and tool-call parts
//! from Cloud Code (Gemini) events, accumulate streamed text, and map the
//! `finishReason` to a normalized terminal reason.

use crate::support::*;

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

    fn text_parts_from_event(event: &Value) -> Vec<(String, Option<String>, bool)> {
        let mut out = Vec::new();
        let Some(candidates) = event
            .get("response")
            .and_then(|r| r.get("candidates"))
            .and_then(|c| c.as_array())
        else {
            return out;
        };

        for candidate in candidates {
            let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            else {
                continue;
            };
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    out.push((
                        text.to_string(),
                        Self::thought_signature(part),
                        part.get("thought").and_then(Value::as_bool) == Some(true),
                    ));
                }
            }
        }

        out
    }

    fn tool_call_parts_from_event(event: &Value, origin_model: Option<&str>) -> Vec<LlmOutputPart> {
        let mut out = Vec::new();
        let Some(candidates) = event
            .get("response")
            .and_then(|r| r.get("candidates"))
            .and_then(|c| c.as_array())
        else {
            return out;
        };
        for candidate in candidates {
            let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            else {
                continue;
            };
            for part in parts {
                if let Some(tool_call) = Self::tool_call_part(part, origin_model) {
                    out.push(tool_call);
                }
            }
        }
        out
    }

    fn thought_signature(part: &Value) -> Option<String> {
        part.get("thoughtSignature")
            .and_then(Value::as_str)
            .filter(|signature| !signature.is_empty())
            .map(str::to_string)
    }

    fn tool_call_part(part: &Value, origin_model: Option<&str>) -> Option<LlmOutputPart> {
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
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            tool_name: name.to_string(),
            input_json,
            replay: Self::thought_signature(part).map(|opaque| ProviderReplayMeta {
                item_id: None,
                opaque: Some(opaque),
                origin: origin_model.map(Self::route_identity_for_model),
            }),
        })
    }

    fn reasoning_replay(
        signature: Option<String>,
        origin_model: Option<&str>,
    ) -> Option<ProviderReasoningReplay> {
        signature.map(|signature| ProviderReasoningReplay {
            item_id: None,
            encrypted_content: None,
            signature: Some(signature),
            redacted: false,
            summary: Vec::new(),
            origin: origin_model.map(Self::route_identity_for_model),
        })
    }

    fn push_reasoning_piece(
        parts: &mut Vec<LlmOutputPart>,
        reasoning_deltas: &mut Vec<String>,
        piece: String,
        signature: Option<String>,
        reconcile_with_previous_event: bool,
        origin_model: Option<&str>,
    ) {
        let replay = Self::reasoning_replay(signature, origin_model);
        if reconcile_with_previous_event
            && let Some(LlmOutputPart::Reasoning {
                text,
                replay: existing_replay,
            }) = parts.last_mut()
        {
            let existing_signature = existing_replay
                .as_ref()
                .and_then(|replay| replay.signature.as_deref());
            let incoming_signature = replay
                .as_ref()
                .and_then(|replay| replay.signature.as_deref());
            let signatures_conflict = matches!(
                (existing_signature, incoming_signature),
                (Some(existing), Some(incoming)) if existing != incoming
            );
            if signatures_conflict {
                Self::push_reasoning_piece(
                    parts,
                    reasoning_deltas,
                    piece,
                    replay.and_then(|replay| replay.signature),
                    false,
                    origin_model,
                );
                return;
            }

            let delta = if piece.starts_with(text.as_str()) {
                piece[text.len()..].to_string()
            } else if text.starts_with(&piece) {
                String::new()
            } else {
                piece
            };
            if !delta.is_empty() {
                text.push_str(&delta);
                reasoning_deltas.push(delta);
            }
            if existing_replay.is_none() && replay.is_some() {
                *existing_replay = replay;
            }
            return;
        }

        if !piece.is_empty() {
            reasoning_deltas.push(piece.clone());
        }
        parts.push(LlmOutputPart::Reasoning {
            text: piece,
            replay,
        });
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

    #[cfg(test)]
    pub(crate) fn process_sse_event(
        raw: &str,
        full: &mut String,
        text_deltas: &mut Vec<String>,
        usage: &mut LlmUsage,
        tool_call_parts: Option<&mut Vec<LlmOutputPart>>,
        finish_event: &mut Option<Value>,
    ) -> Result<(), LlmTransportError> {
        let mut provider_usage = None;
        let mut execution_evidence = None;
        let mut reasoning_deltas = Vec::new();
        Self::process_sse_event_with_text_parts(
            raw,
            SseTextPartSink {
                full,
                text_deltas,
                reasoning_deltas: &mut reasoning_deltas,
                usage,
                provider_usage: &mut provider_usage,
                execution_evidence: &mut execution_evidence,
                tool_call_parts,
                output_parts: None,
                finish_event,
            },
            None,
        )
    }

    pub(crate) fn process_sse_event_with_text_parts(
        raw: &str,
        sink: SseTextPartSink<'_>,
        origin_model: Option<&str>,
    ) -> Result<(), LlmTransportError> {
        let SseTextPartSink {
            full,
            text_deltas,
            reasoning_deltas,
            usage,
            provider_usage,
            execution_evidence,
            tool_call_parts,
            output_parts,
            finish_event,
        } = sink;
        if raw.trim().is_empty() || raw.trim() == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(raw).map_err(|e| {
            LlmTransportError::new(format!("Invalid Cloud Code SSE payload: {e}")).retryable(false)
        })?;
        ExecutionEvidence::merge_optional(
            execution_evidence,
            Self::execution_evidence_from_value(&event),
        )
        .map_err(|error| {
            LlmTransportError::new(format!("Google stream {error}"))
                .with_kind(ProviderFailureKind::Stream)
                .with_code(error.code())
        })?;
        let new_usage = Self::usage_from_event(&event);
        if new_usage.input_tokens > 0
            || new_usage.output_tokens > 0
            || new_usage.cache_read_input_tokens > 0
            || new_usage.cache_write_input_tokens > 0
            || new_usage.reasoning_output_tokens > 0
        {
            *usage = new_usage;
            // Keep the raw `usageMetadata` block alongside the normalized
            // counters, under the same non-zero guard so a trailing empty
            // block cannot clobber the captured sidecar.
            *provider_usage = event
                .get("response")
                .and_then(|response| response.get("usageMetadata"))
                .cloned();
        }
        let mut output_parts = output_parts;
        let mut discarded_output_parts = Vec::new();
        let mut saw_thought_in_event = false;
        for (piece, signature, is_thought) in Self::text_parts_from_event(&event) {
            if is_thought {
                let parts = output_parts
                    .as_deref_mut()
                    .unwrap_or(&mut discarded_output_parts);
                Self::push_reasoning_piece(
                    parts,
                    reasoning_deltas,
                    piece,
                    signature,
                    !saw_thought_in_event,
                    origin_model,
                );
                saw_thought_in_event = true;
                continue;
            }
            let Some(delta) = Self::apply_stream_piece(full, text_deltas, &piece) else {
                continue;
            };
            if let Some(parts) = output_parts.as_deref_mut() {
                parts.push(LlmOutputPart::Text {
                    text: delta,
                    response_meta: signature.map(|signature| ResponseTextMeta {
                        provider_payload: Some(signature),
                        origin: origin_model.map(Self::route_identity_for_model),
                        ..ResponseTextMeta::default()
                    }),
                });
            }
        }
        let tool_calls = Self::tool_call_parts_from_event(&event, origin_model);
        if let Some(parts) = tool_call_parts {
            parts.extend(tool_calls);
        }
        // Capture the last event carrying a non-empty `finishReason` so the
        // streaming finalizer can derive the terminal reason exactly like the
        // non-streaming path instead of hardcoding Stop.
        if Self::finish_reason_str(&event).is_some() {
            *finish_event = Some(event);
        }
        Ok(())
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

                if let Some(text) = item.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    if is_thought {
                        // Gemini flags reasoning text with `thought: true`.
                        // Route those into Reasoning so downstream code
                        // doesn't show them as assistant prose. Signature
                        // lives on the same part.
                        parts.push(LlmOutputPart::Reasoning {
                            text: text.to_string(),
                            replay: Self::reasoning_replay(signature.clone(), origin_model),
                        });
                    } else {
                        parts.push(LlmOutputPart::Text {
                            text: text.to_string(),
                            response_meta: signature.clone().map(|signature| ResponseTextMeta {
                                provider_payload: Some(signature),
                                origin: origin_model.map(Self::route_identity_for_model),
                                ..ResponseTextMeta::default()
                            }),
                        });
                    }
                }
                if let Some(tool_call) = Self::tool_call_part(item, origin_model) {
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
            "STOP" => LlmTerminalReason::Stop,
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
