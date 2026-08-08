//! Provider-neutral response-normalization primitives shared by the lash
//! provider crates. Each of these existed in 2–4 near-identical copies across
//! the OpenAI, Anthropic, Google, and Codex crates; this is the single
//! canonical implementation every provider now calls.
//!
//! These are deliberately small, stateless helpers. The cross-provider
//! conformance suite ([`crate::conformance`]) pins the normalized-output
//! contract, so there is no normalizer trait here — providers wire their own
//! wire-format parsing to these primitives directly.

use lash_core::provider::ProviderOptions;
use lash_core::{ProviderFailureKind, facade_support::LlmTransportError};
use lash_sansio::llm::types::{LlmOutputPart, LlmTerminalReason, LlmUsage};
use serde_json::Value;

use crate::util::{extract_error_detail, parse_i64};

/// Merge incremental streaming usage: overwrite each `dst` field with the
/// corresponding `next` field only when `next` reports a non-zero value,
/// keeping prior non-zero counters when a later event reports only a subset.
///
/// Streaming usage arrives incrementally and later events may carry only some
/// counters; this keeps the earlier ones rather than clobbering them with
/// zeros.
pub fn merge_usage(dst: &mut LlmUsage, next: &LlmUsage) {
    if next.input_tokens > 0 {
        dst.input_tokens = next.input_tokens;
    }
    if next.output_tokens > 0 {
        dst.output_tokens = next.output_tokens;
    }
    if next.cache_read_input_tokens > 0 {
        dst.cache_read_input_tokens = next.cache_read_input_tokens;
    }
    if next.cache_write_input_tokens > 0 {
        dst.cache_write_input_tokens = next.cache_write_input_tokens;
    }
    if next.reasoning_output_tokens > 0 {
        dst.reasoning_output_tokens = next.reasoning_output_tokens;
    }
}

