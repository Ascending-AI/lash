use std::fmt;
use std::sync::Arc;

use lash_core::provider::ProviderHandle;
use lash_llm_transport::LlmHttpTransport;
use lash_provider_anthropic::AnthropicProvider;
use lash_provider_google::GoogleOAuthProvider;
use lash_provider_openai::{OpenAiCompatibleProvider, OpenAiProvider};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical_scripts::{
    ANTHROPIC_MESSAGES_TEXT, GOOGLE_STREAM_GENERATE_TEXT, OPENAI_RESPONSES_TEXT,
};
use crate::provider::ProviderWireScript;

pub const OPENAI_COMPATIBLE: &str = "openai-compatible";
pub const OPENAI: &str = "openai";
pub const ANTHROPIC: &str = "anthropic";
pub const GOOGLE_OAUTH: &str = "google_oauth";

pub const MIGRATED_RUNTIME_PROVIDER_KINDS: &[&str] =
    &[OPENAI_COMPATIBLE, OPENAI, ANTHROPIC, GOOGLE_OAUTH];

const OPENAI_COMPAT_RUNTIME_TEXT: &str =
    include_str!("../provider-scripts/runtime/openai-compatible.chat-runtime-text-stream.json");

#[derive(Debug)]
pub struct RuntimeProviderError {
    message: String,
}

impl RuntimeProviderError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RuntimeProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeProviderError {}

impl From<serde_json::Error> for RuntimeProviderError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<lash_core::facade_support::LlmTransportError> for RuntimeProviderError {
    fn from(value: lash_core::facade_support::LlmTransportError) -> Self {
        Self::new(value.to_string())
    }
}

pub fn runtime_provider_kind_for_session(session_index: usize) -> &'static str {
    MIGRATED_RUNTIME_PROVIDER_KINDS[session_index % MIGRATED_RUNTIME_PROVIDER_KINDS.len()]
}

pub fn runtime_script_name_for_kind(
    provider_kind: &str,
) -> Result<&'static str, RuntimeProviderError> {
    Ok(match provider_kind {
        OPENAI_COMPATIBLE => "openai-compatible.chat-runtime-text-stream",
        OPENAI => "openai.responses-text-stream",
        ANTHROPIC => "anthropic.messages-text-stream",
        GOOGLE_OAUTH => "google.stream-generate-content-text-stream",
        other => {
            return Err(RuntimeProviderError::new(format!(
                "unsupported generated runtime provider `{other}`"
            )));
        }
    })
}

/// The usage one scripted provider turn reports, in the ledger's buckets.
/// [`runtime_script_value_for_turn`] encodes it in each provider's own wire
/// convention; the durable-content oracle decodes it back from the wire.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScriptedUsage {
    pub input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub output_tokens: i64,
    /// Part of `output_tokens`, never more than it.
    pub reasoning_output_tokens: i64,
}

/// One scripted provider turn: the text it streams and, when set, the usage it
/// reports. `None` keeps the canonical script's fixed usage, which is what
/// traces recorded before generated usage existed were run against.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScriptedTurn {
    pub text: String,
    pub usage: Option<ScriptedUsage>,
}

/// The scripted turns an ingress boundary declares: `provider_texts`, paired
/// with `provider_usage` when the generator recorded it.
pub fn scripted_turns_from_ingress(payload: &Value) -> Result<Vec<ScriptedTurn>, String> {
    let texts = payload
        .get("provider_texts")
        .and_then(Value::as_array)
        .ok_or("missing provider_texts")?;
    if texts.is_empty() {
        return Err("provided no runtime provider scripts".to_string());
    }
    let usage = match payload.get("provider_usage") {
        None => vec![None; texts.len()],
        Some(usage) => {
            let usage = serde_json::from_value::<Vec<ScriptedUsage>>(usage.clone())
                .map_err(|err| format!("provider_usage is malformed: {err}"))?;
            if usage.len() != texts.len() {
                return Err(format!(
                    "provider_usage has {} entries for {} provider_texts",
                    usage.len(),
                    texts.len()
                ));
            }
            usage.into_iter().map(Some).collect()
        }
    };
    texts
        .iter()
        .zip(usage)
        .map(|(text, usage)| {
            Ok(ScriptedTurn {
                text: text
                    .as_str()
                    .ok_or("provider_texts holds a non-string entry")?
                    .to_string(),
                usage,
            })
        })
        .collect()
}

