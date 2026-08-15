//! Responses SSE event folding and buffered payload parsing.

use super::*;

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
        LlmTransportError::new(format!("Invalid {provider} SSE payload: {e}")).with_raw(raw)
    })?;
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if event_type == "error" {
        let retryable = event
            .get("error")
            .map(responses_error_is_retryable)
            .unwrap_or(false);
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
        return Err(LlmTransportError::new(message)
            .retryable(retryable)
            .with_raw(event.to_string()));
    }

    let output_index = event
        .get("output_index")
        .and_then(Value::as_u64)
        .map(|value| value as usize);

    if let Some(resp) = event.get("response") {
        state.final_response = Some(resp.clone());
        state.provider_usage = resp.get("usage").filter(|usage| !usage.is_null()).cloned();
        merge_usage(&mut state.usage, &usage_from_response_value(resp));
    } else {
        merge_usage(&mut state.usage, &usage_from_response_value(&event));
    }

    if crate::responses_output_evidence::handle_evidence_only_event(event_type, &event, state) {
        return Ok(());
    }
    match event_type {
        "response.output_item.added" => {
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
        "response.reasoning_summary_part.added" => state.begin_reasoning_part(output_index),
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                state.push_reasoning_delta(delta, output_index);
            }
        }
        "response.reasoning_summary_text.done" => {
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
        "response.reasoning_summary_part.done" => state.finish_reasoning_part(),
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                state.push_text_delta(delta, output_index);
            }
        }
        "response.output_text.done" => {
            if let Some(text) = event.get("text").and_then(|v| v.as_str()) {
                state.reconcile_text_event(text, output_index);
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                state.push_tool_call_delta(
                    output_index,
                    event.get("item_id").and_then(|v| v.as_str()),
                    delta,
                );
            }
        }
        "response.function_call_arguments.done" => {
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
        "response.output_item.done" => {
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
        "response.completed" | "response.incomplete" | "response.done" => {
            state.terminal_event_seen = true;
            if let Some(resp_value) = event.get("response") {
                state.merge_final_response(resp_value);
            }
        }
        "response.failed" => {
            state.terminal_event_seen = true;
            return Err(crate::responses_output_evidence::response_failed_error(
                provider, &event,
            ));
        }
        _ => state.unrecognized_event_observed = true,
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
