//! Responses SSE event folding and buffered payload parsing.

use super::*;
use crate::responses_stream_event::{ResponsesStreamEvent, ResponsesStreamEventClass};

/// Drive one SSE event into `state`, optionally emitting finalized parts.
pub fn process_sse_event(
    provider: &str,
    raw: &str,
    state: &mut ResponsesStreamState,
    emitted_parts: Option<&mut Vec<LlmOutputPart>>,
) -> Result<(), LlmTransportError> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "[DONE]" {
        return Ok(());
    }
    let event: Value = serde_json::from_str(raw).map_err(|e| {
        LlmTransportError::new(format!("Invalid {provider} SSE payload: {e}"))
            .with_raw(raw)
            .with_retry_verdict(TransportRetryVerdict::NotRetryable)
    })?;
    let event_name = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if event_name == "error" {
        let retry_verdict = event
            .get("error")
            .map(responses_error_retry_verdict)
            .unwrap_or_default();
        let message = event
            .get("message")
            .and_then(|v| v.as_str())
            .or_else(|| {
                event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("OpenAI-compatible stream error");
        let failure = LlmTransportError::new(message)
            .with_retry_verdict(retry_verdict)
            .with_raw(event.to_string());
        return Err(classify_openai_error(&event, failure));
    }
    let event_type = ResponsesStreamEvent::parse(event_name);

    let output_index = event
        .get("output_index")
        .and_then(Value::as_u64)
        .map(|value| value as usize);

    if let Some(response) = event.get("response") {
        state.capture_execution_evidence(response, event_type.is_terminal())?;
    }

    if let Some(resp) = event.get("response") {
        state.final_response = Some(resp.clone());
        state.provider_usage = resp.get("usage").filter(|usage| !usage.is_null()).cloned();
        merge_usage(&mut state.usage, &usage_from_response_value(resp));
    } else {
        merge_usage(&mut state.usage, &usage_from_response_value(&event));
    }

    match event_type.handling_class() {
        ResponsesStreamEventClass::EvidenceOnly | ResponsesStreamEventClass::Lifecycle => {
            crate::responses_output_evidence::handle_output_evidence_event(
                event_type, &event, state,
            );
        }
        ResponsesStreamEventClass::Structural => {
            crate::responses_output_evidence::handle_output_evidence_event(
                event_type, &event, state,
            );
            match event_type {
                ResponsesStreamEvent::ResponseOutputItemAdded => {
                    if let Some(item) = event.get("item") {
                        state.streamed_item_content_received |=
                            crate::responses_output_evidence::output_item_has_output_evidence(item);
                        match item.get("type").and_then(|v| v.as_str()) {
                            Some("message") => state.begin_message(Some(item), output_index),
                            Some("function_call") => {
                                let _ = state.update_tool_call_from_item(item, output_index);
                            }
                            Some("reasoning") => state.begin_reasoning_part(output_index),
                            Some(_) => state.streamed_item_content_received = true,
                            None => {}
                        }
                    }
                }
                ResponsesStreamEvent::ResponseReasoningSummaryPartAdded => {
                    state.begin_reasoning_part(output_index)
                }
                ResponsesStreamEvent::ResponseReasoningSummaryTextDelta => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                        state.push_reasoning_delta(delta, output_index);
                    }
                }
                ResponsesStreamEvent::ResponseReasoningSummaryTextDone => {
                    // The `text` field is the full text for the current part; reconcile
                    // by appending the missing suffix if our accumulator lags behind.
                    if let Some(text) = event.get("text").and_then(|v| v.as_str())
                        && let Some(index) = state.current_reasoning_part
                        && let Some(LlmOutputPart::Reasoning { text: existing, .. }) =
                            state.parts.get(index)
                    {
                        let existing = existing.clone();
                        if text != existing
                            && let Some(suffix) = text.strip_prefix(existing.as_str())
                        {
                            state.push_reasoning_delta(suffix, output_index);
                        }
                    }
                }
                ResponsesStreamEvent::ResponseReasoningSummaryPartDone => {
                    state.finish_reasoning_part()
                }
                ResponsesStreamEvent::ResponseOutputTextDelta => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                        state.push_text_delta(delta, output_index);
                    }
                }
                ResponsesStreamEvent::ResponseOutputTextDone => {
                    if let Some(text) = event.get("text").and_then(|v| v.as_str()) {
                        state.reconcile_text_event(text, output_index);
                    }
                }
                ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                        state.push_tool_call_delta(
                            output_index,
                            event.get("item_id").and_then(|v| v.as_str()),
                            delta,
                        );
                    }
                }
                ResponsesStreamEvent::ResponseFunctionCallArgumentsDone => {
                    if let Some(arguments) = event.get("arguments").and_then(|v| v.as_str()) {
                        state.set_tool_call_arguments(
                            output_index,
                            event.get("item_id").and_then(|v| v.as_str()),
                            arguments,
                        );
                    }
                    state.streamed_item_content_received |= event
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.is_empty());
                }
                ResponsesStreamEvent::ResponseOutputItemDone => {
                    if let Some(item) = event.get("item") {
                        match item.get("type").and_then(|v| v.as_str()) {
                            Some("message") => {
                                let part = state.finish_message(Some(item), output_index);
                                if let (Some(parts), Some(part)) = (emitted_parts, part) {
                                    parts.push(part);
                                }
                            }
                            Some("reasoning") => {
                                state.finish_reasoning_part();
                                let part = state.finalize_reasoning_item(item, output_index);
                                if let (Some(parts), Some(part)) = (emitted_parts, part) {
                                    parts.push(part);
                                }
                            }
                            Some("function_call") => {
                                let part = state.finish_tool_call(item, output_index);
                                if let (Some(parts), Some(part)) = (emitted_parts, part) {
                                    parts.push(part);
                                }
                            }
                            Some(_) => state.streamed_item_content_received = true,
                            None => {}
                        }
                    }
                }
                _ => unreachable!("non-structural event classified as structural"),
            }
        }
        ResponsesStreamEventClass::Terminal => {
            state.terminal_event_seen = true;
            if event_type == ResponsesStreamEvent::ResponseFailed {
                return Err(crate::responses_output_evidence::response_failed_error(
                    provider, &event,
                ));
            }
            if let Some(resp_value) = event.get("response") {
                state.merge_final_response(resp_value);
            }
        }
        ResponsesStreamEventClass::Unknown => {
            state.unrecognized_event_observed = true;
        }
    }
    Ok(())
}

/// Parse a buffered SSE payload into `state`.
pub fn parse_sse_payload(
    provider: &str,
    payload: &str,
    state: &mut ResponsesStreamState,
) -> Result<(), LlmTransportError> {
    frame_sse_payload(payload, |raw| process_sse_event(provider, raw, state, None))
}