/// The scripted turn one provider boundary runs: its `text`, and its `usage`
/// when the generator recorded one.
pub fn scripted_turn_from_provider_boundary(payload: &Value) -> Result<ScriptedTurn, String> {
    Ok(ScriptedTurn {
        text: payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        usage: payload
            .get("usage")
            .map(|usage| serde_json::from_value(usage.clone()))
            .transpose()
            .map_err(|err| format!("usage is malformed: {err}"))?,
    })
}

pub fn runtime_scripts_for_turns(
    provider_kind: &str,
    turns: &[ScriptedTurn],
) -> Result<Vec<ProviderWireScript>, RuntimeProviderError> {
    turns
        .iter()
        .map(|turn| runtime_script_for_turn(provider_kind, turn))
        .collect()
}

pub fn runtime_script_for_turn(
    provider_kind: &str,
    turn: &ScriptedTurn,
) -> Result<ProviderWireScript, RuntimeProviderError> {
    let value = runtime_script_value_for_turn(provider_kind, &turn.text, turn.usage.as_ref())?;
    Ok(ProviderWireScript::from_json_str(&value.to_string())?)
}

pub fn runtime_script_for_text(
    provider_kind: &str,
    text: &str,
) -> Result<ProviderWireScript, RuntimeProviderError> {
    let value = runtime_script_value_for_text(provider_kind, text)?;
    let encoded = serde_json::to_string(&value)?;
    Ok(ProviderWireScript::from_json_str(&encoded)?)
}

/// Google runtime fixture variant that reports an explicit zero reasoning
/// count without weakening the canonical fixture's non-zero coverage.
pub fn google_runtime_script_for_text_with_explicit_zero_reasoning(
    text: &str,
) -> Result<ProviderWireScript, RuntimeProviderError> {
    let mut script = runtime_script_value_for_text(GOOGLE_OAUTH, text)?;
    for timeline_index in [1, 2] {
        let encoded = script
            .pointer(&format!("/timeline/{timeline_index}/data"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RuntimeProviderError::new(format!(
                    "Google runtime script missing timeline[{timeline_index}].data"
                ))
            })?;
        let mut event: Value = serde_json::from_str(encoded)?;
        event["response"]["usageMetadata"]["thoughtsTokenCount"] = json!(0);
        set_sse_data(&mut script, timeline_index, event)?;
    }
    ProviderWireScript::from_json_str(&script.to_string()).map_err(Into::into)
}

/// The runtime provider wire script for `text` as a still-mutable JSON value,
/// before it is parsed into a `ProviderWireScript`. This lets failure-script
/// builders reuse the exact happy-path request match and content framing for a
/// provider kind and then perturb the timeline.
pub fn runtime_script_value_for_text(
    provider_kind: &str,
    text: &str,
) -> Result<Value, RuntimeProviderError> {
    runtime_script_value_for_turn(provider_kind, text, None)
}

/// [`runtime_script_value_for_text`] reporting `usage` instead of the canonical
/// script's fixed counts.
pub fn runtime_script_value_for_turn(
    provider_kind: &str,
    text: &str,
    usage: Option<&ScriptedUsage>,
) -> Result<Value, RuntimeProviderError> {
    let mut script: Value = match provider_kind {
        OPENAI_COMPATIBLE => serde_json::from_str(OPENAI_COMPAT_RUNTIME_TEXT)?,
        OPENAI => serde_json::from_str(OPENAI_RESPONSES_TEXT)?,
        ANTHROPIC => serde_json::from_str(ANTHROPIC_MESSAGES_TEXT)?,
        GOOGLE_OAUTH => serde_json::from_str(GOOGLE_STREAM_GENERATE_TEXT)?,
        other => {
            return Err(RuntimeProviderError::new(format!(
                "unsupported generated runtime provider `{other}`"
            )));
        }
    };

    match provider_kind {
        OPENAI_COMPATIBLE => {
            set_sse_data(
                &mut script,
                1,
                json!({
                    "choices": [
                        {
                            "delta": {
                                "content": text,
                            }
                        }
                    ]
                }),
            )?;
        }
        OPENAI => {
            set_sse_data(
                &mut script,
                2,
                json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "item_id": "msg_1",
                    "delta": text,
                }),
            )?;
            let done_item = json!({
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": text}],
            });
            set_sse_data(
                &mut script,
                3,
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": done_item,
                }),
            )?;
            set_sse_data(
                &mut script,
                4,
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "output": [done_item],
                        "usage": {"input_tokens": 5, "output_tokens": 2},
                    }
                }),
            )?;
        }
        ANTHROPIC => {
            set_sse_data(
                &mut script,
                3,
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                }),
            )?;
        }
        GOOGLE_OAUTH => {
            if let Some(body) = script
                .pointer_mut("/request_match/body")
                .and_then(Value::as_object_mut)
            {
                body.remove("request.contents[0].parts[0].text");
                body.remove("request.sessionId");
            }
            set_sse_data(
                &mut script,
                1,
                json!({
                    "response": {
                        "responseId": "google-evidence-1",
                        "modelVersion": "gemini-3.1-pro-served",
                        "candidates": [
                            {
                                "content": {
                                    "parts": [
                                        {
                                            "text": text,
                                        }
                                    ]
                                }
                            }
                        ],
                        "usageMetadata": {
                            "promptTokenCount": 6,
                            "candidatesTokenCount": 3,
                            "thoughtsTokenCount": 1,
                        }
                    }
                }),
            )?;
        }
        _ => unreachable!("provider kind was validated above"),
    }

    if let Some(usage) = usage {
        encode_scripted_usage(provider_kind, &mut script, usage)?;
    }
    script["expected_provider"]["text"] = Value::String(text.to_string());
    Ok(script)
}

