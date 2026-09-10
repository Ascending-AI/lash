use super::*;

impl BenchmarkEchoTool {
    pub(crate) fn new(completion_resolver: Arc<dyn lash_core::EffectHost>) -> Self {
        Self {
            completion_resolver,
            completion_witness: crate::runtime_perf::smoke::completion_witness(),
            settlement_control: None,
        }
    }

    pub(crate) fn with_settlement_control(
        completion_resolver: Arc<dyn lash_core::EffectHost>,
        settlement_control: Arc<BenchmarkSettlementControl>,
    ) -> Self {
        Self {
            completion_resolver,
            completion_witness: crate::runtime_perf::smoke::completion_witness(),
            settlement_control: Some(settlement_control),
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

#[derive(Default)]
pub(crate) struct BenchmarkToolCatalogObserver {
    state: Mutex<Option<ActiveToolCatalogObservation>>,
    suppress_composition_counting: AtomicBool,
}

struct ActiveToolCatalogObservation {
    variant: &'static str,
    session_id: SessionId,
    phase_probe: Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe>,
    observation_stage: Arc<dyn Fn() -> u8 + Send + Sync>,
    setup_recomposition_count: u64,
    recomposition_count: u64,
}

pub(crate) struct BenchmarkToolCatalogObservation {
    pub(crate) cache_state: u64,
    pub(crate) setup_recomposition_count: u64,
    pub(crate) recomposition_count: u64,
}

impl BenchmarkToolCatalogObserver {
    pub(crate) fn suppress_composition_counting(&self) {
        assert!(
            !self
                .suppress_composition_counting
                .swap(true, Ordering::SeqCst),
            "tool-catalog composition counting already suppressed"
        );
    }

    pub(crate) fn resume_composition_counting(&self) {
        assert!(
            self.suppress_composition_counting
                .swap(false, Ordering::SeqCst),
            "tool-catalog composition counting was not suppressed"
        );
    }

    pub(crate) fn arm(
        &self,
        variant: &'static str,
        session_id: SessionId,
        phase_probe: Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe>,
        observation_stage: Arc<dyn Fn() -> u8 + Send + Sync>,
    ) {
        let mut state = self.state.lock_recover();
        assert!(state.is_none(), "tool-catalog observer already armed");
        *state = Some(ActiveToolCatalogObservation {
            variant,
            session_id,
            phase_probe,
            observation_stage,
            setup_recomposition_count: 0,
            recomposition_count: 0,
        });
    }

    pub(crate) fn observe_session_catalog_composition(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::PluginError> {
        if self.suppress_composition_counting.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (probe, phase) = {
            let mut state = self.state.lock_recover();
            let Some(active) = state.as_mut() else {
                return Ok(());
            };
            if active.session_id != session_id {
                return Ok(());
            }
            match (active.observation_stage)() {
                0 => {
                    active.setup_recomposition_count += 1;
                    return Ok(());
                }
                1 => {}
                _ => return Ok(()),
            }
            active.recomposition_count += 1;
            (
                Arc::clone(&active.phase_probe),
                format!(
                    "tool_catalog.{}.session_catalog_composition",
                    active.variant
                ),
            )
        };
        probe.begin_named(&phase);
        probe.end_named(&phase);
        Ok(())
    }

    pub(crate) fn finish(&self) -> BenchmarkToolCatalogObservation {
        let active = self
            .state
            .lock_recover()
            .take()
            .expect("tool-catalog observer was not armed");
        BenchmarkToolCatalogObservation {
            cache_state: u64::from(
                active.recomposition_count == 0
                    && (active.variant != "warm" || active.setup_recomposition_count > 0),
            ),
            setup_recomposition_count: active.setup_recomposition_count,
            recomposition_count: active.recomposition_count,
        }
    }
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

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == benchmark_async_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolOutcome {
        match call.name {
            "benchmark_echo" => execute_benchmark_echo(call).await,
            "benchmark_slow" => execute_benchmark_slow(call).await,
            "benchmark_async" => {
                execute_benchmark_async(
                    Arc::clone(&self.completion_resolver),
                    self.settlement_control.clone(),
                    self.completion_witness.clone(),
                    call,
                )
                .await
            }
            _ => ToolOutcome::err_fmt(format_args!("Unknown benchmark tool: {}", call.name)),
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolOutcome {
        match call.name {
            "oblique_search" => execute_oblique_search(call).await,
            "oblique_judge_candidates" => execute_oblique_judge_candidates(call).await,
            "oblique_list_async_handles" => execute_oblique_list_async_handles(call).await,
            _ => ToolOutcome::err_fmt(format_args!(
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolOutcome {
        let Some((account, operation)) = benchmark_mail_route(call.name) else {
            return ToolOutcome::err_fmt(format_args!(
                "Unknown benchmark workbench mail tool: {}",
                call.name
            ));
        };
        match operation {
            "send" => ToolOutcome::err_fmt(
                "benchmark mail send requires the leaf attempt signature that declares its emission",
            ),
            "list" => ToolOutcome::ok(serde_json::json!({
                "account": account,
                "messages": [],
            })),
            _ => ToolOutcome::err_fmt(format_args!("unsupported mail operation `{operation}`")),
        }
    }

    async fn execute_attempt(&self, call: lash_core::ToolCall<'_>) -> ToolAttemptOutcome {
        let Some((account, operation)) = benchmark_mail_route(call.name) else {
            return done_without_intents(ToolOutcome::err_fmt(format_args!(
                "Unknown benchmark workbench mail tool: {}",
                call.name
            )));
        };
        match operation {
            "send" => execute_benchmark_mail_send(call, account),
            _ => done_without_intents(self.execute(call).await),
        }
    }
}

fn done_without_intents(result: ToolOutcome) -> ToolAttemptOutcome {
    match result {
        ToolOutcome::Done(output) => {
            ToolAttemptOutcome::done_without_intents(ToolOutcomeDone::from_output(*output))
        }
        ToolOutcome::Pending(pending) => ToolAttemptOutcome::pending(pending),
    }
}

/// Commit the send receipt and declare the `mail.received` emission it owes.
///
/// The emission is a journaled trigger occurrence, so it cannot run inside the
/// recorded attempt body: it is declared here and executed by the intent
/// executor once the attempt commits. This site opens the occurrence-to-delivery
/// window; the harness closes it only after observing the delivery process's
/// durable claim and terminal state.
fn execute_benchmark_mail_send(
    call: lash_core::ToolCall<'_>,
    account: &'static str,
) -> ToolAttemptOutcome {
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
        return done_without_intents(ToolOutcome::err_fmt(
            "benchmark mail send requires a replay key",
        ));
    };
    let source_key = match empty_trigger_source_key(BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE) {
        Ok(source_key) => source_key,
        Err(err) => return done_without_intents(ToolOutcome::err_fmt(err.to_string())),
    };
    let message_id = format!("{account}-{replay_key}");
    let payload = serde_json::json!({
        "account": account,
        "title": title,
        "text": text,
    });
    let idempotency_key = format!("{replay_key}:mail.received:{account}");
    let _phase = call.context.named_phase("trigger.occurrence_to_delivery");
    let intent = lash_core::ToolIntent::EmitTrigger(lash_core::EmitTriggerIntent {
        session_id: SessionId::from(call.context.session_id()),
        request: TriggerOccurrenceRequest::new(
            BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE,
            source_key,
            payload,
            idempotency_key,
        )
        .with_source(serde_json::json!({})),
    });
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(serde_json::json!({
            "account": account,
            "id": message_id,
        })),
        lash_core::ToolIntents::v1(vec![intent]),
    )
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
    .with_tool_binding(ToolBinding::new(["inbox", account], operation).with_authority_type("Inbox"))
}

async fn execute_benchmark_echo(call: lash_core::ToolCall<'_>) -> ToolOutcome {
    tokio::task::yield_now().await;
    ToolOutcome::ok(serde_json::json!({
        "value": call.args.get("value").cloned().unwrap_or(serde_json::Value::Null),
        "ordinal": call.args.get("ordinal").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

async fn execute_benchmark_slow(call: lash_core::ToolCall<'_>) -> ToolOutcome {
    let delay_ms = call
        .args
        .get("delay_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(25);
    if let Some(cancel) = call.context.cancellation_token().cloned() {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
            _ = cancel.cancelled() => return ToolOutcome::cancelled("benchmark_slow cancelled"),
        }
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    ToolOutcome::ok(serde_json::json!({
        "value": call.args.get("value").cloned().unwrap_or(serde_json::Value::Null),
        "delay_ms": delay_ms,
    }))
}

async fn execute_benchmark_async(
    completion_resolver: Arc<dyn lash_core::EffectHost>,
    settlement_control: Option<Arc<BenchmarkSettlementControl>>,
    completion_witness: Option<Arc<crate::runtime_perf::smoke::CompletionWitness>>,
    call: lash_core::ToolCall<'_>,
) -> ToolOutcome {
    let key = match call.context.completion_key() {
        Ok(key) => key,
        Err(err) => return ToolOutcome::err_fmt(err),
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
    let pending_phase = settlement_control
        .as_ref()
        .map(|_| call.context.named_phase("async_settlement.child_pending"));
    let completion = async move {
        if let Some(control) = settlement_control {
            let _ = control.hold_completion().await;
        } else if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        drop(pending_phase);
        let outcome = completion_resolver
            .resolve_await_event(
                &key,
                Resolution::Ok(serde_json::json!({
                    "value": value,
                    "mode": "pending_completion",
                    "delay_ms": delay_ms
                })),
            )
            .await?;
        anyhow::ensure!(
            matches!(outcome, lash_core::ResolveOutcome::Accepted),
            "benchmark completion was not accepted: {outcome:?}"
        );
        Ok(())
    };
    if let Some(witness) = completion_witness {
        witness.spawn(completion);
    } else {
        tokio::spawn(completion);
    }
    ToolOutcome::pending(lash_core::PendingCompletion::new())
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
    .with_tool_binding(ToolBinding::new(["tools"], "benchmark_echo").with_authority_type("Tools"))
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
    .with_tool_binding(
        ToolBinding::new(["tools"], "benchmark_slow").with_authority_type("Tools"),
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
    .with_tool_binding(ToolBinding::new(["tools"], "benchmark_async").with_authority_type("Tools"))
}

pub(super) fn benchmark_oblique_tool_definitions() -> Vec<ToolDefinition> {
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

pub(super) fn benchmark_oblique_search_tool_definition() -> ToolDefinition {
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
    .with_tool_binding(
        ToolBinding::new(["obliq"], "search").with_authority_type("Obliq"),
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
    .with_tool_binding(ToolBinding::new(["obliq"], "judge_candidates").with_authority_type("Obliq"))
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
    .with_tool_binding(
        ToolBinding::new(["obliq"], "list_async_handles").with_authority_type("Obliq"),
    )
}

pub(super) fn oblique_search_output_schema() -> serde_json::Value {
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

async fn execute_oblique_search(call: lash_core::ToolCall<'_>) -> ToolOutcome {
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
    ToolOutcome::ok(serde_json::json!({ "matches": matches }))
}

async fn execute_oblique_judge_candidates(call: lash_core::ToolCall<'_>) -> ToolOutcome {
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
        return ToolOutcome::err_fmt("oblique_judge_candidates requires candidate_doc_ids");
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
        Err(err) => return ToolOutcome::err_fmt(err.to_string()),
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
    ToolOutcome::ok(serde_json::json!({
        "ranked_doc_ids": ranked_doc_ids,
        "positive_doc_ids": positive_doc_ids,
        "direct_completion": direct_completion,
    }))
}

async fn execute_oblique_list_async_handles(_call: lash_core::ToolCall<'_>) -> ToolOutcome {
    tokio::task::yield_now().await;
    ToolOutcome::ok(serde_json::json!({
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolOutcome {
        if !GMAIL_LIKE_TOOL_NAMES.contains(&call.name) {
            return ToolOutcome::err_fmt(format_args!("Unknown benchmark tool: {}", call.name));
        }
        tokio::task::yield_now().await;
        ToolOutcome::ok(serde_json::json!({
            "tool": call.name,
            "ok": true,
            "echo": call.args,
        }))
    }
}

impl BenchmarkLargeToolCatalog {
    pub(super) fn build_tool_definitions() -> Vec<ToolDefinition> {
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
    .with_tool_binding(
        ToolBinding::new(
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

pub(super) const GMAIL_LIKE_TOOL_NAMES: &[&str] = &[
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
