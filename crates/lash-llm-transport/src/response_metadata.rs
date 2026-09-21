//! Provider-neutral allowlisted response-metadata capture.

use std::collections::BTreeMap;

use lash_core::provider::ProviderOptions;
use serde_json::Value;

/// Stable response-metadata key holding a gateway's top-level `meta` block
/// verbatim.
///
/// OpenAI-compatible gateways (OpenRouter, Opper) answer with a top-level
/// `meta` object next to `usage` — `meta.routing` carries requested / served /
/// attempts / strategy. Lash never interprets it; it is retained so a host can
/// read served-route provenance off the response.
pub const GATEWAY_META_KEY: &str = "gateway:meta";

/// Top-level response field the gateway `meta` block arrives in.
const GATEWAY_META_FIELD: &str = "meta";

/// Accumulates wire observations for one provider request.
///
/// Headers are captured once when the response starts. Buffered JSON and each
/// SSE event pass through the same body-pointer capture, with the last value
/// observed at a pointer winning. `header:` and `body:` observations are
/// host-configured allowlists; [`GATEWAY_META_KEY`] is retained whenever the
/// gateway sends it, with the same last-wins rule.
#[derive(Clone, Debug, Default)]
pub struct ResponseMetadataCapture {
    headers: Vec<String>,
    body_paths: Vec<String>,
    captured: BTreeMap<String, Value>,
}

impl ResponseMetadataCapture {
    pub fn from_response(options: &ProviderOptions, response_headers: &[(String, String)]) -> Self {
        let mut capture = Self {
            headers: options
                .response_metadata_headers
                .iter()
                .map(|name| name.to_ascii_lowercase())
                .collect(),
            body_paths: options.response_metadata_body_paths.clone(),
            captured: BTreeMap::new(),
        };
        capture.capture_headers(response_headers);
        capture
    }

    pub fn is_active(&self) -> bool {
        !self.headers.is_empty() || !self.body_paths.is_empty()
    }

    /// An allowlist makes every payload worth decoding. Without one, only a
    /// payload that mentions the gateway `meta` field is: the substring test
    /// keeps the unconfigured streaming path to one scan per event instead of
    /// a second full JSON parse.
    fn worth_decoding(&self, raw: &str) -> bool {
        self.is_active() || raw.contains("\"meta\"")
    }

    /// Capture allowlisted headers, matching names case-insensitively.
    pub fn capture_headers(&mut self, headers: &[(String, String)]) {
        for allowed_name in &self.headers {
            let key = format!("header:{allowed_name}");
            if self.captured.contains_key(&key) {
                continue;
            }
            if let Some((_, value)) = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(allowed_name))
            {
                self.captured.insert(key, Value::String(value.clone()));
            }
        }
    }

    /// Capture configured JSON pointers and the gateway `meta` block from one
    /// decoded response value.
    pub fn capture_body(&mut self, value: &Value) {
        for pointer in &self.body_paths {
            if let Some(value) = value.pointer(pointer) {
                self.captured
                    .insert(format!("body:{pointer}"), value.clone());
            }
        }
        self.capture_gateway_meta(value);
    }

    /// Retain a gateway's top-level `meta` block verbatim under
    /// [`GATEWAY_META_KEY`], the last block observed winning.
    ///
    /// An absent or null `meta` retains nothing, so a gateway that does not
    /// send one leaves no key. A `meta` that is not a JSON object contradicts
    /// the shape every such gateway documents; it is dropped with a debug
    /// trace rather than retained or raised, because an observation must never
    /// change response parsing semantics.
    fn capture_gateway_meta(&mut self, value: &Value) {
        match value.get(GATEWAY_META_FIELD) {
            None | Some(Value::Null) => {}
            Some(meta @ Value::Object(_)) => {
                self.captured
                    .insert(GATEWAY_META_KEY.to_string(), meta.clone());
            }
            Some(other) => {
                tracing::debug!(
                    observed_json_type = json_type_name(other),
                    "dropping gateway response meta that is not a JSON object"
                );
            }
        }
    }

    /// Capture one SSE payload. Invalid or non-JSON provider events are
    /// ignored: observation must never change response parsing semantics.
    pub fn capture_sse_event(&mut self, raw: &str) {
        if self.worth_decoding(raw)
            && let Ok(value) = serde_json::from_str(raw)
        {
            self.capture_body(&value);
        }
    }

    /// Capture a buffered response that may contain either JSON or framed SSE.
    pub fn capture_body_text(&mut self, raw: &str) {
        if !self.worth_decoding(raw) {
            return;
        }
        if raw.trim_start().starts_with("data:") || raw.contains("\ndata:") {
            let _ = crate::frame_sse_payload(raw, |event| {
                self.capture_sse_event(event);
                Ok(())
            });
        } else if let Ok(value) = serde_json::from_str(raw) {
            self.capture_body(&value);
        }
    }

    /// Snapshot the observations for a partial response while retaining the
    /// accumulator for later events.
    pub fn metadata(&self) -> BTreeMap<String, Value> {
        self.captured.clone()
    }

    pub fn into_metadata(self) -> BTreeMap<String, Value> {
        self.captured
    }
}

