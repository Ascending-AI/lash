use serde_json::{Value, json};

use lash_core::{ToolCall, ToolDefinition, ToolOutcome};

use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt, execution_failure,
    object_schema, require_str, retryable_io_failure,
};

/// Web search via Tavily API.
pub struct WebSearch {
    api_key: String,
    client: reqwest::Client,
}

impl WebSearch {
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

/// Build the cached `search_web` tool provider for the given Tavily API key.
pub fn web_search_provider(api_key: impl Into<String>) -> StaticToolProvider<WebSearch> {
    StaticToolProvider::new(vec![web_search_tool_definition()], WebSearch::new(api_key))
}

#[async_trait::async_trait]
impl StaticToolExecute for WebSearch {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let args = call.args;
        let query = match require_str(args, "query") {
            Ok(query) => query,
            Err(err) => return err,
        };
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 20);

        if self.api_key.trim().is_empty() {
            return execution_failure(
                "tavily_api_key_missing",
                "Tavily API key is required for web.search",
            );
        }

        let body = json!({
            "query": query,
            "max_results": limit,
        });

        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(data) => ToolOutcome::ok(json!({
                    "results": sanitize_results(data.get("results")),
                })),
                Err(err) => execution_failure(
                    "web_search_response_decode_failed",
                    format!("Failed to parse response: {err}"),
                ),
            },
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                execution_failure(
                    "tavily_api_error",
                    format!("Tavily API error ({status}): {body}"),
                )
            }
            Err(err) => retryable_io_failure(
                "web_search_request_failed",
                format!("Request failed: {err}"),
                Some(250),
            ),
        }
    }
}

fn sanitize_results(results: Option<&Value>) -> Vec<Value> {
    results
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            json!({
                "title": item.get("title").and_then(Value::as_str).unwrap_or_default(),
                "url": item.get("url").and_then(Value::as_str).unwrap_or_default(),
                "content": item.get("content").and_then(Value::as_str).unwrap_or_default(),
            })
        })
        .collect()
}

fn web_search_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
                "tool:search_web",
                "search_web",
                "Search the web for candidate sources. Returns ranked `results` with snippet text; use `web.fetch` when you need the page itself. This tool does not expose Tavily's optional generated answer; summarize from result snippets and fetched pages.",
                object_schema(
                    serde_json::json!({
                        "query": { "type": "string" },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 20,
                            "default": 5,
                            "description": "Maximum results to return (default 5)"
                        }
                    }),
                    &["query"],
                ),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "results": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "title": { "type": "string" },
                                    "url": { "type": "string" },
                                    "content": {
                                        "type": "string",
                                        "description": "Search-result snippet text."
                                    }
                                },
                                "required": ["title", "url", "content"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["results"],
                    "additionalProperties": false
                }),
            )
            .with_examples(vec![
                "await web.search({ query: \"latest Rust release notes\", limit: 5 })?".into(),
            ])
            .with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 250, 1000))
            .with_lashlang_binding(lash_tool_support::lashlang_binding(
                ["web"],
                "search",
                &["web_search"],
            ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_web_uses_limit_argument_in_model_contract() {
        let definition = web_search_tool_definition();

        let properties = definition
            .contract
            .input_schema
            .canonical
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("object properties");
        assert!(properties.contains_key("limit"));
        assert!(!properties.contains_key("max_results"));
        assert_eq!(properties["limit"]["default"], serde_json::json!(5));
        assert_eq!(properties["limit"]["maximum"], serde_json::json!(20));
        assert!(
            definition
                .contract
                .examples
                .iter()
                .all(|example| example.contains("limit"))
        );
        assert_eq!(
            definition.contract.output_schema.canonical["type"],
            serde_json::json!("object")
        );
        assert_eq!(
            definition.contract.output_schema.canonical["required"],
            serde_json::json!(["results"])
        );
        assert!(
            !definition.contract.output_schema.canonical["properties"]
                .as_object()
                .unwrap()
                .contains_key("answer")
        );
        assert_eq!(
            definition.manifest.activation,
            lash_core::ToolActivation::Always
        );
    }

    #[test]
    fn search_web_sanitizes_tavily_results_to_contract() {
        let results = sanitize_results(Some(&serde_json::json!([
            {
                "title": "Title",
                "url": "https://example.com",
                "content": "Snippet",
                "score": 0.9,
                "raw_content": null,
                "favicon": "https://example.com/favicon.ico"
            }
        ])));

        assert_eq!(
            results,
            vec![serde_json::json!({
                "title": "Title",
                "url": "https://example.com",
                "content": "Snippet"
            })]
        );
    }

    #[tokio::test]
    async fn missing_query_is_a_structured_invalid_request() {
        let result = lash_core::testing::run_tool(
            &web_search_provider("unused"),
            "search_web",
            &serde_json::json!({}),
        )
        .await;

        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("missing query must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::InvalidRequest);
        assert_eq!(failure.code, "invalid_tool_args");
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }
}