/// Terminal reason derived from assembled parts alone: `ToolUse` when any tool
/// call is present, otherwise `Stop`. Used as the fallback when a provider's
/// wire response carries no recognizable finish reason.
pub fn terminal_reason_from_parts(parts: &[LlmOutputPart]) -> LlmTerminalReason {
    if parts
        .iter()
        .any(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
    {
        LlmTerminalReason::ToolUse
    } else {
        LlmTerminalReason::Stop
    }
}

/// Parse token usage from an OpenAI-compatible response object. Handles both
/// Responses (`input_tokens` / `output_tokens`) and Chat Completions
/// (`prompt_tokens` / `completion_tokens`) shapes, plus common gateway aliases.
pub fn openai_usage_from_response_value(value: &Value) -> LlmUsage {
    openai_usage_from_usage_value(value.get("usage").unwrap_or(&Value::Null))
}

/// Parse token usage from an OpenAI-compatible `usage` object. OpenAI reports
/// prompt/input tokens as a total; Lash stores uncached input, cache-read, and
/// cache-write as separate buckets.
pub fn openai_usage_from_usage_value(usage: &Value) -> LlmUsage {
    let cached_tokens = parse_i64(
        usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .or_else(|| usage.get("cached_tokens"))
            .or_else(|| usage.get("prompt_cache_hit_tokens")),
    );
    let cache_write_tokens = parse_i64(
        usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cache_write_tokens"))
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cache_write_tokens"))
            })
            .or_else(|| usage.get("cache_write_tokens")),
    );
    let prompt_tokens = parse_i64(
        usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens")),
    );
    LlmUsage {
        input_tokens: prompt_tokens
            .saturating_sub(cached_tokens)
            .saturating_sub(cache_write_tokens)
            .max(0),
        output_tokens: parse_i64(
            usage
                .get("output_tokens")
                .or_else(|| usage.get("completion_tokens")),
        ),
        cache_read_input_tokens: cached_tokens,
        cache_write_input_tokens: cache_write_tokens,
        reasoning_output_tokens: parse_i64(
            usage
                .get("reasoning_tokens")
                .or_else(|| {
                    usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                })
                .or_else(|| {
                    usage
                        .get("completion_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                }),
        ),
    }
}

/// Map a final OpenAI Responses object to a terminal reason. Honors both
/// snake_case and camelCase incomplete details emitted by compatible gateways.
pub fn openai_terminal_reason_from_response_value(
    value: &Value,
    parts: &[LlmOutputPart],
) -> LlmTerminalReason {
    let incomplete_details = value
        .get("incomplete_details")
        .or_else(|| value.get("incompleteDetails"))
        .filter(|details| !details.is_null());
    let incomplete_reason =
        incomplete_details.and_then(|details| details.get("reason").and_then(Value::as_str));
    match incomplete_reason {
        Some("max_output_tokens" | "max_tokens") => return LlmTerminalReason::OutputLimit,
        Some("content_filter" | "safety") => return LlmTerminalReason::ContentFilter,
        Some(_) => return LlmTerminalReason::ProviderError,
        None => {}
    }
    if value.get("status").and_then(Value::as_str) == Some("cancelled") {
        return LlmTerminalReason::Cancelled;
    }
    if value.get("status").and_then(Value::as_str) == Some("failed") {
        return LlmTerminalReason::ProviderError;
    }
    if value.get("status").and_then(Value::as_str) == Some("incomplete") && parts.is_empty() {
        return LlmTerminalReason::ProviderError;
    }
    terminal_reason_from_parts(parts)
}

/// Map a Chat Completions `finish_reason` value, preserving the provided
/// fallback when the finish reason is absent or empty.
pub fn openai_terminal_reason_from_chat_finish_reason(
    finish_reason: &str,
    empty_fallback: LlmTerminalReason,
) -> LlmTerminalReason {
    match finish_reason {
        "stop" => LlmTerminalReason::Stop,
        "tool_calls" | "function_call" => LlmTerminalReason::ToolUse,
        "length" | "max_tokens" => LlmTerminalReason::OutputLimit,
        "content_filter" | "safety" => LlmTerminalReason::ContentFilter,
        "" => empty_fallback,
        _ => LlmTerminalReason::ProviderError,
    }
}

/// Map a final OpenAI-compatible Chat Completions object to a terminal reason,
/// falling back to assembled parts when no finish reason is present.
pub fn openai_terminal_reason_from_chat_value(
    value: &Value,
    parts: &[LlmOutputPart],
) -> LlmTerminalReason {
    let first_choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    if first_choice
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("refusal"))
        .and_then(Value::as_str)
        .is_some_and(|refusal| !refusal.is_empty())
    {
        return LlmTerminalReason::ContentFilter;
    }
    let finish = first_choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("");
    openai_terminal_reason_from_chat_finish_reason(finish, terminal_reason_from_parts(parts))
}

/// Frame a buffered SSE payload — possibly several `data:`/`event:` lines
/// separated by blank lines — into individual event strings, invoking
/// `on_event` once per complete event. Each event's `data:` lines are joined
/// with `\n`; `event:` lines are ignored; a blank line terminates the current
/// event. A trailing event without a closing blank line is still flushed.
///
/// This frames an already-buffered SSE blob (e.g. a non-streaming response
/// body that nonetheless carries SSE text), as opposed to
/// [`crate::streaming::drive_sse_response`], which frames a live byte stream.
pub fn frame_sse_payload<F>(payload: &str, mut on_event: F) -> Result<(), LlmTransportError>
where
    F: FnMut(&str) -> Result<(), LlmTransportError>,
{
    let mut event_lines: Vec<String> = Vec::new();
    for mut line in payload.lines().map(str::to_string) {
        if line.ends_with('\r') {
            line.pop();
        }
        if let Some(data) = line.strip_prefix("data:") {
            event_lines.push(data.trim().to_string());
            continue;
        }
        if line.starts_with("event:") {
            continue;
        }
        if line.trim().is_empty() && !event_lines.is_empty() {
            let raw = event_lines.join("\n");
            on_event(&raw)?;
            event_lines.clear();
        }
    }
    if !event_lines.is_empty() {
        let raw = event_lines.join("\n");
        on_event(&raw)?;
    }
    Ok(())
}

/// Build the canonical HTTP-error envelope every provider returns for a non-2xx
/// response: an [`LlmTransportError`] carrying the status code, the response
/// headers, and the raw body, plus the originating request body when available.
///
/// `message` is the provider-specific human-readable summary (e.g.
/// `"Anthropic request failed with 429"`). The provider's `error.message`, or
/// a bounded raw-body fallback, is appended here so every caller gets the same
/// detail extraction. Retryability is left to the central provider failure
/// classifier, which reads the attached status.
///
/// The envelope is pre-labelled [`ProviderFailureKind::Http`] so it is
/// self-describing even before classification. This matches what
/// `DefaultProviderFailureClassifier` derives anyway (it upgrades `Unknown` to
/// `Http` whenever a status is attached), and the classifier's status/text
/// reclassifications (`Auth`, `Validation`, `Quota`, `Unsupported`) are
/// unconditional, so the pre-set kind can never mask them.
pub fn http_error_envelope(
    message: impl Into<String>,
    status: u16,
    headers: Vec<(String, String)>,
    raw_body: impl Into<String>,
    request_body: Option<String>,
) -> LlmTransportError {
    let raw_body = raw_body.into();
    let message = message.into();
    let message = extract_error_detail(&raw_body)
        .filter(|detail| !message.contains(detail))
        .map_or(message.clone(), |detail| format!("{message}: {detail}"));
    let mut err = LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Http)
        .with_status(status)
        .with_headers(headers)
        .with_raw(raw_body.clone());
    if err.retry_after.is_none()
        && let Some(retry_after) = retry_after_from_error_body(&raw_body)
    {
        err = err.with_retry_after(retry_after);
    }
    if let Some(request_body) = request_body {
        err = err.with_request_body(request_body);
    }
    err
}

