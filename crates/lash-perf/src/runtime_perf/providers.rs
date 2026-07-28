use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use lash_core::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmOutputSpec, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStreamEvent, LlmUsage,
};
use lash_core::testing::TestProvider;
use lash_core::{
    DirectJsonSchema, DirectRequest, Resolution, ToolContract, ToolDefinition, ToolManifest,
    ToolOutputContract, ToolProvider, ToolResult, TriggerOccurrenceRequest,
    empty_trigger_source_key,
};
#[cfg(test)]
use lash_lashlang_runtime::tool_lashlang_binding;
use lash_lashlang_runtime::{LashlangToolBinding, ToolDefinitionLashlangExt};

use super::scenarios::RuntimePerfScenario;

const OPENAI_COMPAT_STREAM_CHUNK_COUNT: usize = 256;
const OPENAI_COMPAT_STREAM_CHUNK_BYTES: usize = 96;

pub(crate) struct BenchmarkStreamProfile {
    pub(crate) full_text: String,
    pub(crate) deltas: Vec<String>,
    pub(crate) parts: Vec<LlmOutputPart>,
}

pub(crate) struct BenchmarkProviderControl {
    pub(crate) provider_started: tokio::sync::Notify,
    pub(crate) release_provider: tokio::sync::Notify,
    provider_calls: AtomicUsize,
}

impl BenchmarkProviderControl {
    fn new() -> Self {
        Self {
            provider_started: tokio::sync::Notify::new(),
            release_provider: tokio::sync::Notify::new(),
            provider_calls: AtomicUsize::new(0),
        }
    }
}

pub(crate) fn benchmark_provider(scenario: RuntimePerfScenario) -> TestProvider {
    benchmark_provider_with_control(scenario).0
}

pub(crate) fn benchmark_provider_with_control(
    scenario: RuntimePerfScenario,
) -> (TestProvider, Option<Arc<BenchmarkProviderControl>>) {
    let control = matches!(
        scenario,
        RuntimePerfScenario::TurnCancelRoundTrip | RuntimePerfScenario::IngressClaimProjection
    )
    .then(|| Arc::new(BenchmarkProviderControl::new()));
    let completion_control = control.clone();
    let provider = TestProvider::builder()
        .kind("benchmark")
        .serialize_config(move || {
            serde_json::json!({
                "scenario": scenario.name(),
            })
        })
        .requires_streaming(true)
        .complete(move |req| {
            let completion_control = completion_control.clone();
            async move {
                if matches!(scenario, RuntimePerfScenario::TurnCancelRoundTrip) {
                    completion_control
                        .as_ref()
                        .expect("cancel round-trip control")
                        .provider_started
                        .notify_one();
                    return std::future::pending().await;
                }
                let ingress_profile =
                    if matches!(scenario, RuntimePerfScenario::IngressClaimProjection) {
                        let control = completion_control
                            .as_ref()
                            .expect("ingress projection control");
                        let call_index = control.provider_calls.fetch_add(1, Ordering::SeqCst);
                        if call_index.is_multiple_of(2) {
                            control.provider_started.notify_one();
                            control.release_provider.notified().await;
                            Some(text_profile(lashlang_block(
                                r#"print("checkpoint before projection")"#,
                            )))
                        } else {
                            Some(text_profile(lashlang_block(
                                r#"finish "runtime perf benchmark ok""#,
                            )))
                        }
                    } else {
                        None
                    };
                let profile = ingress_profile
                    .unwrap_or_else(|| benchmark_stream_profile_for_request(scenario, &req));
                let usage = LlmUsage {
                    input_tokens: 1_024,
                    output_tokens: 64,
                    cache_read_input_tokens: 512,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 48,
                };
                if let Some(tx) = req.stream_events.as_ref() {
                    if profile.deltas.is_empty() {
                        for part in &profile.parts {
                            tx.send(LlmStreamEvent::Part(part.clone()));
                        }
                    } else {
                        for delta in &profile.deltas {
                            tx.send(LlmStreamEvent::Delta(delta.clone()));
                        }
                    }
                    tx.send(LlmStreamEvent::Usage(usage.clone()));
                }
                let parts = if profile.parts.is_empty() {
                    vec![LlmOutputPart::Text {
                        text: profile.full_text.clone(),
                        response_meta: None,
                    }]
                } else {
                    profile.parts
                };
                Ok(LlmResponse {
                    full_text: profile.full_text.clone(),
                    parts,
                    usage,
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    terminal_diagnostic: None,
                    provider_usage: None,
                    request_body: None,
                    http_summary: None,
                    execution_evidence: None,
                    generation_disposition: None,
                    response_metadata: Default::default(),
                })
            }
        })
        .build();
    (provider, control)
}

#[derive(Clone)]
pub(crate) struct BenchmarkEchoTool {
    completion_resolver: Arc<dyn lash_core::EffectHost>,
}

impl BenchmarkEchoTool {
    pub(crate) fn new(completion_resolver: Arc<dyn lash_core::EffectHost>) -> Self {
        Self {
            completion_resolver,
        }
    }
}

#[derive(Default)]
pub(crate) struct BenchmarkWorkbenchMailTool;

#[derive(Clone)]
pub(crate) struct BenchmarkLargeToolCatalog {
    cache: Arc<BenchmarkLargeToolCatalogCache>,
}

#[derive(Default)]
pub(crate) struct BenchmarkObliqueTools;

struct BenchmarkLargeToolCatalogCache {
    manifests: Vec<ToolManifest>,
    contracts: HashMap<String, Arc<ToolContract>>,
}

impl Default for BenchmarkLargeToolCatalog {
    fn default() -> Self {
        Self {
            cache: Arc::clone(large_tool_catalog_cache()),
        }
    }
}

fn large_tool_catalog_cache() -> &'static Arc<BenchmarkLargeToolCatalogCache> {
    static CACHE: OnceLock<Arc<BenchmarkLargeToolCatalogCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let definitions = BenchmarkLargeToolCatalog::build_tool_definitions();
        let manifests = definitions
            .iter()
            .map(ToolDefinition::manifest)
            .collect::<Vec<_>>();
        let contracts = definitions
            .iter()
            .map(|definition| {
                (
                    definition.name().to_string(),
                    Arc::new(definition.contract()) as Arc<ToolContract>,
                )
            })
            .collect::<HashMap<_, _>>();
        Arc::new(BenchmarkLargeToolCatalogCache {
            manifests,
            contracts,
        })
    })
}

