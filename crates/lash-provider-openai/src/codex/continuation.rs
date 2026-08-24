//! Cached-context continuation planning for the WebSocket transport.
//!
//! One responsibility: decide what a reused socket has to resend. A completed
//! response is recorded as a continuation (its id, the input it answered, and
//! the items it produced); the next request on the same socket is replayed
//! against that record, and only the suffix plus a `previous_response_id` is
//! sent when the prefix still matches. Every miss is named so the attempt trace
//! says why the full context went out again.

use serde_json::{Value, json};

use super::{CodexProvider, CodexTransport};

#[derive(Clone, Debug, Default)]
pub(super) struct CodexContinuation {
    previous_response_id: String,
    request_input: Vec<Value>,
    response_items: Vec<Value>,
    body_fingerprint: String,
}

#[derive(Clone, Debug)]
pub(super) struct CodexWebsocketRequestPlan {
    pub(super) body: Value,
    pub(super) context: CodexWebsocketContextPlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodexWebsocketCacheMiss {
    Disabled { continuation_available: bool },
    MissingContinuation,
    BodyFingerprintMismatch,
    InputPrefixMismatch,
}

impl CodexWebsocketCacheMiss {
    pub(super) fn wire_reason(self) -> &'static str {
        match self {
            Self::Disabled { .. } => "disabled",
            Self::MissingContinuation => "missing_continuation",
            Self::BodyFingerprintMismatch => "body_fingerprint_mismatch",
            Self::InputPrefixMismatch => "input_prefix_mismatch",
        }
    }