fn retry_after_from_error_body(raw_body: &str) -> Option<std::time::Duration> {
    let value = serde_json::from_str::<serde_json::Value>(raw_body).ok()?;
    find_json_string(&value, "retryDelay").and_then(parse_provider_duration)
}

fn find_json_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    match value {
        serde_json::Value::Object(fields) => fields
            .get(key)
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                fields
                    .values()
                    .find_map(|value| find_json_string(value, key))
            }),
        serde_json::Value::Array(items) => {
            items.iter().find_map(|value| find_json_string(value, key))
        }
        _ => None,
    }
}

fn parse_provider_duration(value: &str) -> Option<std::time::Duration> {
    if let Some(milliseconds) = value.strip_suffix("ms") {
        return milliseconds
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(|value| std::time::Duration::try_from_secs_f64(value / 1_000.0).ok());
    }
    value
        .strip_suffix('s')?
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|value| std::time::Duration::try_from_secs_f64(value).ok())
}

/// Append the `options` tail to a provider's `serialize_config` map: emit the
/// `options` key only when the options differ from the default, matching what
/// each provider's `serialize_config` hand-rolled.
pub fn serialize_options_tail(
    map: &mut serde_json::Map<String, serde_json::Value>,
    options: &ProviderOptions,
) {
    if !options.is_default() {
        map.insert(
            "options".to_string(),
            serde_json::to_value(options).unwrap_or(serde_json::Value::Null),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_usage_keeps_prior_nonzero_when_next_is_zero() {
        let mut dst = LlmUsage {
            input_tokens: 100,
            ..LlmUsage::default()
        };
        merge_usage(
            &mut dst,
            &LlmUsage {
                output_tokens: 20,
                cache_write_input_tokens: 7,
                ..LlmUsage::default()
            },
        );
        assert_eq!(dst.input_tokens, 100);
        assert_eq!(dst.output_tokens, 20);
        assert_eq!(dst.cache_write_input_tokens, 7);
    }

    #[test]
    fn terminal_reason_from_parts_prefers_tool_use() {
        let tool = vec![LlmOutputPart::ToolCall {
            call_id: "c".into(),
            tool_name: "t".into(),
            input_json: "{}".into(),
            replay: None,
        }];
        assert_eq!(
            terminal_reason_from_parts(&tool),
            LlmTerminalReason::ToolUse
        );
        assert_eq!(terminal_reason_from_parts(&[]), LlmTerminalReason::Stop);
    }

    #[test]
    fn frame_sse_payload_splits_events_and_joins_data_lines() {
        let mut events = Vec::new();
        frame_sse_payload("event: x\ndata: a\ndata: b\n\ndata: c\n", |e| {
            events.push(e.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(events, vec!["a\nb".to_string(), "c".to_string()]);
    }

    #[test]
    fn openai_usage_parser_accepts_responses_and_chat_completion_shapes() {
        let responses_usage = openai_usage_from_response_value(&serde_json::json!({
            "usage": {
                "input_tokens": 11,
                "output_tokens": 7,
                "input_tokens_details": {"cached_tokens": 5, "cache_write_tokens": 2},
                "output_tokens_details": {"reasoning_tokens": 5}
            }
        }));
        assert_eq!(responses_usage.input_tokens, 4);
        assert_eq!(responses_usage.output_tokens, 7);
        assert_eq!(responses_usage.cache_read_input_tokens, 5);
        assert_eq!(responses_usage.cache_write_input_tokens, 2);
        assert_eq!(responses_usage.reasoning_output_tokens, 5);

        let chat_usage = openai_usage_from_response_value(&serde_json::json!({
            "usage": {
                "prompt_tokens": 13,
                "completion_tokens": 17,
                "prompt_tokens_details": {"cached_tokens": 6, "cache_write_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 4}
            }
        }));
        assert_eq!(chat_usage.input_tokens, 3);
        assert_eq!(chat_usage.output_tokens, 17);
        assert_eq!(chat_usage.cache_read_input_tokens, 6);
        assert_eq!(chat_usage.cache_write_input_tokens, 4);
        assert_eq!(chat_usage.reasoning_output_tokens, 4);
    }

    #[test]
    fn openai_terminal_mappers_use_wire_reason_then_parts_fallback() {
        let tool_parts = vec![LlmOutputPart::ToolCall {
            call_id: "c".into(),
            tool_name: "t".into(),
            input_json: "{}".into(),
            replay: None,
        }];
        assert_eq!(
            openai_terminal_reason_from_chat_value(
                &serde_json::json!({"choices":[{"finish_reason":"length"}]}),
                &tool_parts,
            ),
            LlmTerminalReason::OutputLimit
        );
        assert_eq!(
            openai_terminal_reason_from_chat_value(
                &serde_json::json!({"choices":[{}]}),
                &tool_parts
            ),
            LlmTerminalReason::ToolUse
        );
        assert_eq!(
            openai_terminal_reason_from_response_value(
                &serde_json::json!({"status":"incomplete","incomplete_details":{"reason":"safety"}}),
                &[],
            ),
            LlmTerminalReason::ContentFilter
        );
        assert_eq!(
            openai_terminal_reason_from_chat_value(
                &serde_json::json!({
                    "choices": [{
                        "message": {"refusal": ""},
                        "finish_reason": "stop"
                    }]
                }),
                &[],
            ),
            LlmTerminalReason::Stop
        );
    }

    #[test]
    fn http_error_envelope_carries_http_kind_status_headers_raw_and_request_body() {
        let err = http_error_envelope(
            "Provider request failed with 429",
            429,
            vec![("retry-after".to_string(), "7".to_string())],
            r#"{"error":"rate limited"}"#,
            Some(r#"{"model":"m"}"#.to_string()),
        );
        assert_eq!(err.kind, ProviderFailureKind::Http);
        assert_eq!(err.status, Some(429));
        assert_eq!(err.code.as_deref(), Some("429"));
        assert_eq!(
            err.raw.as_deref().map(String::as_str),
            Some(r#"{"error":"rate limited"}"#)
        );
        assert_eq!(err.request_body.as_deref(), Some(r#"{"model":"m"}"#));
        assert_eq!(
            err.retry_after,
            Some(std::time::Duration::from_secs(7)),
            "with_headers must derive retry-after from the header pairs"
        );

        let without_request_body = http_error_envelope("failed", 500, Vec::new(), "boom", None);
        assert_eq!(without_request_body.request_body, None);
    }

    #[test]
    fn http_error_envelope_reads_google_retry_info_when_header_is_absent() {
        let err = http_error_envelope(
            "Gemini request failed with 429",
            429,
            Vec::new(),
            r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"55s"}]}}"#,
            None,
        );
        assert_eq!(err.retry_after, Some(std::time::Duration::from_secs(55)));
    }

    #[test]
    fn http_error_envelope_ignores_overflowing_provider_retry_delay() {
        let err = http_error_envelope(
            "Provider request failed with 429",
            429,
            Vec::new(),
            r#"{"error":{"details":[{"retryDelay":"1e300s"}]}}"#,
            None,
        );
        assert_eq!(err.retry_after, None);
    }

    #[test]
    fn serialize_options_tail_omits_default() {
        let mut map = serde_json::Map::new();
        serialize_options_tail(&mut map, &ProviderOptions::default());
        assert!(map.is_empty());
    }
}