#[async_trait::async_trait]
impl ToolProvider for BenchmarkEchoTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![
            benchmark_echo_tool_definition().manifest(),
            benchmark_slow_tool_definition().manifest(),
            benchmark_async_tool_definition().manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<std::sync::Arc<ToolContract>> {
        match name {
            "benchmark_echo" => Some(std::sync::Arc::new(
                benchmark_echo_tool_definition().contract(),
            )),
            "benchmark_slow" => Some(std::sync::Arc::new(
                benchmark_slow_tool_definition().contract(),
            )),
            "benchmark_async" => Some(std::sync::Arc::new(
                benchmark_async_tool_definition().contract(),
            )),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolResult {
        match call.name {
            "benchmark_echo" => execute_benchmark_echo(call).await,
            "benchmark_slow" => execute_benchmark_slow(call).await,
            "benchmark_async" => {
                execute_benchmark_async(Arc::clone(&self.completion_resolver), call).await
            }
            _ => ToolResult::err_fmt(format_args!("Unknown benchmark tool: {}", call.name)),
        }
    }
}

#[async_trait::async_trait]
impl ToolProvider for BenchmarkObliqueTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        benchmark_oblique_tool_definitions()
            .into_iter()
            .map(|definition| definition.manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        benchmark_oblique_tool_definition_for(name)
            .map(|definition| Arc::new(definition.contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolResult {
        match call.name {
            "oblique_search" => execute_oblique_search(call).await,
            "oblique_judge_candidates" => execute_oblique_judge_candidates(call).await,
            "oblique_list_async_handles" => execute_oblique_list_async_handles(call).await,
            _ => ToolResult::err_fmt(format_args!(
                "Unknown benchmark oblique tool: {}",
                call.name
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolProvider for BenchmarkWorkbenchMailTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        benchmark_mail_tool_definitions()
            .into_iter()
            .map(|definition| definition.manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        benchmark_mail_tool_definition_for(name).map(|definition| Arc::new(definition.contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolResult {
        let Some((account, operation)) = benchmark_mail_route(call.name) else {
            return ToolResult::err_fmt(format_args!(
                "Unknown benchmark workbench mail tool: {}",
                call.name
            ));
        };
        match operation {
            "send" => execute_benchmark_mail_send(call, account).await,
            "list" => ToolResult::ok(serde_json::json!({
                "account": account,
                "messages": [],
            })),
            _ => ToolResult::err_fmt(format_args!("unsupported mail operation `{operation}`")),
        }
    }
}

async fn execute_benchmark_mail_send(
    call: lash_core::ToolCall<'_>,
    account: &'static str,
) -> ToolResult {
    let title = call
        .args
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("runtime perf mail");
    let text = call
        .args
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let Some(replay_key) = call.context.replay_key() else {
        return ToolResult::err_fmt("benchmark mail send requires a replay key");
    };
    let source_key = match empty_trigger_source_key(BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE) {
        Ok(source_key) => source_key,
        Err(err) => return ToolResult::err_fmt(err.to_string()),
    };
    let message_id = format!("{account}-{replay_key}");
    let payload = serde_json::json!({
        "account": account,
        "title": title,
        "text": text,
    });
    let idempotency_key = format!("{replay_key}:mail.received:{account}");
    let _phase = call.context.named_phase("trigger.occurrence_to_delivery");
    if let Err(err) = call
        .context
        .triggers()
        .emit(
            TriggerOccurrenceRequest::new(
                BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE,
                source_key,
                payload,
                idempotency_key,
            )
            .with_source(serde_json::json!({})),
        )
        .await
    {
        return ToolResult::err_fmt(err.to_string());
    }
    ToolResult::ok(serde_json::json!({
        "account": account,
        "id": message_id,
    }))
}

pub(crate) const BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE: &str = "mail.received";

const BENCHMARK_MAIL_ACCOUNTS: [&str; 2] = ["test", "test23"];
const BENCHMARK_MAIL_OPERATIONS: [&str; 2] = ["send", "list"];

fn benchmark_mail_route(name: &str) -> Option<(&'static str, &'static str)> {
    let rest = name.strip_prefix("inbox__")?;
    for account in BENCHMARK_MAIL_ACCOUNTS {
        for operation in BENCHMARK_MAIL_OPERATIONS {
            if let Some(tail) = rest.strip_prefix(account)
                && tail.strip_prefix("__") == Some(operation)
            {
                return Some((account, operation));
            }
        }
    }
    None
}

fn benchmark_mail_tool_definition_for(name: &str) -> Option<ToolDefinition> {
    let (account, operation) = benchmark_mail_route(name)?;
    Some(benchmark_mail_tool_definition(account, operation))
}

fn benchmark_mail_tool_definitions() -> Vec<ToolDefinition> {
    let mut definitions =
        Vec::with_capacity(BENCHMARK_MAIL_ACCOUNTS.len() * BENCHMARK_MAIL_OPERATIONS.len());
    for account in BENCHMARK_MAIL_ACCOUNTS {
        for operation in BENCHMARK_MAIL_OPERATIONS {
            definitions.push(benchmark_mail_tool_definition(account, operation));
        }
    }
    definitions
}

fn benchmark_mail_tool_definition(account: &str, operation: &str) -> ToolDefinition {
    let (input_schema, output_schema, description) = match operation {
        "send" => (
            serde_json::json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["title"],
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "account": { "type": "string" },
                    "id": { "type": "string" }
                },
                "required": ["account", "id"],
                "additionalProperties": false
            }),
            "Send a deterministic benchmark message and emit mail.received.",
        ),
        "list" => (
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "account": { "type": "string" },
                    "messages": { "type": "array" }
                },
                "required": ["account", "messages"],
                "additionalProperties": false
            }),
            "List deterministic benchmark messages.",
        ),
        _ => unreachable!("benchmark mail operation must be known"),
    };
    ToolDefinition::raw(
        format!("tool:inbox__{account}__{operation}"),
        format!("inbox__{account}__{operation}"),
        format!("{description} Account inbox.{account}."),
        input_schema,
        output_schema,
    )
    .with_lashlang_binding(
        LashlangToolBinding::new(["inbox", account], operation).with_authority_type("Inbox"),
    )
}

async fn execute_benchmark_echo(call: lash_core::ToolCall<'_>) -> ToolResult {
    tokio::task::yield_now().await;
    ToolResult::ok(serde_json::json!({
        "value": call.args.get("value").cloned().unwrap_or(serde_json::Value::Null),
        "ordinal": call.args.get("ordinal").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

async fn execute_benchmark_slow(call: lash_core::ToolCall<'_>) -> ToolResult {
    let delay_ms = call
        .args
        .get("delay_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(25);
    if let Some(cancel) = call.context.cancellation_token().cloned() {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
            _ = cancel.cancelled() => return ToolResult::cancelled("benchmark_slow cancelled"),
        }
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    ToolResult::ok(serde_json::json!({
        "value": call.args.get("value").cloned().unwrap_or(serde_json::Value::Null),
        "delay_ms": delay_ms,
    }))
}

async fn execute_benchmark_async(
    completion_resolver: Arc<dyn lash_core::EffectHost>,
    call: lash_core::ToolCall<'_>,
) -> ToolResult {
    let key = match call.context.completion_key().await {
        Ok(key) => key,
        Err(err) => return ToolResult::err_fmt(err),
    };
    let delay_ms = call
        .args
        .get("delay_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let value = call
        .args
        .get("value")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    tokio::spawn(async move {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        let _ = completion_resolver
            .resolve_await_event(
                &key,
                Resolution::Ok(serde_json::json!({
                    "value": value,
                    "mode": "pending_completion",
                    "delay_ms": delay_ms
                })),
            )
            .await;
    });
    ToolResult::pending(lash_core::PendingCompletion::new())
}

fn benchmark_echo_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:benchmark_echo",
        "benchmark_echo",
        "Return the input payload with a tiny async yield for runtime profiling.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": ["string", "number", "boolean", "object", "array", "null"] },
                "ordinal": { "type": "integer" }
            },
            "additionalProperties": true
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": {},
                "ordinal": {
                    "anyOf": [
                        { "type": "integer" },
                        { "type": "null" }
                    ]
                }
            },
            "required": ["value", "ordinal"],
            "additionalProperties": false
        }),
    )
    .with_lashlang_binding(
        LashlangToolBinding::new(["tools"], "benchmark_echo").with_authority_type("Tools"),
    )
}

fn benchmark_slow_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:benchmark_slow",
        "benchmark_slow",
        "Sleep briefly before returning; used to profile process handle cancellation and await paths.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": ["string", "number", "boolean", "object", "array", "null"] },
                "delay_ms": { "type": "integer", "minimum": 0, "maximum": 1000 }
            },
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": {},
                "delay_ms": { "type": "integer", "minimum": 0 }
            },
            "required": ["value", "delay_ms"],
            "additionalProperties": false
        }),
    )
    .with_lashlang_binding(
        LashlangToolBinding::new(["tools"], "benchmark_slow").with_authority_type("Tools"),
    )
}

