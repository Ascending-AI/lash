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
    pub(super) cached: bool,
    pub(super) continuation_available: bool,
    pub(super) cache_miss_reason: Option<&'static str>,
    pub(super) previous_response_id: Option<String>,
    pub(super) full_input_items: usize,
    pub(super) sent_input_items: usize,
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
        Self::cached_websocket_body_result(continuation, full_body).ok()
    }

    fn cached_websocket_body_result(
        continuation: &CodexContinuation,
        full_body: &Value,
    ) -> Result<Value, &'static str> {
        let current_fingerprint = Self::body_fingerprint(full_body);
        let current_input = Self::body_input(full_body);
        if continuation.body_fingerprint != current_fingerprint {
            return Err("body_fingerprint_mismatch");
        }
        let mut baseline = continuation.request_input.clone();
        baseline.extend(continuation.response_items.clone());
        if current_input.len() < baseline.len()
            || !current_input
                .iter()
                .take(baseline.len())
                .eq(baseline.iter())
        {
            return Err("input_prefix_mismatch");
        }

        let mut body = full_body.clone();
        body["previous_response_id"] = json!(continuation.previous_response_id);
        body["input"] = Value::Array(current_input[baseline.len()..].to_vec());
        Ok(body)
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
        let (body, cached, cache_miss_reason) = match (allow_cached_context, continuation) {
            (false, _) => (full_body.clone(), false, Some("disabled")),
            (true, None) => (full_body.clone(), false, Some("missing_continuation")),
            (true, Some(cached)) => match Self::cached_websocket_body_result(cached, full_body) {
                Ok(body) => (body, true, None),
                Err(reason) => (full_body.clone(), false, Some(reason)),
            },
        };
        let previous_response_id = body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let sent_input_items = Self::body_input(&body).len();
        CodexWebsocketRequestPlan {
            body,
            cached,
            continuation_available,
            cache_miss_reason,
            previous_response_id,
            full_input_items,
            sent_input_items,
        }
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
