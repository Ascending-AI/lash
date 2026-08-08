//! Provider-neutral allowlisted response-metadata capture.

use std::collections::BTreeMap;

use lash_core::provider::ProviderOptions;
use serde_json::Value;

/// Accumulates allowlisted wire observations for one provider request.
///
/// Headers are captured once when the response starts. Buffered JSON and each
/// SSE event pass through the same body-pointer capture, with the last value
/// observed at a pointer winning.
#[derive(Clone, Debug, Default)]
pub struct ResponseMetadataCapture {
    headers: Vec<String>,
    body_paths: Vec<String>,
    captured: BTreeMap<String, Value>,
}

impl ResponseMetadataCapture {
    /// Start a capture from the shared provider options and response headers.
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

    /// Whether either allowlist asks the transport to inspect the response.
    pub fn is_active(&self) -> bool {
        !self.headers.is_empty() || !self.body_paths.is_empty()
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

    /// Capture configured JSON pointers from one decoded response value.
    pub fn capture_body(&mut self, value: &Value) {
        for pointer in &self.body_paths {
            if let Some(value) = value.pointer(pointer) {
                self.captured
                    .insert(format!("body:{pointer}"), value.clone());
            }
        }
    }

    /// Capture one SSE payload. Invalid or non-JSON provider events are
    /// ignored: observation must never change response parsing semantics.
    pub fn capture_sse_event(&mut self, raw: &str) {
        if self.is_active()
            && let Ok(value) = serde_json::from_str(raw)
        {
            self.capture_body(&value);
        }
    }

    /// Capture a buffered response that may contain either JSON or framed SSE.
    pub fn capture_body_text(&mut self, raw: &str) {
        if !self.is_active() {
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

    /// Finish capture and return the response metadata map.
    pub fn into_metadata(self) -> BTreeMap<String, Value> {
        self.captured
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
}