fn benchmark_async_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:benchmark_async",
        "benchmark_async",
        "Return asynchronously through the host AwaitEvent completion path.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": ["string", "number", "boolean", "object", "array", "null"] },
                "delay_ms": { "type": "integer", "minimum": 0 }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": {},
                "mode": { "type": "string" },
                "delay_ms": { "type": "integer", "minimum": 0 }
            },
            "required": ["value", "mode", "delay_ms"],
            "additionalProperties": false
        }),
    )
    .with_lashlang_binding(
        LashlangToolBinding::new(["tools"], "benchmark_async").with_authority_type("Tools"),
    )
}

fn benchmark_oblique_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        benchmark_oblique_search_tool_definition(),
        benchmark_oblique_judge_tool_definition(),
        benchmark_oblique_list_handles_tool_definition(),
    ]
}

fn benchmark_oblique_tool_definition_for(name: &str) -> Option<ToolDefinition> {
    match name {
        "oblique_search" => Some(benchmark_oblique_search_tool_definition()),
        "oblique_judge_candidates" => Some(benchmark_oblique_judge_tool_definition()),
        "oblique_list_async_handles" => Some(benchmark_oblique_list_handles_tool_definition()),
        _ => None,
    }
}

fn benchmark_oblique_search_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:oblique_search",
        "oblique_search",
        "Synthetic OBLIQ retrieval. Returns a ranked match list with full text and nested metadata inline.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "minItems": 1,
                    "maxItems": 12
                },
                "mode": { "type": "string", "enum": ["hybrid", "bm25", "dense", "late"], "default": "hybrid" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 300, "default": 96 },
                "candidate_pool": { "type": "integer", "minimum": 1, "maximum": 2000, "default": 512 }
            },
            "required": ["queries"],
            "additionalProperties": false
        }),
        oblique_search_output_schema(),
    )
    .with_lashlang_binding(
        LashlangToolBinding::new(["obliq"], "search").with_authority_type("Obliq"),
    )
}

fn benchmark_oblique_judge_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:oblique_judge_candidates",
        "oblique_judge_candidates",
        "Synthetic OBLIQ candidate judge. Calls the direct completion client from inside the tool.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "verifier_predicate": { "type": "string", "minLength": 1 },
                "candidate_doc_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "maxItems": 600
                },
                "surface_bait": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 20
                }
            },
            "required": ["verifier_predicate", "candidate_doc_ids"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "ranked_doc_ids": { "type": "array", "items": { "type": "string" } },
                "positive_doc_ids": { "type": "array", "items": { "type": "string" } },
                "direct_completion": { "type": "object", "additionalProperties": true }
            },
            "required": ["ranked_doc_ids", "positive_doc_ids", "direct_completion"],
            "additionalProperties": false
        }),
    )
    .with_lashlang_binding(
        LashlangToolBinding::new(["obliq"], "judge_candidates").with_authority_type("Obliq"),
    )
}

fn benchmark_oblique_list_handles_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:oblique_list_async_handles",
        "oblique_list_async_handles",
        "Synthetic live async handle listing shaped like the OBLIQ helper tool.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "monitor": { "type": "object", "additionalProperties": true },
                "subagent": { "type": "object", "additionalProperties": true },
                "tool": { "type": "object", "additionalProperties": true }
            },
            "required": ["monitor", "subagent", "tool"],
            "additionalProperties": false
        }),
    )
    .with_lashlang_binding(
        LashlangToolBinding::new(["obliq"], "list_async_handles").with_authority_type("Obliq"),
    )
}

fn oblique_search_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "matches": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "rank": { "type": "integer" },
                        "doc_id": { "type": "string" },
                        "score": { "type": "number" },
                        "text": { "type": "string" },
                        "metadata": { "type": "object", "additionalProperties": true }
                    },
                    "required": ["rank", "doc_id", "score", "text", "metadata"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["matches"],
        "additionalProperties": false
    })
}

