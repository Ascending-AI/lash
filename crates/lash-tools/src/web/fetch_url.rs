use serde_json::json;

use lash_core::{ToolCall, ToolDefinition, ToolFailure, ToolFailureClass, ToolOutcome, ToolValue};

use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolDefinitionBindingExt, execution_failure,
    object_schema, require_str, retryable_io_failure,
};

/// Fetch a URL and return its content as text.
pub struct FetchUrl {
    api_key: String,
    client: reqwest::Client,
}

impl FetchUrl {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }
}

impl Default for FetchUrl {
    fn default() -> Self {
        Self::new("")
    }
}

/// Build the cached `fetch_url` tool provider for the given Tavily API key.
pub fn fetch_url_provider(api_key: impl Into<String>) -> StaticToolProvider<FetchUrl> {
    StaticToolProvider::new(vec![fetch_url_tool_definition()], FetchUrl::new(api_key))
}

#[async_trait::async_trait]
impl StaticToolExecute for FetchUrl {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let args = call.args;
        let url = match require_str(args, "url") {
            Ok(s) => s,
            Err(e) => return e,
        };

        if self.api_key.trim().is_empty() {
            return execution_failure(
                "tavily_api_key_missing",
                "Tavily API key is required for web.fetch",
            );
        }

        let body = json!({
            "api_key": self.api_key,
            "urls": [url],
        });

        let resp = self
            .client
            .post("https://api.tavily.com/extract")
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(resp) => resp,
            Err(err) => {
                return retryable_io_failure(
                    "web_fetch_request_failed",
                    format!("web.fetch request failed: {err}"),
                    Some(250),
                );
            }
        };
        let status = resp.status();
        let value: serde_json::Value = match resp.json().await {
            Ok(value) => value,
            Err(err) => {
                return execution_failure(
                    "web_fetch_response_decode_failed",
                    format!("web.fetch response failed: {err}"),
                );
            }
        };
        if !status.is_success() {
            let body = ToolValue::from(value.clone());
            let mut failure = ToolFailure::tool(
                ToolFailureClass::Execution,
                "tavily_api_error",
                format!("Tavily API error ({status}): {value}"),
            );
            failure.raw = Some(body);
            return ToolOutcome::failure(failure);
        }
        let content = value
            .get("results")
            .and_then(|value| value.as_array())
            .and_then(|results| results.first())
            .and_then(|item| item.get("raw_content").or_else(|| item.get("content")))
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        ToolOutcome::ok(json!({
            "url": url,
            "content": content,
        }))
    }
}

fn fetch_url_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
                "tool:fetch_url",
                "fetch_url",
                "Fetch one known URL and extract readable page text.",
                object_schema(
                    serde_json::json!({
                        "url": { "type": "string", "format": "uri" }
                    }),
                    &["url"],
                ),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Fetched URL."
                        },
                        "content": {
                            "type": "string",
                            "description": "Extracted readable page text. Empty when no extractable content was returned."
                        }
                    },
                    "required": ["url", "content"],
                    "additionalProperties": false
                }),
            )
            .with_examples(vec!["await web.fetch({ url: \"https://www.rust-lang.org/\" })?".into()])
            .with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 250, 1000))
            .with_tool_binding(lash_tool_support::tool_binding(
                ["web"],
                "fetch",
                &["fetch", "open_url"],
            ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fetch_url_returns_minimal_typed_record() {
        let definition = fetch_url_tool_definition();

        assert_eq!(
            definition.contract.output_schema.canonical["type"],
            serde_json::json!("object")
        );
        assert_eq!(
            definition.contract.output_schema.canonical["required"],
            serde_json::json!(["url", "content"])
        );
        assert_eq!(
            definition.contract.output_schema.canonical["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            definition.manifest.activation,
            lash_core::ToolActivation::Always
        );
    }

    #[tokio::test]
    async fn missing_api_key_is_a_structured_execution_failure() {
        let result = lash_core::testing::run_tool(
            &fetch_url_provider(""),
            "fetch_url",
            &json!({"url": "https://example.com"}),
        )
        .await;

        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("missing API key must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::Execution);
        assert_eq!(failure.code, "tavily_api_key_missing");
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }
}
