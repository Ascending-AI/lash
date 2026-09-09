//! OpenRouter Chat Completions usage, with unknown quantities kept unknown.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Usage {
    pub prompt_tokens_total: Option<u64>,
    pub prompt_uncached: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    #[serde(rename = "cost_usd")]
    pub cost: Option<f64>,
}
impl Usage {
    pub(crate) fn from_raw(raw: &Value) -> Self {
        let total = raw["prompt_tokens"].as_u64();
        // Missing detail buckets on an otherwise metered response mean zero.
        let detail =
            |pointer| total.map(|_| raw.pointer(pointer).and_then(Value::as_u64).unwrap_or(0));
        let read = detail("/prompt_tokens_details/cached_tokens");
        let write = detail("/prompt_tokens_details/cache_write_tokens");
        Self {
            prompt_tokens_total: total,
            prompt_uncached: total
                .zip(read)
                .zip(write)
                .and_then(|((t, r), w)| t.checked_sub(r)?.checked_sub(w)),
            cache_read: read,
            cache_write: write,
            completion_tokens: raw["completion_tokens"].as_u64(),
            reasoning_tokens: raw["completion_tokens"].as_u64().map(|_| {
                raw.pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            }),
            cost: raw["cost"].as_f64(),
        }
    }
    pub(crate) fn from_attempts(rows: &[Value]) -> Self {
        let sum = |field: &str| rows.iter().map(|r| r[field].as_u64()).sum::<Option<u64>>();
        Self {
            prompt_tokens_total: sum("prompt_tokens_total"),
            prompt_uncached: sum("prompt_uncached"),
            cache_read: sum("cache_read"),
            cache_write: sum("cache_write"),
            completion_tokens: sum("completion_tokens"),
            reasoning_tokens: sum("reasoning_tokens"),
            // Legacy cost is retained on attempt rows for the existing grader.
            cost: rows
                .iter()
                .map(|r| r.get("cost_usd").unwrap_or(&r["cost"]).as_f64())
                .sum(),
        }
    }
}

pub(crate) fn request_sizes(body: &Value) -> Value {
    let messages = body["messages"].as_array();
    let serialized_size =
        |value: &Value| (value.to_string().len(), value.to_string().chars().count());
    let (bytes, chars) = messages
        .map(|m| serialized_size(&json!(m)))
        .unwrap_or_default();
    let select = |roles: &[&str]| {
        let selected = messages
            .into_iter()
            .flatten()
            .filter(|m| roles.contains(&m["role"].as_str().unwrap_or("")))
            .cloned()
            .collect::<Vec<_>>();
        selected
            .iter()
            .map(|m| serialized_size(&m["content"]))
            .fold((0, 0), |(b, c), (x, y)| (b + x, c + y))
    };
    let (system_bytes, system_chars) = select(&["system", "developer"]);
    let (tool_bytes, tool_chars) = select(&["tool"]);
    let tools = body.get("tools").cloned().unwrap_or(json!([]));
    let (tools_bytes, tools_chars) = serialized_size(&tools);
    json!({"system_prompt_chars":system_chars,"system_prompt_bytes":system_bytes,
        "messages_chars":chars,"messages_bytes":bytes,"tool_result_chars":tool_chars,"tool_result_bytes":tool_bytes,
        "tool_definitions_count":tools.as_array().map_or(0,Vec::len),"tool_definitions_chars":tools_chars,"tool_definitions_bytes":tools_bytes})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disjoint_cache_buckets_restore_prompt_and_reasoning_is_a_subset() {
        for (raw, uncached) in [
            (
                json!({"prompt_tokens":5030,"completion_tokens":90,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":5027},"completion_tokens_details":{"reasoning_tokens":40},"cost":0.001}),
                3,
            ),
            (
                json!({"prompt_tokens":5114,"completion_tokens":70,"prompt_tokens_details":{"cached_tokens":4608,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":20},"cost":0.002}),
                506,
            ),
        ] {
            let u = Usage::from_raw(&raw);
            assert_eq!(u.prompt_uncached, Some(uncached));
            assert_eq!(
                u.prompt_tokens_total,
                Some(uncached + u.cache_read.unwrap() + u.cache_write.unwrap())
            );
            assert_eq!(u.completion_tokens, raw["completion_tokens"].as_u64());
            let rows = vec![serde_json::to_value(&u).unwrap(); 2];
            let sum = Usage::from_attempts(&rows);
            assert_eq!(
                sum.prompt_tokens_total,
                u.prompt_tokens_total.map(|n| 2 * n)
            );
            assert_eq!(sum.cost, u.cost.map(|n| 2.0 * n));
        }
        assert!(Usage::from_raw(&Value::Null).prompt_tokens_total.is_none());
        assert!(
            Usage::from_attempts(&[json!({})])
                .prompt_tokens_total
                .is_none()
        );
        assert!(
            Usage::from_raw(
                &json!({"prompt_tokens":1,"prompt_tokens_details":{"cached_tokens":2}})
            )
            .prompt_uncached
            .is_none()
        );
    }
    #[test]
    fn sizes_distinguish_unicode_bytes_and_tool_results() {
        let sizes = request_sizes(
            &json!({"messages":[{"role":"system","content":"é"},{"role":"tool","content":"abc"}],"tools":[{}]}),
        );
        assert_eq!(sizes["system_prompt_chars"], 3);
        assert_eq!(sizes["system_prompt_bytes"], 4);
        assert_eq!(sizes["tool_result_chars"], 5);
        assert_eq!(sizes["tool_definitions_count"], 1);
    }
}

#[cfg(test)]
mod recorded_tests {
    use super::*;
    #[test]
    fn live_glm_and_sol_usage_matches_facade_disjoint_buckets() {
        let fixtures: Vec<Value> =
            serde_json::from_str(include_str!("../fixtures/openrouter-usage.json")).unwrap();
        assert!(fixtures.iter().any(|r| r["model"] == "z-ai/glm-5.3-flash"));
        assert!(fixtures.iter().any(|r| r["model"] == "openai/gpt-5.6-sol"));
        for r in fixtures {
            let u = Usage::from_raw(&r["raw_usage"]);
            assert_eq!(u.prompt_tokens_total, r["prompt_tokens_total"].as_u64());
            assert_eq!(u.prompt_uncached, r["tokens"]["input"].as_u64());
            assert_eq!(u.cache_read, r["tokens"]["cache_read"].as_u64());
            assert_eq!(u.cache_write, r["tokens"]["cache_write"].as_u64());
            assert_eq!(u.completion_tokens, r["tokens"]["output"].as_u64());
            assert_eq!(u.reasoning_tokens, r["reasoning_tokens"].as_u64());
            assert_eq!(u.cost, r["cost_usd"].as_f64());
        }
    }
}