async fn execute_oblique_search(call: lash_core::ToolCall<'_>) -> ToolResult {
    tokio::task::yield_now().await;
    let limit = call
        .args
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(96)
        .clamp(1, 128) as usize;
    let query_count = call
        .args
        .get("queries")
        .and_then(serde_json::Value::as_array)
        .map(|queries| queries.len().max(1))
        .unwrap_or(1);
    let mode = call
        .args
        .get("mode")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("hybrid");
    let matches = (0..limit)
        .map(|index| {
            serde_json::json!({
                "rank": index + 1,
                "doc_id": format!("doc_{query_count}_{index:04}"),
                "score": 1.0 / (index + 1) as f64,
                "text": oblique_doc_text(index),
                "metadata": {
                    "mode": mode,
                    "source": {
                        "subset": "math",
                        "family": format!("latent-pattern-{}", index % 8),
                        "path": ["obliq", "analogues", "math"]
                    },
                    "signals": {
                        "dense": 0.72,
                        "bm25": 13.5 + index as f64,
                        "late_interaction": index % 3 == 0
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    ToolResult::ok(serde_json::json!({ "matches": matches }))
}

async fn execute_oblique_judge_candidates(call: lash_core::ToolCall<'_>) -> ToolResult {
    let candidate_ids = call
        .args
        .get("candidate_doc_ids")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .take(64)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if candidate_ids.is_empty() {
        return ToolResult::err_fmt("oblique_judge_candidates requires candidate_doc_ids");
    }
    let completion = match call
        .context
        .direct_completions()
        .complete(
            oblique_judge_direct_request(&candidate_ids),
            "oblique_judge",
        )
        .await
    {
        Ok(completion) => completion,
        Err(err) => return ToolResult::err_fmt(err.to_string()),
    };
    let direct_completion = serde_json::from_str(&completion.text).unwrap_or_else(|_| {
        serde_json::json!({
            "text": completion.text
        })
    });
    let ranked_doc_ids = candidate_ids.iter().take(24).cloned().collect::<Vec<_>>();
    let positive_doc_ids = candidate_ids
        .iter()
        .step_by(5)
        .take(8)
        .cloned()
        .collect::<Vec<_>>();
    ToolResult::ok(serde_json::json!({
        "ranked_doc_ids": ranked_doc_ids,
        "positive_doc_ids": positive_doc_ids,
        "direct_completion": direct_completion,
    }))
}

async fn execute_oblique_list_async_handles(_call: lash_core::ToolCall<'_>) -> ToolResult {
    tokio::task::yield_now().await;
    ToolResult::ok(serde_json::json!({
        "monitor": { "rerank": { "__handle__": "monitor", "id": "rerank-monitor" } },
        "subagent": { "explore": { "__handle__": "subagent", "id": "explore-subagent" } },
        "tool": { "search": { "__handle__": "tool", "id": "search-tool" } }
    }))
}

fn oblique_judge_direct_request(candidate_ids: &[String]) -> DirectRequest {
    DirectRequest::json_schema(
        "mock-model",
        format!(
            "Judge {} synthetic OBLIQ candidates and return stable JSON.",
            candidate_ids.len()
        ),
        DirectJsonSchema {
            name: "runtime_perf_oblique_judge".to_string(),
            strict: true,
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["ranked_doc_ids", "rationale"],
                "properties": {
                    "ranked_doc_ids": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "rationale": { "type": "string" }
                }
            })
            .into(),
        },
    )
}

fn oblique_doc_text(index: usize) -> String {
    const CHUNK: &str = "abstract strategy transfers across topics: compare latent operator, invariant, proof sketch, distractor surface, and retrieval evidence. ";
    let mut text = format!("Document {index:04}. ");
    for repeat in 0..8 {
        text.push_str(CHUNK);
        text.push_str(&format!("segment={repeat}; "));
    }
    text
}

#[async_trait::async_trait]
impl ToolProvider for BenchmarkLargeToolCatalog {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        self.cache.manifests.clone()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.cache.contracts.get(name).cloned()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolResult {
        if !GMAIL_LIKE_TOOL_NAMES.contains(&call.name) {
            return ToolResult::err_fmt(format_args!("Unknown benchmark tool: {}", call.name));
        }
        tokio::task::yield_now().await;
        ToolResult::ok(serde_json::json!({
            "tool": call.name,
            "ok": true,
            "echo": call.args,
        }))
    }
}

impl BenchmarkLargeToolCatalog {
    fn build_tool_definitions() -> Vec<ToolDefinition> {
        GMAIL_LIKE_TOOL_NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| gmail_like_tool_definition(index, name))
            .collect()
    }
}

fn gmail_like_tool_definition(index: usize, name: &str) -> ToolDefinition {
    let mut definition = ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        gmail_like_tool_description(index, name),
        gmail_like_input_schema(name),
        gmail_like_output_schema(name),
    )
    .with_examples(vec![
        format!(
            r#"call {name} {{ user_id: "me", message_id: "msg_123", payload: {{ label_ids: ["INBOX", "IMPORTANT"] }} }}"#
        ),
        format!(
            r#"call {name} {{ user_id: "me", query: "from:alerts@example.com newer_than:7d", limit: 25 }}"#
        ),
    ])
    .with_lashlang_binding(
        LashlangToolBinding::new(
            ["gmail"],
            name.trim_start_matches("GMAIL_").to_ascii_lowercase(),
        )
        .with_aliases([name
            .trim_start_matches("GMAIL_")
            .to_ascii_lowercase()
            .replace('_', " ")]),
    );

    if index.is_multiple_of(7) {
        definition.contract.output_contract = ToolOutputContract::from_input_schema(
            "projection",
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "status": { "type": "string" }
                },
                "additionalProperties": true
            })),
        );
    }

    definition
}

fn gmail_like_tool_description(index: usize, name: &str) -> String {
    let verb = name
        .trim_start_matches("GMAIL_")
        .to_ascii_lowercase()
        .replace('_', " ");
    format!(
        "Synthetic Gmail toolkit operation #{index}: {verb}. Mirrors a real provider action with OAuth-scoped Gmail semantics, mailbox resource identifiers, optional label/filter/thread payloads, and structured result projection metadata. The description is intentionally long enough to exercise prompt-side tool documentation, compact catalog rendering, and RLM callable surface construction for provider-sized toolkits."
    )
}