/// Write `usage` into a runtime script in `provider_kind`'s wire convention:
/// OpenAI reports prompt tokens inclusive of cached ones and reasoning inside
/// the completion count; Anthropic splits input (`message_start`) from output
/// (`message_delta`); Google reports candidates and thoughts separately.
fn encode_scripted_usage(
    provider_kind: &str,
    script: &mut Value,
    usage: &ScriptedUsage,
) -> Result<(), RuntimeProviderError> {
    let prompt = usage.input_tokens + usage.cache_read_input_tokens;
    match provider_kind {
        OPENAI_COMPATIBLE => {
            // The canonical runtime stream carries no usage chunk; the usage
            // chunk rides after the finish chunk, where OpenAI streams it.
            let timeline = script
                .get_mut("timeline")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| RuntimeProviderError::new("runtime script has no timeline"))?;
            let finish_at = timeline
                .get(2)
                .and_then(|event| event.get("at"))
                .and_then(Value::as_u64)
                .ok_or_else(|| RuntimeProviderError::new("runtime script has no finish chunk"))?;
            // Every wire event keeps its own release instant: sharing one
            // with the finish chunk would let task polling decide whether the
            // transport was already parked on the usage gate.
            let at = finish_at + 1;
            for later in timeline.iter_mut().skip(3) {
                if let Some(later_at) = later.get("at").and_then(Value::as_u64) {
                    later["at"] = json!(later_at + 1);
                }
            }
            timeline.insert(
                3,
                json!({
                    "at": at,
                    "event": "sse",
                    "data": json!({
                        "choices": [],
                        "usage": {
                            "prompt_tokens": prompt,
                            "completion_tokens": usage.output_tokens,
                            "prompt_tokens_details": {"cached_tokens": usage.cache_read_input_tokens},
                            "completion_tokens_details": {"reasoning_tokens": usage.reasoning_output_tokens},
                        },
                    })
                    .to_string(),
                }),
            );
        }
        OPENAI => {
            let mut completed = sse_data(script, 4)?;
            completed["response"]["usage"] = json!({
                "input_tokens": prompt,
                "output_tokens": usage.output_tokens,
                "input_tokens_details": {"cached_tokens": usage.cache_read_input_tokens},
                "output_tokens_details": {"reasoning_tokens": usage.reasoning_output_tokens},
            });
            set_sse_data(script, 4, completed)?;
        }
        ANTHROPIC => {
            let mut start = sse_data(script, 1)?;
            start["message"]["usage"] = json!({
                "input_tokens": usage.input_tokens,
                "cache_read_input_tokens": usage.cache_read_input_tokens,
                "output_tokens": 0,
            });
            set_sse_data(script, 1, start)?;
            let mut delta = sse_data(script, 5)?;
            delta["usage"] = json!({
                "output_tokens": usage.output_tokens,
                "output_tokens_details": {"thinking_tokens": usage.reasoning_output_tokens},
            });
            set_sse_data(script, 5, delta)?;
        }
        GOOGLE_OAUTH => {
            for timeline_index in [1, 2] {
                let mut event = sse_data(script, timeline_index)?;
                event["response"]["usageMetadata"] = json!({
                    "promptTokenCount": prompt,
                    "cachedContentTokenCount": usage.cache_read_input_tokens,
                    "candidatesTokenCount": usage.output_tokens - usage.reasoning_output_tokens,
                    "thoughtsTokenCount": usage.reasoning_output_tokens,
                });
                set_sse_data(script, timeline_index, event)?;
            }
        }
        other => {
            return Err(RuntimeProviderError::new(format!(
                "unsupported generated runtime provider `{other}`"
            )));
        }
    }
    Ok(())
}