    fn continuation_available(self) -> bool {
        match self {
            Self::Disabled {
                continuation_available,
            } => continuation_available,
            Self::MissingContinuation => false,
            Self::BodyFingerprintMismatch | Self::InputPrefixMismatch => true,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum CodexWebsocketContextPlan {
    FullContext {
        miss: CodexWebsocketCacheMiss,
        input_items: usize,
    },
    Continued {
        previous_response_id: String,
        full_input_items: usize,
        sent_input_items: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CodexWebsocketRenderedContext<'a> {
    pub(super) cached_request: bool,
    pub(super) continuation_available: bool,
    pub(super) cache_miss_reason: Option<&'static str>,
    pub(super) previous_response_id: Option<&'a str>,
    pub(super) full_input_items: usize,
    pub(super) sent_input_items: usize,
}

impl CodexWebsocketContextPlan {
    pub(super) fn is_continued(&self) -> bool {
        matches!(self, Self::Continued { .. })
    }

    pub(super) fn rendered(&self) -> CodexWebsocketRenderedContext<'_> {
        match self {
            Self::FullContext { miss, input_items } => CodexWebsocketRenderedContext {
                cached_request: false,
                continuation_available: miss.continuation_available(),
                cache_miss_reason: Some(miss.wire_reason()),
                previous_response_id: None,
                full_input_items: *input_items,
                sent_input_items: *input_items,
            },
            Self::Continued {
                previous_response_id,
                full_input_items,
                sent_input_items,
            } => CodexWebsocketRenderedContext {
                cached_request: true,
                continuation_available: true,
                cache_miss_reason: None,
                previous_response_id: Some(previous_response_id),
                full_input_items: *full_input_items,
                sent_input_items: *sent_input_items,
            },
        }
    }
}

impl CodexProvider {
    fn body_input(body: &Value) -> Vec<Value> {
        body.get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn body_fingerprint(body: &Value) -> String {
        let mut comparable = body.clone();
        if let Some(obj) = comparable.as_object_mut() {
            obj.remove("input");
            obj.remove("previous_response_id");
        }
        comparable.to_string()
    }

    fn response_output_items(final_response: &Value) -> Vec<Value> {
        final_response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn cached_websocket_body(
        continuation: &CodexContinuation,
        full_body: &Value,
    ) -> Option<Value> {
        Self::cached_websocket_body_result(continuation, full_body)
            .map(|(body, _)| body)
            .ok()
    }

    fn cached_websocket_body_result(
        continuation: &CodexContinuation,
        full_body: &Value,
    ) -> Result<(Value, String), CodexWebsocketCacheMiss> {
        let current_fingerprint = Self::body_fingerprint(full_body);
        let current_input = Self::body_input(full_body);
        if continuation.body_fingerprint != current_fingerprint {
            return Err(CodexWebsocketCacheMiss::BodyFingerprintMismatch);
        }
        let mut baseline = continuation.request_input.clone();
        baseline.extend(continuation.response_items.clone());
        if current_input.len() < baseline.len()
            || !current_input
                .iter()
                .take(baseline.len())
                .eq(baseline.iter())
        {
            return Err(CodexWebsocketCacheMiss::InputPrefixMismatch);
        }

        let mut body = full_body.clone();
        let previous_response_id = continuation.previous_response_id.clone();
        body["previous_response_id"] = json!(previous_response_id);
        body["input"] = Value::Array(current_input[baseline.len()..].to_vec());
        Ok((body, previous_response_id))
    }

    pub(super) fn websocket_continuation_enabled(&self) -> bool {
        matches!(
            self.transport,
            CodexTransport::Auto | CodexTransport::WebsocketCached
        )
    }

    pub(super) fn websocket_request_plan(
        &self,
        full_body: &Value,
        continuation: Option<&CodexContinuation>,
        allow_cached_context: bool,
    ) -> CodexWebsocketRequestPlan {
        let full_input_items = Self::body_input(full_body).len();
        let continuation_available = continuation.is_some();
        let (body, context) = match (allow_cached_context, continuation) {
            (false, _) => (
                full_body.clone(),
                CodexWebsocketContextPlan::FullContext {
                    miss: CodexWebsocketCacheMiss::Disabled {
                        continuation_available,
                    },
                    input_items: full_input_items,
                },
            ),
            (true, None) => (
                full_body.clone(),
                CodexWebsocketContextPlan::FullContext {
                    miss: CodexWebsocketCacheMiss::MissingContinuation,
                    input_items: full_input_items,
                },
            ),
            (true, Some(cached)) => match Self::cached_websocket_body_result(cached, full_body) {
                Ok((body, previous_response_id)) => {
                    let sent_input_items = Self::body_input(&body).len();
                    (
                        body,
                        CodexWebsocketContextPlan::Continued {
                            previous_response_id,
                            full_input_items,
                            sent_input_items,
                        },
                    )
                }
                Err(miss) => (
                    full_body.clone(),
                    CodexWebsocketContextPlan::FullContext {
                        miss,
                        input_items: full_input_items,
                    },
                ),
            },
        };
        CodexWebsocketRequestPlan { body, context }
    }

    pub(super) fn websocket_create_request(body: &Value) -> Value {
        let mut request = body
            .as_object()
            .cloned()
            .unwrap_or_else(serde_json::Map::new);
        request.insert("type".to_string(), json!("response.create"));
        Value::Object(request)
    }

    pub(super) fn continuation_from_response(
        full_body: &Value,
        final_response: &Value,
    ) -> Option<CodexContinuation> {
        let completed = final_response
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status == "completed");
        let response_id = final_response.get("id").and_then(Value::as_str)?;
        if !completed || response_id.is_empty() {
            return None;
        }
        Some(CodexContinuation {
            previous_response_id: response_id.to_string(),
            request_input: Self::body_input(full_body),
            response_items: Self::response_output_items(final_response),
            body_fingerprint: Self::body_fingerprint(full_body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_body(input: Vec<Value>) -> Value {
        json!({
            "model": "gpt-5.4",
            "input": input,
            "stream": true,
        })
    }

    fn continuation(first_input: &Value, response_item: &Value) -> CodexContinuation {
        let body = full_body(vec![first_input.clone()]);
        CodexContinuation {
            previous_response_id: "resp_1".to_string(),
            request_input: vec![first_input.clone()],
            response_items: vec![response_item.clone()],
            body_fingerprint: CodexProvider::body_fingerprint(&body),
        }
    }

    #[test]
    fn context_plan_renders_each_outcome_once() {
        let provider = CodexProvider::new("access", "refresh", 0);
        let first_input = json!({"type": "message", "role": "user", "content": "hello"});
        let response_item = json!({"type": "message", "role": "assistant", "content": "answer"});
        let continuation = continuation(&first_input, &response_item);
        let first_body = full_body(vec![first_input.clone()]);
        let disabled_without_continuation =
            provider.websocket_request_plan(&first_body, None, false);
        let disabled_with_continuation =
            provider.websocket_request_plan(&first_body, Some(&continuation), false);
        let missing_continuation = provider.websocket_request_plan(&first_body, None, true);

        let mut fingerprint_mismatch_body = first_body.clone();
        fingerprint_mismatch_body["model"] = json!("gpt-5-codex");
        let fingerprint_mismatch =
            provider.websocket_request_plan(&fingerprint_mismatch_body, Some(&continuation), true);

        let prefix_mismatch_body = full_body(vec![first_input.clone(), json!({"next": true})]);
        let prefix_mismatch =
            provider.websocket_request_plan(&prefix_mismatch_body, Some(&continuation), true);

        let continued_body = full_body(vec![
            first_input,
            response_item,
            json!({"type": "message", "role": "user", "content": "next"}),
        ]);
        let continued = provider.websocket_request_plan(&continued_body, Some(&continuation), true);

        let cases = [
            (
                "disabled without continuation",
                disabled_without_continuation,
                CodexWebsocketRenderedContext {
                    cached_request: false,
                    continuation_available: false,
                    cache_miss_reason: Some("disabled"),
                    previous_response_id: None,
                    full_input_items: 1,
                    sent_input_items: 1,
                },
            ),
            (
                "disabled with continuation",
                disabled_with_continuation,
                CodexWebsocketRenderedContext {
                    cached_request: false,
                    continuation_available: true,
                    cache_miss_reason: Some("disabled"),
                    previous_response_id: None,
                    full_input_items: 1,
                    sent_input_items: 1,
                },
            ),
            (
                "missing continuation",
                missing_continuation,
                CodexWebsocketRenderedContext {
                    cached_request: false,
                    continuation_available: false,
                    cache_miss_reason: Some("missing_continuation"),
                    previous_response_id: None,
                    full_input_items: 1,
                    sent_input_items: 1,
                },
            ),
            (
                "body fingerprint mismatch",
                fingerprint_mismatch,
                CodexWebsocketRenderedContext {
                    cached_request: false,
                    continuation_available: true,
                    cache_miss_reason: Some("body_fingerprint_mismatch"),
                    previous_response_id: None,
                    full_input_items: 1,
                    sent_input_items: 1,
                },
            ),
            (
                "input prefix mismatch",
                prefix_mismatch,
                CodexWebsocketRenderedContext {
                    cached_request: false,
                    continuation_available: true,
                    cache_miss_reason: Some("input_prefix_mismatch"),
                    previous_response_id: None,
                    full_input_items: 2,
                    sent_input_items: 2,
                },
            ),
            (
                "continued",
                continued,
                CodexWebsocketRenderedContext {
                    cached_request: true,
                    continuation_available: true,
                    cache_miss_reason: None,
                    previous_response_id: Some("resp_1"),
                    full_input_items: 3,
                    sent_input_items: 1,
                },
            ),
        ];

        for (outcome, plan, expected) in cases {
            assert_eq!(plan.context.rendered(), expected, "{outcome}");
        }
    }

    #[test]
    fn cache_miss_wire_reasons_are_exhaustive() {
        let reasons = [
            (
                CodexWebsocketCacheMiss::Disabled {
                    continuation_available: false,
                },
                "disabled",
            ),
            (
                CodexWebsocketCacheMiss::MissingContinuation,
                "missing_continuation",
            ),
            (
                CodexWebsocketCacheMiss::BodyFingerprintMismatch,
                "body_fingerprint_mismatch",
            ),
            (
                CodexWebsocketCacheMiss::InputPrefixMismatch,
                "input_prefix_mismatch",
            ),
        ];

        for (miss, expected) in reasons {
            assert_eq!(miss.wire_reason(), expected);
        }
    }
}