fn gmail_like_input_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "$defs": {
            "email_address": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Display name for the mailbox participant." },
                    "email": { "type": "string", "format": "email", "description": "RFC 5322 email address." }
                },
                "required": ["email"],
                "additionalProperties": false
            },
            "message_part": {
                "type": "object",
                "properties": {
                    "mime_type": { "type": "string" },
                    "filename": { "type": "string" },
                    "headers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "value": { "type": "string" }
                            },
                            "required": ["name", "value"],
                            "additionalProperties": false
                        }
                    },
                    "body": {
                        "type": "object",
                        "properties": {
                            "size": { "type": "integer" },
                            "data": { "type": "string" },
                            "attachment_id": { "type": "string" }
                        },
                        "additionalProperties": false
                    },
                    "parts": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/message_part" }
                    }
                },
                "additionalProperties": false
            },
            "label_mutation": {
                "type": "object",
                "properties": {
                    "add_label_ids": { "type": "array", "items": { "type": "string" }, "maxItems": 50 },
                    "remove_label_ids": { "type": "array", "items": { "type": "string" }, "maxItems": 50 }
                },
                "additionalProperties": false
            }
        },
        "properties": {
            "user_id": {
                "type": "string",
                "description": "Gmail user id. Use `me` for the authenticated user.",
                "default": "me"
            },
            "message_id": {
                "type": "string",
                "description": "Gmail message, thread, draft, label, filter, or settings resource id."
            },
            "thread_id": { "type": "string" },
            "query": {
                "type": "string",
                "description": "Search query, label expression, email address, or filter criteria."
            },
            "projection": {
                "description": "Optional output type witness for tools whose response should be compacted to selected fields.",
                "anyOf": [
                    { "type": "string", "enum": ["summary", "full", "ids_only"] },
                    {
                        "type": "object",
                        "properties": {
                            "fields": { "type": "array", "items": { "type": "string" } },
                            "include_headers": { "type": "boolean" },
                            "include_body": { "type": "boolean" }
                        },
                        "additionalProperties": false
                    }
                ]
            },
            "payload": {
                "description": "Operation-specific Gmail request payload.",
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "raw": { "type": "string", "description": "Base64url encoded RFC 2822 message." },
                            "subject": { "type": "string" },
                            "body": {
                                "type": "object",
                                "properties": {
                                    "plain": { "type": "string" },
                                    "html": { "type": "string" }
                                },
                                "additionalProperties": false
                            },
                            "to": { "type": "array", "items": { "$ref": "#/$defs/email_address" } },
                            "cc": { "type": "array", "items": { "$ref": "#/$defs/email_address" } },
                            "bcc": { "type": "array", "items": { "$ref": "#/$defs/email_address" } },
                            "attachments": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "filename": { "type": "string" },
                                        "mime_type": { "type": "string" },
                                        "content_base64": { "type": "string" },
                                        "document_id": { "type": "string" }
                                    },
                                    "additionalProperties": false
                                }
                            }
                        },
                        "additionalProperties": false
                    },
                    { "$ref": "#/$defs/label_mutation" },
                    {
                        "type": "object",
                        "properties": {
                            "criteria": {
                                "type": "object",
                                "properties": {
                                    "from": { "type": "string" },
                                    "to": { "type": "string" },
                                    "subject": { "type": "string" },
                                    "has_attachment": { "type": "boolean" },
                                    "query": { "type": "string" }
                                },
                                "additionalProperties": false
                            },
                            "action": { "$ref": "#/$defs/label_mutation" }
                        },
                        "additionalProperties": false
                    },
                    { "$ref": "#/$defs/message_part" }
                ]
            },
            "page_token": { "type": "string" },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 500,
                "description": "Maximum number of records to inspect or mutate."
            },
            "include_spam_trash": {
                "type": "boolean",
                "description": "Whether to include spam and trash folders when the Gmail API supports it."
            },
            "labels": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "visibility": { "type": "string", "enum": ["labelShow", "labelHide", "labelShowIfUnread"] },
                        "color": {
                            "type": "object",
                            "properties": {
                                "text_color": { "type": "string" },
                                "background_color": { "type": "string" }
                            },
                            "additionalProperties": false
                        }
                    },
                    "additionalProperties": false
                }
            },
            "operation": {
                "type": "string",
                "const": name
            }
        },
        "required": ["user_id"],
        "additionalProperties": false
    })
}

fn gmail_like_output_schema(_name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "status": { "type": "string", "enum": ["ok", "queued", "partial", "not_modified"] },
            "messages": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "thread_id": { "type": "string" },
                        "label_ids": { "type": "array", "items": { "type": "string" } },
                        "snippet": { "type": "string" },
                        "payload": {
                            "type": "object",
                            "properties": {
                                "mime_type": { "type": "string" },
                                "headers": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "name": { "type": "string" },
                                            "value": { "type": "string" }
                                        },
                                        "additionalProperties": false
                                    }
                                },
                                "body": { "type": "object", "additionalProperties": true },
                                "parts": {
                                    "type": "array",
                                    "items": { "type": "object", "additionalProperties": true }
                                }
                            },
                            "additionalProperties": false
                        }
                    },
                    "additionalProperties": false
                }
            },
            "thread": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "history_id": { "type": "string" },
                    "messages": { "type": "array", "items": { "type": "object", "additionalProperties": true } }
                },
                "additionalProperties": false
            },
            "metadata": {
                "type": "object",
                "properties": {
                    "next_page_token": { "type": "string" },
                    "result_size_estimate": { "type": "integer" },
                    "rate_limit": {
                        "type": "object",
                        "properties": {
                            "remaining": { "type": "integer" },
                            "reset_at": { "type": "string", "format": "date-time" }
                        },
                        "additionalProperties": false
                    }
                },
                "additionalProperties": true
            }
        },
        "additionalProperties": true
    })
}

const GMAIL_LIKE_TOOL_NAMES: &[&str] = &[
    "GMAIL_ADD_LABEL_TO_EMAIL",
    "GMAIL_BATCH_DELETE_MESSAGES",
    "GMAIL_BATCH_MODIFY_MESSAGES",
    "GMAIL_CREATE_EMAIL_DRAFT",
    "GMAIL_CREATE_FILTER",
    "GMAIL_CREATE_LABEL",
    "GMAIL_CREATE_PROMPT_POST",
    "GMAIL_DELETE_DRAFT",
    "GMAIL_DELETE_FILTER",
    "GMAIL_DELETE_LABEL",
    "GMAIL_DELETE_MESSAGE",
    "GMAIL_DELETE_THREAD",
    "GMAIL_FETCH_EMAILS",
    "GMAIL_FETCH_MESSAGE_BY_MESSAGE_ID",
    "GMAIL_FETCH_MESSAGE_BY_THREAD_ID",
    "GMAIL_FORWARD_MESSAGE",
    "GMAIL_GET_ATTACHMENT",
    "GMAIL_GET_AUTO_FORWARDING",
    "GMAIL_GET_CONTACTS",
    "GMAIL_GET_DRAFT",
    "GMAIL_GET_FILTER",
    "GMAIL_GET_LABEL",
    "GMAIL_GET_LANGUAGE_SETTINGS",
    "GMAIL_GET_PEOPLE",
    "GMAIL_GET_PROFILE",
    "GMAIL_GET_VACATION_SETTINGS",
    "GMAIL_IMPORT_MESSAGE",
    "GMAIL_INSERT_MESSAGE",
    "GMAIL_LIST_CSE_IDENTITIES",
    "GMAIL_LIST_CSE_KEYPAIRS",
    "GMAIL_LIST_DRAFTS",
    "GMAIL_LIST_FILTERS",
    "GMAIL_LIST_FORWARDING_ADDRESSES",
    "GMAIL_LIST_HISTORY",
    "GMAIL_LIST_LABELS",
    "GMAIL_LIST_MESSAGES",
    "GMAIL_LIST_SEND_AS",
    "GMAIL_LIST_SMIME_INFO",
    "GMAIL_LIST_THREADS",
    "GMAIL_MODIFY_THREAD_LABELS",
    "GMAIL_MOVE_THREAD_TO_TRASH",
    "GMAIL_MOVE_TO_TRASH",
    "GMAIL_PATCH_LABEL",
    "GMAIL_PATCH_SEND_AS",
    "GMAIL_REMOVE_LABEL",
    "GMAIL_REPLY_TO_THREAD",
    "GMAIL_SEARCH_PEOPLE",
    "GMAIL_SEND_DRAFT",
    "GMAIL_SEND_EMAIL",
    "GMAIL_SETTINGS_GET_IMAP",
    "GMAIL_SETTINGS_GET_POP",
    "GMAIL_SETTINGS_SEND_AS_GET",
    "GMAIL_STOP_WATCH",
    "GMAIL_UNTRASH_MESSAGE",
    "GMAIL_UNTRASH_THREAD",
    "GMAIL_UPDATE_DRAFT",
    "GMAIL_UPDATE_IMAP_SETTINGS",
    "GMAIL_UPDATE_LABEL",
    "GMAIL_UPDATE_LANGUAGE_SETTINGS",
    "GMAIL_UPDATE_POP_SETTINGS",
    "GMAIL_UPDATE_SEND_AS",
    "GMAIL_UPDATE_USER_ATTRIBUTES_VALUES",
    "GMAIL_UPDATE_VACATION_SETTINGS",
];