fn sse_data(script: &Value, timeline_index: usize) -> Result<Value, RuntimeProviderError> {
    let encoded = script
        .pointer(&format!("/timeline/{timeline_index}/data"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RuntimeProviderError::new(format!(
                "provider runtime script missing timeline[{timeline_index}].data"
            ))
        })?;
    Ok(serde_json::from_str(encoded)?)
}

/// The distinct, non-vacuous prose a live failure script streams BEFORE its
/// terminal fault. A correct runtime must NOT commit this text on a mid-stream
/// terminal failure; if it leaks the partial prose, the live-failure oracle
/// catches it (it is searched for in the committed transcript).
pub const LIVE_FAILURE_LEAK_PROSE: &str = "LEAK-PARTIAL-PROSE-MUST-NOT-COMMIT";

/// The timeline index of the first streaming content delta for a provider kind,
/// matching the `set_sse_data` positions in `runtime_script_value_for_text`.
fn content_delta_index_for_kind(provider_kind: &str) -> Result<usize, RuntimeProviderError> {
    Ok(match provider_kind {
        OPENAI_COMPATIBLE => 1,
        OPENAI => 2,
        ANTHROPIC => 3,
        GOOGLE_OAUTH => 1,
        other => {
            return Err(RuntimeProviderError::new(format!(
                "live failure script not supported for provider `{other}`"
            )));
        }
    })
}

/// A runtime provider wire script that streams `prose_deltas` VALID content
/// deltas (each carrying `LIVE_FAILURE_LEAK_PROSE`) and THEN a non-retryable
/// malformed SSE chunk, so a live `session.turn().run()` first receives genuine
/// partial prose and then fails mid-stream. Because real prose is offered before
/// the fault, the oracle's "no committed output" assertion is non-vacuous: a
/// runtime that leaks the partial prose on terminal failure WOULD commit it. The
/// request match is the happy-path match, so a real turn matches it and the fault
/// is delivered through the real provider wire parser.
pub fn live_failure_script(
    provider_kind: &str,
    prose_deltas: usize,
) -> Result<ProviderWireScript, RuntimeProviderError> {
    if prose_deltas == 0 {
        return Err(RuntimeProviderError::new(
            "a live failure script must stream at least one valid prose delta before the fault",
        ));
    }
    let content_index = content_delta_index_for_kind(provider_kind)?;
    let mut script = runtime_script_value_for_text(provider_kind, LIVE_FAILURE_LEAK_PROSE)?;
    let timeline = script
        .get("timeline")
        .and_then(Value::as_array)
        .ok_or_else(|| RuntimeProviderError::new("runtime script timeline was not an array"))?;
    if content_index >= timeline.len() {
        return Err(RuntimeProviderError::new(format!(
            "runtime script for `{provider_kind}` has no content delta at index {content_index}"
        )));
    }
    // Preamble (everything up to but excluding the first content delta) followed
    // by `prose_deltas` valid content deltas and then a single malformed chunk.
    // The success-completion tail is dropped: the turn fails at the malformed
    // chunk before it would be reached.
    let content_delta = timeline[content_index].clone();
    let mut new_timeline = timeline[..content_index].to_vec();
    let mut at = content_delta
        .get("at")
        .and_then(Value::as_u64)
        .unwrap_or(20);
    for _ in 0..prose_deltas {
        let mut delta = content_delta.clone();
        if let Some(object) = delta.as_object_mut() {
            object.insert("at".to_string(), json!(at));
        }
        new_timeline.push(delta);
        at += 1;
    }
    new_timeline.push(json!({
        "at": at,
        "event": "sse",
        "data": "{ malformed provider event",
    }));
    script["timeline"] = Value::Array(new_timeline);
    script["expected_provider"] = json!({
        "mutation": "malformed_sse_chunk_after_valid_prose",
        "prose_deltas": prose_deltas,
        "leak_prose": LIVE_FAILURE_LEAK_PROSE,
        "expected": "terminal provider parser error after valid partial prose",
    });
    let encoded = serde_json::to_string(&script)?;
    Ok(ProviderWireScript::from_json_str(&encoded)?)
}