/// Name the JSON type of a value for a diagnostic, never its contents.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlists_headers_and_last_sse_body_value() {
        let options = ProviderOptions {
            response_metadata_headers: vec!["X-Request-Cost".to_string()],
            response_metadata_body_paths: vec!["/usage/cost".to_string()],
            ..ProviderOptions::default()
        };
        let mut capture = ResponseMetadataCapture::from_response(
            &options,
            &[
                ("x-request-cost".to_string(), "0.01".to_string()),
                ("set-cookie".to_string(), "secret".to_string()),
            ],
        );
        capture.capture_body_text(concat!(
            "data: {\"usage\":{\"cost\":1},\"secret\":\"first\"}\n\n",
            "data: {\"usage\":{\"cost\":2},\"secret\":\"last\"}\n\n"
        ));

        let metadata = capture.into_metadata();
        assert_eq!(metadata["header:x-request-cost"], serde_json::json!("0.01"));
        assert_eq!(metadata["body:/usage/cost"], serde_json::json!(2));
        assert!(!metadata.contains_key("header:set-cookie"));
        assert!(!metadata.values().any(|value| value == "secret"));
    }

    #[test]
    fn gateway_meta_is_retained_verbatim_without_any_allowlist() {
        let mut capture = ResponseMetadataCapture::default();
        capture.capture_body_text(
            r#"{"id":"gen-1","meta":{"routing":{"requested":"auto","served":"deepinfra","attempts":2,"strategy":"fallback"}}}"#,
        );

        let metadata = capture.into_metadata();
        assert_eq!(
            metadata[GATEWAY_META_KEY],
            serde_json::json!({"routing":{"requested":"auto","served":"deepinfra","attempts":2,"strategy":"fallback"}})
        );
    }

    #[test]
    fn gateway_meta_capture_is_last_wins_across_sse_events() {
        let mut capture = ResponseMetadataCapture::default();
        capture.capture_body_text(concat!(
            "data: {\"meta\":{\"routing\":{\"served\":\"first\"}}}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: {\"meta\":{\"routing\":{\"served\":\"last\"}}}\n\n"
        ));

        let metadata = capture.into_metadata();
        assert_eq!(
            metadata[GATEWAY_META_KEY],
            serde_json::json!({"routing":{"served":"last"}})
        );
    }

    #[test]
    fn absent_or_malformed_gateway_meta_retains_no_key() {
        let mut absent = ResponseMetadataCapture::default();
        absent.capture_body_text(r#"{"id":"gen-1","usage":{"total_tokens":3}}"#);
        assert!(absent.into_metadata().is_empty());

        let mut null = ResponseMetadataCapture::default();
        null.capture_body_text(r#"{"id":"gen-1","meta":null}"#);
        assert!(null.into_metadata().is_empty());

        for malformed in [
            r#"{"id":"gen-1","meta":"routing"}"#,
            r#"{"id":"gen-1","meta":[{"routing":{}}]}"#,
            r#"{"id":"gen-1","meta":7}"#,
        ] {
            let mut capture = ResponseMetadataCapture::default();
            capture.capture_body_text(malformed);
            assert!(
                capture.into_metadata().is_empty(),
                "malformed meta is dropped, not retained: {malformed}"
            );
        }
    }

    #[test]
    fn gateway_meta_is_read_only_from_the_top_level() {
        let mut capture = ResponseMetadataCapture::default();
        capture.capture_body_text(r#"{"choices":[{"meta":{"routing":{"served":"nested"}}}]}"#);
        assert!(capture.into_metadata().is_empty());
    }
}