pub(crate) fn benchmark_stream_profile(scenario: RuntimePerfScenario) -> BenchmarkStreamProfile {
    benchmark_stream_profile_for_request(scenario, &empty_request())
}

fn benchmark_stream_profile_for_request(
    scenario: RuntimePerfScenario,
    request: &LlmRequest,
) -> BenchmarkStreamProfile {
    if matches!(
        scenario,
        RuntimePerfScenario::RlmSubagentSpawn
            | RuntimePerfScenario::RlmObliqueStackMix
            | RuntimePerfScenario::DeepTurnComposition
    ) && request_text(request).contains("Subagent capability: default. Depth: 1/5.")
    {
        if matches!(scenario, RuntimePerfScenario::DeepTurnComposition) {
            return text_profile(lashlang_block(
                r#"
sleep for "0ms"
result = await tools.benchmark_async({ value: len(chunk), delay_ms: 0 })?
finish { len: result.value }"#,
            ));
        }
        return text_profile(lashlang_block("finish { len: len(chunk) }"));
    }

    if matches!(
        scenario,
        RuntimePerfScenario::ObservationalMemoryMaintenance
    ) && request.session_id().ends_with("-om-observer")
    {
        return text_profile(
            "<observations>Runtime perf observer captured persisted benchmark messages.</observations>\n\
             <current-task>Measure post-persist observational memory maintenance.</current-task>\n\
             <suggested-response>Report the runtime perf benchmark marker.</suggested-response>",
        );
    }

    if request.output_spec.is_some() || request.session_id().ends_with("-llm-query") {
        if request.output_spec.as_ref().is_some_and(|spec| {
            matches!(spec, LlmOutputSpec::JsonSchema(schema) if schema.name == "tool_search_rerank")
        }) {
            return text_profile(
                serde_json::json!({
                    "tool_names": [
                        "GMAIL_SEND_EMAIL",
                        "GMAIL_CREATE_EMAIL_DRAFT",
                        "GMAIL_LIST_MESSAGES",
                        "exec_command"
                    ]
                })
                .to_string(),
            );
        }
        if request.output_spec.as_ref().is_some_and(|spec| {
            matches!(spec, LlmOutputSpec::JsonSchema(schema) if schema.name == "runtime_perf_oblique_judge")
        }) {
            return text_profile(
                serde_json::json!({
                    "ranked_doc_ids": ["doc_4_0000", "doc_4_0005", "doc_4_0010"],
                    "rationale": "synthetic direct judge response"
                })
                .to_string(),
            );
        }
        return text_profile(
            serde_json::json!({
                "kind": "value",
                "value": "runtime perf benchmark ok",
                "error": null,
            })
            .to_string(),
        );
    }

    match scenario {
        RuntimePerfScenario::OpenAiCompatStream => {
            let alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
            let mut deltas = Vec::with_capacity(OPENAI_COMPAT_STREAM_CHUNK_COUNT + 1);
            for index in 0..OPENAI_COMPAT_STREAM_CHUNK_COUNT {
                let prefix = format!("chunk-{index:03}: ");
                let fill_len = OPENAI_COMPAT_STREAM_CHUNK_BYTES.saturating_sub(prefix.len() + 1);
                let body: String = alphabet
                    .chars()
                    .cycle()
                    .skip(index % alphabet.len())
                    .take(fill_len)
                    .collect();
                deltas.push(format!("{prefix}{body}\n"));
            }
            deltas.push("runtime perf benchmark ok".to_string());
            BenchmarkStreamProfile {
                full_text: deltas.concat(),
                deltas,
                parts: Vec::new(),
            }
        }
        RuntimePerfScenario::StandardToolCalls => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-batch-call",
                    "batch",
                    serde_json::json!({
                        "tool_calls": [
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 1,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 2,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 3,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 4,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 5,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 6,
                                }
                            }
                        ]
                    }),
                )
            }
        }
        RuntimePerfScenario::StandardAsyncToolCompletion => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-async-completion-call",
                    "benchmark_async",
                    serde_json::json!({
                        "value": "runtime perf benchmark ok"
                    }),
                )
            }
        }
        RuntimePerfScenario::StandardShellOutput => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-shell-output-call",
                    "exec_command",
                    serde_json::json!({
                        "cmd": "for i in $(seq 1 160); do printf 'runtime-perf-shell-line-%03d abcdefghijklmnopqrstuvwxyz0123456789\\n' \"$i\"; done",
                        "timeout_ms": 5000,
                        "max_output_tokens": 4096
                    }),
                )
            }
        }
        RuntimePerfScenario::ToolDiscoverySearch => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-tool-discovery-call",
                    "search_tools",
                    serde_json::json!({
                        "query": "gmail send email draft label message search",
                        "module": "gmail",
                        "limit": 8
                    }),
                )
            }
        }
        RuntimePerfScenario::Rlm
        | RuntimePerfScenario::RlmLargeToolCatalog
        | RuntimePerfScenario::EmbedRlm
        | RuntimePerfScenario::TraceJsonlExtended => {
            let text = lashlang_block(r#"finish "runtime perf benchmark ok""#);
            text_profile(text)
        }
        RuntimePerfScenario::RlmStreamedPairedLashlang => {
            let full_text = concat!(
                "Visible preface before executable code.\n",
                "<lashlang>\n",
                "value = \"runtime perf benchmark ok\"\n",
                "finish value\n",
                "</lashlang>\n",
                "This suffix must be ignored after the close tag."
            )
            .to_string();
            BenchmarkStreamProfile {
                full_text,
                deltas: vec![
                    "Visible preface before executable code.\n<lash".to_string(),
                    "lang>\nvalue = \"runtime perf benchmark ok\"\n".to_string(),
                    "finish value\n</lash".to_string(),
                    "lang>\nThis suffix must be ignored after the close tag.".to_string(),
                ],
                parts: Vec::new(),
            }
        }
        RuntimePerfScenario::RlmGlobals => {
            // Mix of small (inline) and large (degraded preview) globals so the
            // benchmark exercises both branches of `render_bound_variables`:
            // small values render in full, while `big_map`/`big_notes`/`big_text`
            // exceed the inline budget and go through the keys / head-tail
            // truncation path that runs on every prompt build.
            let text = lashlang_block(
                r#"
big_map = {}
for i in range(24) {
  big_map[format("room_{}", i)] = { exits: ["north", "south", "east"], items: [format("item_{}", i)] }
}
big_notes = []
for i in range(45) {
  big_notes = push(big_notes, format("note {}: long observation about world state, plan, and next steps", i))
}
big_text = "Loud Room: "
for i in range(40) {
  big_text = format("{}echo step {} dampens the acoustics; ", big_text, i)
}
live_record = {
  status: "ready",
  turn: input.turn,
  goal: input.goal,
  nested: {
    path: input.path,
    labels: ["runtime", "rlm", "globals"],
    counters: { first: 1, second: 2, third: 3 }
  }
}
live_list = [
  { name: "alpha", count: 1 },
  { name: "beta", count: 2 },
  { name: "gamma", count: 3 }
]
live_message = "runtime perf benchmark ok"
host_snapshot = { benchmark: benchmark, input: input, chat: chat }
finish live_message"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmLargePrint => {
            let text = lashlang_block(
                r#"
big_text = ""
for i in range(70) {
  big_text = format("{}line {}: abcdefghijklmnopqrstuvwxyz0123456789 abcdefghijklmnopqrstuvwxyz0123456789\n", big_text, i)
}

rows = []
for i in range(16) {
  rows = push(rows, {
    id: format("row_{}", i),
    status: "ok",
    exit_code: 0,
    stderr: "short diagnostic stderr remains visible",
    output: big_text
  })
}

payload = {
  status: "ok",
  error: null,
  exit_code: 0,
  stderr: "short diagnostic stderr remains visible",
  output: big_text,
  rows: rows,
  metadata: {
    scenario: "rlm_large_print",
    turn: 1,
    tags: ["runtime", "projection", "print"]
  }
}

result = await tools.benchmark_echo({ value: payload, ordinal: 1 })?
print result
finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmToolCalls => {
            let text = lashlang_block(
                r#"
first = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 1 })?
second = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 2 })?
third = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 3 })?
fourth = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 4 })?
finish first.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmAsyncToolCompletion => {
            let text = lashlang_block(
                r#"
first = await tools.benchmark_async({ value: "runtime perf benchmark ok", delay_ms: 0 })?
second = await tools.benchmark_async({ value: "runtime perf benchmark ok", delay_ms: 0 })?
finish first.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmProcessHandles => {
            let text = lashlang_block(
                r#"
process benchmark_echo_process(tool: Tools, value: str, ordinal: int) {
  result = await tool.benchmark_echo({ value: value, ordinal: ordinal })?
  finish result
}

process benchmark_slow_process(tool: Tools, value: str, delay_ms: int) {
  result = await tool.benchmark_slow({ value: value, delay_ms: delay_ms })?
  finish result
}

first = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 1)
second = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 2)
slow = start benchmark_slow_process(tool: tools, value: "cancelled", delay_ms: 50)
live = await processes.list({})?
cancel slow
first_result = (await first)?
second_result = (await second)?
finish first_result.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmTriggerMailPipeline => {
            let text = lashlang_block(
                r#"
@label(title: "Define and register mail forwarder")
process forward_mail(event: mail.Received) {
  if event.account == "test" {
    await inbox.test23.send({
      title: format("[Fwd from test] {}", event.title),
      text: event.text
    })?
  }
  finish true
}

existing = await triggers.list({
  name: "runtime-perf-test-to-test23-forwarder",
  enabled: true
})?

if len(existing) > 0 {
  handle = existing[0]
} else {
  handle = await triggers.register({
    source: mail.received({}),
    target: forward_mail,
    inputs: { event: trigger.event },
    name: "runtime-perf-test-to-test23-forwarder"
  })?
}

@label(title: "Send test message to inbox.test")
sent = await inbox.test.send({
  title: "Hello from test",
  text: "This is a forwarding test for runtime perf stack profiling."
})?

finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => {
            let text = lashlang_block(
                r#"
process benchmark_async_process(tool: Tools, value: str) {
  result = await tool.benchmark_async({ value: value, delay_ms: 0 })?
  finish result
}

first = start benchmark_async_process(tool: tools, value: "runtime perf benchmark ok")
second = start benchmark_async_process(tool: tools, value: "runtime perf benchmark ok")
first_result = (await first)?
second_result = (await second)?
finish first_result.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmSubagentSpawn => {
            let text = lashlang_block(
                r#"
process spawn_child(agents: Agents) {
  result = await agents.spawn({
    capability: "default",
    task: "Submit `{ len: len(chunk) }` using the seeded `chunk` variable.",
    seed: { chunk: ["alpha", "beta", "gamma"] },
    output: Type { len: int }
  })?
  finish result
}

handle = start spawn_child(agents: agents)
result = (await handle)?
finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmObliqueStackMix => {
            let text = lashlang_block(
                r#"
process explore(agents: Agents) {
  result = await agents.spawn({
    capability: "default",
    task: "Return `{ len: len(chunk) }` using the seeded chunk.",
    seed: { chunk: ["obliq", "retrieval", "rerank", "trace"] },
    output: Type { len: int }
  })?
  finish result
}

first_pool = await obliq.search({
  queries: [
    "latent algebraic invariant transfer",
    "proof strategy analogue with distractor wording",
    "operator comparison under hidden structure",
    "same abstract solution different surface topic"
  ],
  mode: "hybrid",
  limit: 72,
  candidate_pool: 512
})?

second_pool = await obliq.search({
  queries: [
    "geometric proof reused as combinatorial invariant",
    "relevance by method not vocabulary",
    "avoid surface lexical overlap"
  ],
  mode: "hybrid",
  limit: 72,
  candidate_pool: 512
})?

candidate_ids = [match.doc_id for match in first_pool.matches]
for match in second_pool.matches {
  candidate_ids = push(candidate_ids, match.doc_id)
}

subagent_handle = start explore(agents: agents)
handles = await obliq.list_async_handles({})?
judged = await obliq.judge_candidates({
  verifier_predicate: "documents share the same abstract proof strategy, not topic words",
  candidate_doc_ids: candidate_ids,
  surface_bait: ["same vocabulary", "same topic"]
})?
subagent = (await subagent_handle)?

print {
  first_pool: first_pool,
  second_pool: second_pool,
  handles: handles,
  judged: judged,
  subagent: subagent
}

finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::IngressClaimProjection => {
            if request_text(request).contains("ingress projection marker") {
                text_profile(lashlang_block(r#"finish "runtime perf benchmark ok""#))
            } else {
                text_profile(lashlang_block(r#"print("checkpoint before projection")"#))
            }
        }
        RuntimePerfScenario::DeepTurnComposition => {
            if request_text(request).contains("deep composition ingress marker") {
                return text_profile(lashlang_block(r#"finish "runtime perf benchmark ok""#));
            }
            let text = lashlang_block(
                r#"
process deep_child(agents: Agents, tool: Tools) {
  pending = await tool.benchmark_async({ value: "parent tool loop", delay_ms: 0 })?
  sleep for "0ms"
  child = await agents.spawn({
    capability: "default",
    task: "Use the seeded chunk and return its length after the durable waits.",
    seed: { chunk: ["parent", "child", "tool", "wait"] },
    output: Type { len: int }
  })?
  finish { pending: pending.value, child: child.len }
}

handle = start deep_child(agents: agents, tool: tools)
result = (await handle)?
print result"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmLlmQuery => {
            let text = lashlang_block(
                r#"
result = await llm.query({
  task: "Return the exact benchmark marker.",
  inputs: { marker: "runtime perf benchmark ok" }
})?
finish result"#,
            );
            text_profile(text)
        }
        _ => text_profile("runtime perf benchmark ok"),
    }
}

fn lashlang_block(source: &str) -> String {
    format!("<lashlang>\n{}\n</lashlang>", source.trim())
}

fn text_profile(text: impl Into<String>) -> BenchmarkStreamProfile {
    let text = text.into();
    BenchmarkStreamProfile {
        full_text: text.clone(),
        deltas: vec![text],
        parts: Vec::new(),
    }
}

fn tool_call_profile(
    call_id: impl Into<String>,
    tool_name: impl Into<String>,
    args: serde_json::Value,
) -> BenchmarkStreamProfile {
    BenchmarkStreamProfile {
        full_text: String::new(),
        deltas: Vec::new(),
        parts: vec![LlmOutputPart::ToolCall {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            input_json: args.to_string(),
            replay: None,
        }],
    }
}

fn request_has_tool_result(request: &LlmRequest) -> bool {
    request.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
    })
}

fn request_text(request: &LlmRequest) -> String {
    let mut out = String::new();
    for message in &request.messages {
        for block in message.blocks.iter() {
            match block {
                LlmContentBlock::Text { text, .. } => out.push_str(text),
                LlmContentBlock::ToolResult { content, .. } => out.push_str(content),
                LlmContentBlock::ToolCall {
                    tool_name,
                    input_json,
                    ..
                } => {
                    out.push_str(tool_name);
                    out.push_str(input_json);
                }
                LlmContentBlock::Reasoning { text, .. } => out.push_str(text),
                LlmContentBlock::Attachment { .. } => {}
            }
            out.push('\n');
        }
    }
    out
}

fn empty_request() -> LlmRequest {
    LlmRequest {
        model: "mock-model".to_string(),
        messages: Vec::new(),
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: std::sync::Arc::new(Vec::new()),
        tool_choice: Default::default(),
        generation: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        scope: LlmRequestScope::new(
            "runtime-perf-empty".to_string(),
            "runtime-perf-empty:frame".to_string(),
            "runtime-perf-empty:request".to_string(),
        ),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{ToolCatalogBuildInput, build_tool_catalog};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn large_tool_catalog_fixture_matches_gmail_sized_callable_catalog() {
        let defs = BenchmarkLargeToolCatalog::build_tool_definitions();
        assert_eq!(defs.len(), 63);
        assert!(defs.iter().all(|def| {
            let binding = tool_lashlang_binding(&def.manifest)
                .expect("valid lashlang binding")
                .expect("benchmark tool has lashlang binding");
            binding.module_path == vec!["gmail".to_string()]
                && !def.contract.input_schema.canonical["properties"]
                    .as_object()
                    .expect("object schema")
                    .is_empty()
        }));
        assert!(
            defs.iter()
                .any(|def| !def.contract.output_contract.is_static()),
            "fixture should cover dynamic output contracts"
        );
        let first = defs.first().expect("fixture tool");
        assert!(
            first.contract.input_schema.canonical["$defs"]["message_part"]["properties"]["parts"]
                ["items"]
                ["$ref"]
                .as_str()
                == Some("#/$defs/message_part"),
            "fixture should include recursive nested schema refs"
        );
        assert!(
            first.contract.input_schema.canonical["properties"]["payload"]["oneOf"]
                .as_array()
                .is_some_and(|variants| variants.len() >= 4),
            "fixture should include provider-style payload unions"
        );
        assert!(
            first.contract.input_schema.canonical["properties"]["projection"]["anyOf"]
                .as_array()
                .is_some_and(|variants| variants.len() >= 2),
            "fixture should include output projection unions"
        );
    }

    #[test]
    fn rlm_large_tool_catalog_does_not_resolve_nested_schema_contracts_without_tool_calls() {
        let definitions = BenchmarkLargeToolCatalog::build_tool_definitions();
        let manifests = definitions
            .iter()
            .map(|definition| definition.manifest())
            .collect::<Vec<_>>();
        let contract_resolutions = Arc::new(AtomicUsize::new(0));
        let resolver_count = Arc::clone(&contract_resolutions);

        let surface = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |name| {
                resolver_count.fetch_add(1, Ordering::SeqCst);
                definitions
                    .iter()
                    .find(|definition| definition.name() == name)
                    .map(|definition| Arc::new(definition.contract()))
            })),
            contributions: Vec::new(),
        });

        // Every member is callable under the flat catalog; building the
        // catalog resolves no contracts (rendering is lazy and protocol-owned).
        assert_eq!(surface.callable_tools().len(), GMAIL_LIKE_TOOL_NAMES.len());
        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn oblique_fixture_exposes_retrieval_judge_and_handle_tools_to_lashlang() {
        let definitions = benchmark_oblique_tool_definitions();
        assert_eq!(definitions.len(), 3);
        let bindings = definitions
            .iter()
            .map(|definition| {
                let binding = tool_lashlang_binding(&definition.manifest)
                    .expect("valid lashlang binding")
                    .expect("oblique fixture tool has lashlang binding");
                (
                    definition.name().to_string(),
                    binding.module_path,
                    binding.operation,
                )
            })
            .collect::<Vec<_>>();
        assert!(bindings.contains(&(
            "oblique_search".to_string(),
            vec!["obliq".to_string()],
            Some("search".to_string())
        )));
        assert!(bindings.contains(&(
            "oblique_judge_candidates".to_string(),
            vec!["obliq".to_string()],
            Some("judge_candidates".to_string())
        )));
        assert!(bindings.contains(&(
            "oblique_list_async_handles".to_string(),
            vec!["obliq".to_string()],
            Some("list_async_handles".to_string())
        )));
        let search_contract = benchmark_oblique_search_tool_definition().contract;
        assert!(search_contract.output_contract.is_static());
        assert_eq!(
            search_contract.output_schema.canonical,
            oblique_search_output_schema()
        );
    }

    #[test]
    fn streamed_paired_lashlang_profile_splits_tags_and_trailing_suffix() {
        let profile = benchmark_stream_profile(RuntimePerfScenario::RlmStreamedPairedLashlang);
        assert_eq!(profile.full_text.matches("<lashlang>").count(), 1);
        assert_eq!(profile.full_text.matches("</lashlang>").count(), 1);
        assert!(
            profile
                .full_text
                .ends_with("This suffix must be ignored after the close tag.")
        );
        assert_eq!(profile.deltas.len(), 4);
        assert!(profile.deltas[0].ends_with("<lash"));
        assert!(profile.deltas[1].starts_with("lang>"));
        assert!(profile.deltas[2].ends_with("</lash"));
        assert!(profile.deltas[3].starts_with("lang>"));
        assert!(profile.deltas[3].contains("This suffix must be ignored"));
        assert!(profile.parts.is_empty());
    }
}