/// The native tool-call id a suspend-roundtrip session's first exchange streams.
pub const SUSPEND_TOOL_CALL_ID: &str = "suspend-call-1";

/// Two real openai-compatible provider wire scripts for a suspend-roundtrip
/// session: the first streams a native tool call for `tool_name` (which parks the
/// live turn on the await key), the second streams the final `resumed` answer
/// after the scheduler resolves the await. Routing these through the real
/// `ScriptedLlmHttpTransport` (rather than a `TestProvider`) means the parked turn
/// also exercises real provider wire parsing on both exchanges.
pub fn suspend_roundtrip_scripts(
    tool_name: &str,
) -> Result<Vec<ProviderWireScript>, RuntimeProviderError> {
    let tool_call_delta = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": SUSPEND_TOOL_CALL_ID,
                    "type": "function",
                    "function": { "name": tool_name, "arguments": "{}" }
                }]
            }
        }]
    })
    .to_string();
    let finish_delta = json!({
        "choices": [{ "finish_reason": "tool_calls", "delta": {} }]
    })
    .to_string();
    let tool_call_script = json!({
        "schema": "lash.provider-wire-script.v1",
        "name": format!("openai-compatible.chat-suspend-{tool_name}"),
        "provider_kind": "openai-compatible",
        "endpoint": { "method": "POST", "path": "/chat/completions" },
        "request_match": {
            "body": {
                "model": { "equals": "openai/gpt-5.4" },
                "stream": { "equals": true },
                "messages": { "contains_role": "user" }
            },
            "headers": {
                "authorization": { "present": true },
                "content-type": { "contains": "application/json" }
            }
        },
        "timeline": [
            {
                "at": 10,
                "event": "response_start",
                "status": 200,
                "headers": [
                    { "name": "content-type", "value": "text/event-stream; charset=utf-8" },
                    { "name": "x-request-id", "value": "req-suspend-tool" }
                ]
            },
            { "at": 20, "event": "sse", "data": tool_call_delta },
            { "at": 21, "event": "sse", "data": finish_delta },
            { "at": 22, "event": "sse", "data": "[DONE]" },
            { "at": 23, "event": "end" }
        ],
        "expected_provider": { "terminal_reason": "tool_calls" }
    });
    let tool_call_script = ProviderWireScript::from_json_str(&tool_call_script.to_string())?;
    let resumed_script = runtime_script_for_text(OPENAI_COMPATIBLE, "resumed")?;
    Ok(vec![tool_call_script, resumed_script])
}

pub fn runtime_provider_components<T>(
    provider_kind: &str,
    transport: &Arc<T>,
) -> Result<(ProviderHandle, lash::ModelSpec, String), RuntimeProviderError>
where
    T: LlmHttpTransport + 'static,
{
    let transport = provider_transport(transport);
    let (provider, model_name): (ProviderHandle, &str) = match provider_kind {
        OPENAI_COMPATIBLE => {
            let provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
                .with_transport(transport);
            (
                ProviderHandle::new(provider.into_components()),
                "openai/gpt-5.4",
            )
        }
        OPENAI => {
            let provider = OpenAiProvider::new("test-key").with_transport(transport);
            (ProviderHandle::new(provider.into_components()), "gpt-5.4")
        }
        ANTHROPIC => {
            let provider = AnthropicProvider::new("test-key")
                .with_base_url(Some("https://anthropic.test".to_string()))
                .with_transport(transport);
            (
                ProviderHandle::new(provider.into_components()),
                "claude-sonnet-4-20250514",
            )
        }
        GOOGLE_OAUTH => {
            let provider = GoogleOAuthProvider::new(
                "access-token",
                "refresh-token",
                0,
                lash_provider_google::GoogleOAuthClient {
                    id: "oauth-client-id".into(),
                    secret: "oauth-client-secret".into(),
                },
            )
            .with_project_id(Some("project-1".to_string()))
            .with_transport(transport);
            (
                ProviderHandle::new(provider.into_components()),
                "gemini-3.1-pro-preview",
            )
        }
        other => {
            return Err(RuntimeProviderError::new(format!(
                "unsupported generated runtime provider `{other}`"
            )));
        }
    };
    let model = lash::ModelSpec::builder(model_name)
        .context_window_tokens(200_000)
        .build()
        .map_err(|err| RuntimeProviderError::new(err.to_string()))?;
    Ok((provider, model, provider_kind.to_string()))
}

