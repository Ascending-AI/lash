use std::fmt;
use std::sync::Arc;

use lash_core::provider::ProviderHandle;
use lash_llm_transport::LlmHttpTransport;
use lash_provider_anthropic::AnthropicProvider;
use lash_provider_google::GoogleOAuthProvider;
use lash_provider_openai::{OpenAiCompatibleProvider, OpenAiProvider};
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

pub fn runtime_scripts_for_texts(
    provider_kind: &str,
    texts: &[String],
) -> Result<Vec<ProviderWireScript>, RuntimeProviderError> {
    texts
        .iter()
        .map(|text| runtime_script_for_text(provider_kind, text))
        .collect()
}

pub fn runtime_script_for_text(
    provider_kind: &str,
    text: &str,
) -> Result<ProviderWireScript, RuntimeProviderError> {
    let value = runtime_script_value_for_text(provider_kind, text)?;
    let encoded = serde_json::to_string(&value)?;
    Ok(ProviderWireScript::from_json_str(&encoded)?)
}

/// The runtime provider wire script for `text` as a still-mutable JSON value,
/// before it is parsed into a `ProviderWireScript`. This lets failure-script
/// builders reuse the exact happy-path request match and content framing for a
/// provider kind and then perturb the timeline.
pub fn runtime_script_value_for_text(
    provider_kind: &str,
    text: &str,
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
                            "thoughtsTokenCount": 0,
                        }
                    }
                }),
            )?;
        }
        _ => unreachable!("provider kind was validated above"),
    }

    script["expected_provider"]["text"] = Value::String(text.to_string());
    Ok(script)
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
                    "id": "suspend-call-1",
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
            let provider = GoogleOAuthProvider::new("access-token", "refresh-token", 0)
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

    #[test]
    fn generated_google_partial_fixture_preserves_identity_and_explicit_zero_reasoning() {
        let script = live_failure_script(GOOGLE_OAUTH, 2).expect("Google partial fixture");
        let payload = sse_payload(&script.timeline[1]);
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
            Some(&json!(0))
        );
    }

    #[test]
    fn generated_openai_fixture_does_not_carry_google_identity_fields() {
        let script = runtime_script_value_for_text(OPENAI, "answer").expect("OpenAI fixture");
        let payload = sse_payload(
            &ProviderWireScript::from_json_str(&script.to_string())
                .expect("valid OpenAI fixture")
                .timeline[4],
        );
        assert!(payload.pointer("/response/responseId").is_none());
        assert!(payload.pointer("/response/modelVersion").is_none());
    }
}
