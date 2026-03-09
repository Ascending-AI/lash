use serde_json::json;

use crate::{ToolDefinition, ToolParam, ToolProvider, ToolResult};

/// Web search via Tavily API.
pub struct WebSearch {
    api_key: String,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl ToolProvider for WebSearch {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "search_web".into(),
            description: vec![crate::ToolText::new(
                "Search the web via Tavily. Returns a dict with `results` (list of `{title, url, content}` dicts) and, when available, a top-level `answer` summary string. Access results via `r[\"results\"]` and `r[\"answer\"]`.",
                [
                    crate::ExecutionMode::Repl,
                    crate::ExecutionMode::NativeTools,
                ],
            )],
            params: vec![
                ToolParam::typed("query", "str"),
                ToolParam {
                    name: "max_results".into(),
                    r#type: "int".into(),
                    description: "Maximum results to return (default 5)".into(),
                    required: false,
                },
            ],
            returns: "dict".into(),
            examples: vec![
                crate::ToolText::new(
                    "r = search_web(query=\"latest Rust release notes\", max_results=5)\nr[\"answer\"]\nr[\"results\"][0][\"url\"]",
                    [crate::ExecutionMode::Repl],
                ),
                crate::ToolText::new(
                    "search_web(query=\"latest Rust release notes\", max_results=5)",
                    [crate::ExecutionMode::NativeTools],
                ),
            ],
            hidden: false,
            inject_into_prompt: false,
        }]
    }

    async fn execute(&self, _name: &str, args: &serde_json::Value) -> ToolResult {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5);

        let body = json!({
            "api_key": self.api_key,
            "query": query,
            "max_results": max_results,
            "include_answer": true,
        });

        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(data) => {
                    let results: Vec<serde_json::Value> = data
                        .get("results")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|r| {
                                    json!({
                                        "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                                        "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                                        "content": r.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    let mut result = json!({ "results": results });
                    if let Some(answer) = data.get("answer").and_then(|v| v.as_str()) {
                        result["answer"] = json!(answer);
                    }

                    ToolResult::ok(result)
                }
                Err(e) => ToolResult::err_fmt(format_args!("Failed to parse response: {e}")),
            },
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                ToolResult::err_fmt(format_args!("Tavily API error ({status}): {body}"))
            }
            Err(e) => ToolResult::err_fmt(format_args!("Request failed: {e}")),
        }
    }
}