fn set_sse_data(
    script: &mut Value,
    timeline_index: usize,
    data: Value,
) -> Result<(), RuntimeProviderError> {
    let slot = script
        .get_mut("timeline")
        .and_then(Value::as_array_mut)
        .and_then(|timeline| timeline.get_mut(timeline_index))
        .and_then(|event| event.get_mut("data"))
        .ok_or_else(|| {
            RuntimeProviderError::new(format!(
                "provider runtime script missing timeline[{timeline_index}].data"
            ))
        })?;
    *slot = Value::String(data.to_string());
    Ok(())
}

fn provider_transport<T>(transport: &Arc<T>) -> Arc<dyn LlmHttpTransport>
where
    T: LlmHttpTransport + 'static,
{
    transport.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderWireEvent;

    fn sse_payload(event: &ProviderWireEvent) -> Value {
        let ProviderWireEvent::Sse { data, .. } = event else {
            panic!("expected scripted SSE event");
        };
        serde_json::from_str(data).expect("scripted SSE payload is JSON")
    }

    /// The content oracle's own wire decoder reads back exactly the text and
    /// usage each provider's encoding wrote, so a ledger mismatch is lash's.
    #[test]
    fn scripted_turn_encodings_decode_to_what_they_report() {
        let usage = ScriptedUsage {
            input_tokens: i64::from(u32::MAX) + 3,
            cache_read_input_tokens: 1 << 40,
            output_tokens: (1 << 53) - 2,
            reasoning_output_tokens: 7,
        };
        let text = "caf\u{e9} \u{0}\r\n\u{1f980}e\u{301}";
        for kind in MIGRATED_RUNTIME_PROVIDER_KINDS {
            let script = runtime_script_for_turn(
                kind,
                &ScriptedTurn {
                    text: text.to_string(),
                    usage: Some(usage),
                },
            )
            .expect("scripted turn");
            let emitted = crate::content_oracle::emitted_attempt(&script).expect("decodes");
            assert_eq!(emitted.text, text, "{kind}");
            assert!(emitted.completed, "{kind}");
            assert_eq!(
                emitted.usage,
                Some(crate::content_oracle::UsageBuckets {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_input_tokens: usage.cache_read_input_tokens,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: usage.reasoning_output_tokens,
                }),
                "{kind}"
            );
        }
    }

    #[test]
    fn generated_google_partial_fixture_preserves_canonical_identity_and_reasoning() {
        let script = live_failure_script(GOOGLE_OAUTH, 2).expect("Google partial fixture");
        let payload = sse_payload(&script.timeline()[1]);
        assert_eq!(
            payload.pointer("/response/responseId"),
            Some(&json!("google-evidence-1"))
        );
        assert_eq!(
            payload.pointer("/response/modelVersion"),
            Some(&json!("gemini-3.1-pro-served"))
        );
        assert_eq!(
            payload.pointer("/response/usageMetadata/thoughtsTokenCount"),
            Some(&json!(1))
        );
    }

    #[test]
    fn generated_google_explicit_zero_reasoning_variant_preserves_presence() {
        let script = google_runtime_script_for_text_with_explicit_zero_reasoning("answer")
            .expect("Google explicit-zero variant");
        for timeline_index in [1, 2] {
            let payload = sse_payload(&script.timeline()[timeline_index]);
            assert_eq!(
                payload.pointer("/response/usageMetadata/thoughtsTokenCount"),
                Some(&json!(0))
            );
        }
    }

    #[test]
    fn generated_openai_fixture_does_not_carry_google_identity_fields() {
        let script = runtime_script_value_for_text(OPENAI, "answer").expect("OpenAI fixture");
        let payload = sse_payload(
            &ProviderWireScript::from_json_str(&script.to_string())
                .expect("valid OpenAI fixture")
                .timeline()[4],
        );
        assert!(payload.pointer("/response/responseId").is_none());
        assert!(payload.pointer("/response/modelVersion").is_none());
    }
}
