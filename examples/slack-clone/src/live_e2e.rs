//! Real-model two-agent nonce-swap acceptance for the Slack-clone example.
//!
//! This is intentionally a manual acceptance harness, not application code.
//! It uses the existing OpenAI-compatible provider against OpenRouter, meters
//! provider-reported usage, and gives the final verdict to exact comparisons
//! plus a browser DOM assertion. No model judges another model.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt as _;
use lash::PromptLayerSink as _;
use lash::direct::GenerationOptions;
use lash::provider::{
    CacheControlDialect, LlmRequest, LlmResponse, ModelCapability, Provider, ProviderFailureKind,
    ProviderHandle, ProviderOptions, ProviderReliability, ReasoningCapability, ReasoningEncoding,
    ReasoningSelection, SamplingCapability,
};
use lash::tools::{
    LashlangToolBinding, StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition,
    ToolDefinitionLashlangExt as _, ToolProvider, ToolResult,
};
use lash::tracing::{JsonlTraceSink, TraceLevel};
use lash::{LashCore, ModelSpec, TurnInput};
use lash_provider_openai::{
    OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider, ProviderRoutingPrefs,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::bot::slack_api::{ChatPostMessageRequest, HistoryQuery, SlackApi};

pub const DEFAULT_RLM_MODEL: &str = "anthropic/claude-sonnet-5";
pub const DEFAULT_STANDARD_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
pub const DEFAULT_MAX_SPEND_USD: f64 = 2.0;
pub const MAX_ATTEMPTS: usize = 2;
pub const MAX_MODEL_TURNS_PER_AGENT: usize = 10;
const MAX_SESSION_TURNS_PER_AGENT: usize = 2;
const MAX_MODEL_TURNS_PER_SESSION_TURN: usize =
    MAX_MODEL_TURNS_PER_AGENT / MAX_SESSION_TURNS_PER_AGENT;
const SMOKE_RLM_CALL_BUDGET: usize = 1;
const SMOKE_STANDARD_CALL_BUDGET: usize = 4;
const MAX_INPUT_TOKENS_PER_CALL: usize = 32_768;
const MIN_OUTPUT_TOKENS: usize = 128;
const MAX_OUTPUT_TOKENS: usize = 512;
const TURN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Serialize)]
struct ModelPrice {
    input_usd_per_token: f64,
    output_usd_per_token: f64,
    cache_read_usd_per_token: f64,
    cache_write_usd_per_token: f64,
}

fn price_for(model: &str) -> Option<ModelPrice> {
    match model {
        // OpenRouter /api/v1/models, verified 2026-08-14. The harness disables
        // caching, but the reservation still uses the published write price.
        DEFAULT_RLM_MODEL => Some(ModelPrice {
            input_usd_per_token: 0.000_002,
            output_usd_per_token: 0.000_010,
            cache_read_usd_per_token: 0.000_000_2,
            cache_write_usd_per_token: 0.000_002_5,
        }),
        // The dated slug avoids a moving alias. Unknown overrides are rejected
        // rather than assigned a guessed price.
        DEFAULT_STANDARD_MODEL => Some(ModelPrice {
            input_usd_per_token: 0.000_000_14,
            output_usd_per_token: 0.000_000_28,
            cache_read_usd_per_token: 0.000_000_028,
            cache_write_usd_per_token: 0.000_000_14,
        }),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize)]
struct SpendRecord {
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_write_input_tokens: i64,
    reasoning_output_tokens: i64,
    call_cost_usd: f64,
    cumulative_cost_usd: f64,
    provider_usage: Option<Value>,
    outcome: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
struct SpendSnapshot {
    cap_usd: f64,
    total_usd: f64,
    exceeded: bool,
    records: Vec<SpendRecord>,
}

#[derive(Clone, Copy, Debug)]
struct Reservation {
    usd: f64,
    input_tokens: usize,
    output_tokens: usize,
}

#[derive(Clone, Debug)]
struct SpendLedger {
    state: Arc<Mutex<SpendSnapshot>>,
    artifact_path: Arc<PathBuf>,
}

impl SpendLedger {
    fn new(cap_usd: f64, artifact_path: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(SpendSnapshot {
                cap_usd,
                ..SpendSnapshot::default()
            })),
            artifact_path: Arc::new(artifact_path),
        }
    }

    fn snapshot(&self) -> SpendSnapshot {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn is_exceeded(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .exceeded
    }

    fn reserve(
        &self,
        request: &LlmRequest,
    ) -> std::result::Result<Reservation, lash::provider::LlmTransportError> {
        if !request.attachments.is_empty() {
            return Err(typed_provider_error(
                "InputBudgetExceeded",
                ProviderFailureKind::Validation,
                "live E2E does not permit model attachments",
            ));
        }
        // One token per serialized byte is deliberately conservative for the
        // text/tool JSON accepted here, and bounds both prompt and tool schema.
        let input_tokens = serde_json::to_vec(request)
            .map_err(|error| {
                typed_provider_error(
                    "InputBudgetExceeded",
                    ProviderFailureKind::Validation,
                    format!("measure live-E2E request: {error}"),
                )
            })?
            .len();
        if input_tokens > MAX_INPUT_TOKENS_PER_CALL {
            return Err(typed_provider_error(
                "InputBudgetExceeded",
                ProviderFailureKind::Validation,
                format!(
                    "serialized request bound {input_tokens} exceeds {MAX_INPUT_TOKENS_PER_CALL} tokens"
                ),
            ));
        }
        let Some(price) = price_for(&request.model) else {
            return Err(typed_provider_error(
                "ModelNotPriced",
                ProviderFailureKind::Unsupported,
                format!(
                    "model {} has no conservative live-E2E price entry",
                    request.model
                ),
            ));
        };
        let output_tokens = request
            .generation
            .output_token_cap_u64()
            .unwrap_or(MAX_OUTPUT_TOKENS as u64) as usize;
        let conservative_input_price = price
            .input_usd_per_token
            .max(price.cache_read_usd_per_token)
            .max(price.cache_write_usd_per_token);
        let usd = input_tokens as f64 * conservative_input_price
            + output_tokens as f64 * price.output_usd_per_token;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.total_usd + usd > state.cap_usd {
            state.exceeded = true;
            write_json(&self.artifact_path, &*state).ok();
            return Err(typed_provider_error(
                "SpendCapExceeded",
                ProviderFailureKind::Quota,
                format!(
                    "next live-E2E request would raise estimated spend from ${:.6} to ${:.6}, above cap ${:.2}",
                    state.total_usd,
                    state.total_usd + usd,
                    state.cap_usd
                ),
            ));
        }
        state.total_usd += usd;
        write_json(&self.artifact_path, &*state).ok();
        Ok(Reservation {
            usd,
            input_tokens,
            output_tokens,
        })
    }

    fn settle(
        &self,
        model: &str,
        response: &LlmResponse,
        outcome: &'static str,
        reservation: Reservation,
    ) -> std::result::Result<(), lash::provider::LlmTransportError> {
        let Some(provider_usage) = response
            .provider_usage
            .as_ref()
            .filter(|usage| usage_has_number(usage))
        else {
            return Err(typed_provider_error(
                "UsageMetadataMissing",
                ProviderFailureKind::Validation,
                "OpenRouter response did not contain numeric provider usage metadata",
            ));
        };
        let Some(price) = price_for(model) else {
            return Err(typed_provider_error(
                "ModelNotPriced",
                ProviderFailureKind::Unsupported,
                format!("model {model} has no conservative live-E2E price entry"),
            ));
        };
        let usage = &response.usage;
        let call_cost_usd = nonnegative(usage.input_tokens) * price.input_usd_per_token
            + nonnegative(usage.output_tokens) * price.output_usd_per_token
            + nonnegative(usage.cache_read_input_tokens) * price.cache_read_usd_per_token
            + nonnegative(usage.cache_write_input_tokens) * price.cache_write_usd_per_token;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.total_usd = (state.total_usd - reservation.usd).max(0.0) + call_cost_usd;
        let cumulative_cost_usd = state.total_usd;
        state.exceeded = state.total_usd > state.cap_usd;
        state.records.push(SpendRecord {
            model: model.to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            call_cost_usd,
            cumulative_cost_usd,
            provider_usage: Some(provider_usage.clone()),
            outcome,
        });
        write_json(&self.artifact_path, &*state).ok();
        if state.exceeded {
            return Err(typed_provider_error(
                "SpendCapExceeded",
                ProviderFailureKind::Quota,
                format!(
                    "live E2E estimated spend ${:.6} exceeded cap ${:.2}",
                    state.total_usd, state.cap_usd
                ),
            ));
        }
        Ok(())
    }

    fn retain_failed_reservation(&self, model: &str, reservation: Reservation) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let cumulative_cost_usd = state.total_usd;
        state.records.push(SpendRecord {
            model: model.to_string(),
            input_tokens: reservation.input_tokens as i64,
            output_tokens: reservation.output_tokens as i64,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
            call_cost_usd: reservation.usd,
            cumulative_cost_usd,
            provider_usage: None,
            outcome: "transport_failure_reserved",
        });
        write_json(&self.artifact_path, &*state).ok();
    }
}

fn nonnegative(tokens: i64) -> f64 {
    tokens.max(0) as f64
}

fn usage_has_number(value: &Value) -> bool {
    match value {
        Value::Number(_) => true,
        Value::Array(values) => values.iter().any(usage_has_number),
        Value::Object(fields) => fields.values().any(usage_has_number),
        Value::Null | Value::Bool(_) | Value::String(_) => false,
    }
}

fn typed_provider_error(
    code: &str,
    kind: ProviderFailureKind,
    message: impl Into<String>,
) -> lash::provider::LlmTransportError {
    lash::provider::LlmTransportError::new(message)
        .with_code(code)
        .with_kind(kind)
        .retryable(false)
}

struct MeteredProvider {
    inner: Box<dyn Provider>,
    ledger: SpendLedger,
}

impl std::fmt::Debug for MeteredProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MeteredProvider")
            .field("kind", &self.inner.kind())
            .field("spend", &self.ledger.snapshot().total_usd)
            .finish()
    }
}

#[async_trait]
impl Provider for MeteredProvider {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        self.inner.route_identity(model)
    }

    fn options(&self) -> ProviderOptions {
        self.inner.options()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.set_options(options);
    }

    fn serialize_config(&self) -> Value {
        let mut config = self.inner.serialize_config();
        if let Some(object) = config.as_object_mut() {
            object.insert("api_key".to_string(), Value::String("REDACTED".to_string()));
        }
        config
    }

    async fn complete(
        &mut self,
        request: LlmRequest,
    ) -> std::result::Result<LlmResponse, lash::provider::LlmTransportError> {
        if self.ledger.is_exceeded() {
            return Err(typed_provider_error(
                "SpendCapExceeded",
                ProviderFailureKind::Quota,
                "live E2E spend cap was already exceeded",
            ));
        }
        if price_for(&request.model).is_none() {
            return Err(typed_provider_error(
                "ModelNotPriced",
                ProviderFailureKind::Unsupported,
                format!(
                    "model {} has no conservative live-E2E price entry",
                    request.model
                ),
            ));
        }
        let model = request.model.clone();
        let reservation = self.ledger.reserve(&request)?;
        match self.inner.complete(request).await {
            Ok(response) => {
                self.ledger
                    .settle(&model, &response, "success", reservation)?;
                Ok(response)
            }
            Err(error) => {
                if let Some(partial) = error.partial_response.as_deref() {
                    self.ledger
                        .settle(&model, partial, "partial_failure", reservation)?;
                } else {
                    self.ledger.retain_failed_reservation(&model, reservation);
                }
                Err(error)
            }
        }
    }

    fn generation_retry_guarantee(
        &self,
        request: &LlmRequest,
    ) -> lash::provider::GenerationRetryGuarantee {
        self.inner.generation_retry_guarantee(request)
    }

    fn requires_streaming(&self) -> bool {
        self.inner.requires_streaming()
    }

    async fn close(&self) -> std::result::Result<(), lash::provider::LlmTransportError> {
        self.inner.close().await
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(Self {
            inner: self.inner.clone_boxed(),
            ledger: self.ledger.clone(),
        })
    }
}

#[derive(Clone, Debug)]
struct Config {
    api_key: String,
    base_url: String,
    artifact_dir: PathBuf,
    rlm_model: String,
    standard_model: String,
    max_spend_usd: f64,
    output_token_cap: usize,
    smoke_only: bool,
}

impl Config {
    fn from_env_and_args() -> Result<Option<Self>> {
        let Some(api_key) = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            println!("SKIP: OPENROUTER_API_KEY is unset; live Slack-clone E2E was not run");
            return Ok(None);
        };
        let mut base_url = None;
        let mut artifact_dir = None;
        let mut smoke_only = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--base-url" => base_url = args.next(),
                "--artifact-dir" => artifact_dir = args.next().map(PathBuf::from),
                "--smoke-only" => smoke_only = true,
                other => bail!("unknown argument: {other}"),
            }
        }
        let base_url = base_url.context("--base-url is required")?;
        let artifact_dir = artifact_dir.context("--artifact-dir is required")?;
        let rlm_model = env_or("LASH_LIVE_E2E_RLM_MODEL", DEFAULT_RLM_MODEL);
        let standard_model = env_or("LASH_LIVE_E2E_STANDARD_MODEL", DEFAULT_STANDARD_MODEL);
        for model in [&rlm_model, &standard_model] {
            if price_for(model).is_none() {
                bail!("ModelNotPriced: refusing unknown model override {model}");
            }
        }
        let max_spend_usd = std::env::var("LASH_LIVE_E2E_MAX_SPEND_USD")
            .ok()
            .map(|value| value.parse::<f64>())
            .transpose()
            .context("parse LASH_LIVE_E2E_MAX_SPEND_USD")?
            .unwrap_or(DEFAULT_MAX_SPEND_USD);
        if !max_spend_usd.is_finite() || max_spend_usd <= 0.0 {
            bail!("LASH_LIVE_E2E_MAX_SPEND_USD must be a positive finite number");
        }
        let output_token_cap = derived_output_token_cap(max_spend_usd)?;
        Ok(Some(Self {
            api_key,
            base_url,
            artifact_dir,
            rlm_model,
            standard_model,
            max_spend_usd,
            output_token_cap,
            smoke_only,
        }))
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn maximum_rlm_calls() -> usize {
    SMOKE_RLM_CALL_BUDGET + MAX_ATTEMPTS * MAX_MODEL_TURNS_PER_AGENT
}

fn maximum_standard_calls() -> usize {
    SMOKE_STANDARD_CALL_BUDGET + MAX_ATTEMPTS * MAX_MODEL_TURNS_PER_AGENT
}

fn maximum_provider_calls() -> usize {
    maximum_rlm_calls() + maximum_standard_calls()
}

fn worst_case_spend_usd(output_tokens: usize) -> f64 {
    let rlm = price_for(DEFAULT_RLM_MODEL).expect("default RLM model is priced");
    let standard = price_for(DEFAULT_STANDARD_MODEL).expect("default standard model is priced");
    maximum_rlm_calls() as f64
        * (MAX_INPUT_TOKENS_PER_CALL as f64
            * rlm.input_usd_per_token.max(rlm.cache_write_usd_per_token)
            + output_tokens as f64 * rlm.output_usd_per_token)
        + maximum_standard_calls() as f64
            * (MAX_INPUT_TOKENS_PER_CALL as f64
                * standard
                    .input_usd_per_token
                    .max(standard.cache_write_usd_per_token)
                + output_tokens as f64 * standard.output_usd_per_token)
}

fn derived_output_token_cap(max_spend_usd: f64) -> Result<usize> {
    let rlm = price_for(DEFAULT_RLM_MODEL).expect("default RLM model is priced");
    let standard = price_for(DEFAULT_STANDARD_MODEL).expect("default standard model is priced");
    let input_reserve = maximum_rlm_calls() as f64
        * MAX_INPUT_TOKENS_PER_CALL as f64
        * rlm.input_usd_per_token.max(rlm.cache_write_usd_per_token)
        + maximum_standard_calls() as f64
            * MAX_INPUT_TOKENS_PER_CALL as f64
            * standard
                .input_usd_per_token
                .max(standard.cache_write_usd_per_token);
    let output_price = maximum_rlm_calls() as f64 * rlm.output_usd_per_token
        + maximum_standard_calls() as f64 * standard.output_usd_per_token;
    let derived = ((max_spend_usd - input_reserve) / output_price).floor() as usize;
    if derived < MIN_OUTPUT_TOKENS {
        bail!("spend cap ${max_spend_usd:.2} is too low for the {MIN_OUTPUT_TOKENS}-token minimum");
    }
    Ok(derived.min(MAX_OUTPUT_TOKENS))
}

fn provider(config: &Config, ledger: &SpendLedger) -> ProviderHandle {
    let options = ProviderOptions {
        reliability: ProviderReliability::disabled(),
        max_output_tokens: Some(config.output_token_cap as u64),
        ..ProviderOptions::default()
    };
    let mut compat = OpenAiCompat::openrouter();
    // Sonnet 5 advertises tools but not parallel_tool_calls. Lash's OpenAI
    // adapter gates only that optional field behind request_fields.
    compat.request_fields = Some(false);
    compat.provider_routing = Some(ProviderRoutingPrefs {
        require_parameters: true,
    });
    let components = OpenAiCompatibleProvider::new(config.api_key.clone(), OPENROUTER_BASE_URL)
        .with_compat(compat)
        .with_options(options)
        .into_components()
        .map_provider(|inner| {
            Box::new(MeteredProvider {
                inner,
                ledger: ledger.clone(),
            })
        });
    ProviderHandle::new(components)
}

fn model_spec(model: &str, output_cap: usize) -> Result<ModelSpec> {
    let (context, output_capacity, cache_control, sampling, efforts) = match model {
        DEFAULT_RLM_MODEL => (
            1_000_000,
            128_000,
            Some(CacheControlDialect::Anthropic),
            SamplingCapability::Pinned,
            vec!["low", "medium", "high", "max", "xhigh"],
        ),
        DEFAULT_STANDARD_MODEL => (
            1_048_576,
            393_216,
            None,
            SamplingCapability::Configurable,
            vec!["low", "high", "max"],
        ),
        _ => bail!("ModelNotPriced: {model}"),
    };
    ModelSpec::builder(model)
        .variant(ReasoningSelection::Effort("low".to_string()))
        .context_window_tokens(context)
        .output_token_capacity(output_capacity)
        .capability(ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: efforts.into_iter().map(String::from).collect(),
                default_effort: Some("low".to_string()),
                encoding: ReasoningEncoding::Effort,
                ..ReasoningCapability::default()
            }),
            cache_control,
            sampling,
            ..ModelCapability::default()
        })
        .build()
        .with_context(|| format!("build model metadata for {model} with cap {output_cap}"))
}

fn generation(output_cap: usize) -> GenerationOptions {
    GenerationOptions {
        output_token_cap: NonZeroUsize::new(output_cap),
        ..GenerationOptions::default()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EchoArgs {
    value: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct EchoOutput {
    value: String,
}

struct EchoTool;

#[async_trait]
impl StaticToolExecute for EchoTool {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        match serde_json::from_value::<EchoArgs>(call.args.clone()) {
            Ok(args) if call.name == "structural_echo" => {
                ToolResult::ok(json!(EchoOutput { value: args.value }))
            }
            Ok(_) => ToolResult::err(json!("unknown tool")),
            Err(error) => ToolResult::err_fmt(format_args!("invalid arguments: {error}")),
        }
    }
}

fn echo_tools() -> Arc<dyn ToolProvider> {
    Arc::new(StaticToolProvider::new(
        vec![ToolDefinition::typed::<EchoArgs, EchoOutput>(
            "tool:slack_clone.structural_echo",
            "structural_echo",
            "Return the supplied value unchanged. You must call this when requested.",
        )],
        EchoTool,
    ))
}

#[derive(Clone, Debug, Serialize)]
struct SmokeProbe {
    name: &'static str,
    passed: bool,
    evidence: String,
}

async fn run_smoke_probes(
    config: &Config,
    ledger: &SpendLedger,
) -> std::result::Result<Vec<SmokeProbe>, FailureReason> {
    let smoke_dir = config.artifact_dir.join("smoke");
    std::fs::create_dir_all(&smoke_dir).map_err(FailureReason::harness)?;
    let stream_core = standard_core(
        provider(config, ledger),
        model_spec(&config.rlm_model, config.output_token_cap).map_err(FailureReason::harness)?,
        config.output_token_cap,
        SMOKE_RLM_CALL_BUDGET,
        "Answer with exactly STREAM_OK.",
        None,
        smoke_dir.join("stream.trace.jsonl"),
    )
    .map_err(FailureReason::harness)?;
    let stream_session = stream_core
        .session("live-smoke-stream")
        .open()
        .await
        .map_err(FailureReason::harness)?;
    let mut live_stream = stream_session
        .turn(TurnInput::text("Reply now."))
        .stream()
        .map_err(FailureReason::harness)?;
    let (stream, stream_activity_count) = tokio::time::timeout(TURN_TIMEOUT, async move {
        let mut activity_count = 0;
        while let Some(activity) = live_stream.next().await {
            activity.map_err(FailureReason::harness)?;
            activity_count += 1;
        }
        let result = live_stream.finish().await.map_err(FailureReason::harness)?;
        Ok::<_, FailureReason>((result, activity_count))
    })
    .await
    .map_err(|_| FailureReason::TurnTimedOut {
        agent: "smoke-stream".to_string(),
    })??;
    let stream_passed =
        stream.is_success() && !stream.llm_calls.is_empty() && stream_activity_count > 0;

    let tool_core = standard_core(
        provider(config, ledger),
        model_spec(&config.standard_model, config.output_token_cap)
            .map_err(FailureReason::harness)?,
        config.output_token_cap,
        SMOKE_STANDARD_CALL_BUDGET,
        "Call structural_echo exactly once with value TOOL_OK, then report completion.",
        Some(echo_tools()),
        smoke_dir.join("tool.trace.jsonl"),
    )
    .map_err(FailureReason::harness)?;
    let tool_session = tool_core
        .session("live-smoke-tool")
        .open()
        .await
        .map_err(FailureReason::harness)?;
    let tool = tokio::time::timeout(
        TURN_TIMEOUT,
        tool_session
            .turn(TurnInput::text("Perform the required probe."))
            .run(),
    )
    .await
    .map_err(|_| FailureReason::TurnTimedOut {
        agent: "smoke-tool".to_string(),
    })?
    .map_err(FailureReason::harness)?;
    let tool_passed = tool.is_success()
        && tool
            .result
            .tool_calls
            .iter()
            .any(|call| call.tool == "structural_echo");
    let usage = ledger.snapshot();
    let usage_passed = !usage.records.is_empty()
        && usage
            .records
            .iter()
            .all(|record| record.provider_usage.as_ref().is_some_and(usage_has_number));
    let probes = vec![
        SmokeProbe {
            name: "stream_opens_and_terminates",
            passed: stream_passed,
            evidence: format!(
                "llm_calls={} activities={stream_activity_count}",
                stream.llm_calls.len()
            ),
        },
        SmokeProbe {
            name: "tool_call_round_trips",
            passed: tool_passed,
            evidence: format!("tool_calls={}", tool.result.tool_calls.len()),
        },
        SmokeProbe {
            name: "usage_metadata_populated",
            passed: usage_passed,
            evidence: format!(
                "metered_calls={} cost_usd={:.6}",
                usage.records.len(),
                usage.total_usd
            ),
        },
    ];
    write_json(&smoke_dir.join("probes.json"), &probes).map_err(FailureReason::harness)?;
    if let Some(failed) = probes.iter().find(|probe| !probe.passed) {
        return Err(FailureReason::SmokeProbeFailed {
            probe: failed.name.to_string(),
        });
    }
    Ok(probes)
}

fn standard_core(
    provider: ProviderHandle,
    model: ModelSpec,
    output_cap: usize,
    turn_budget: usize,
    instructions: &str,
    tools: Option<Arc<dyn ToolProvider>>,
    trace_path: PathBuf,
) -> Result<LashCore> {
    let mut builder = LashCore::standard_builder(lash::TurnBudget::bounded(turn_budget))
        .provider(provider)
        .model(model)
        .generation(generation(output_cap))
        .instructions(instructions)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .trace_sink(Arc::new(JsonlTraceSink::new(trace_path)))
        .trace_level(TraceLevel::Extended);
    if let Some(tools) = tools {
        builder = builder.tools(tools);
    }
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "slack-clone-live-standard",
            Uuid::new_v4().to_string(),
        ))
        .context("build standard live-E2E core")
}

fn rlm_core(
    provider: ProviderHandle,
    model: ModelSpec,
    output_cap: usize,
    instructions: &str,
    tools: Arc<dyn ToolProvider>,
    trace_path: PathBuf,
) -> Result<LashCore> {
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    LashCore::rlm_builder(
        lash::TurnBudget::bounded(MAX_MODEL_TURNS_PER_SESSION_TURN),
        factory,
    )
    .provider(provider)
    .model(model)
    .generation(generation(output_cap))
    .instructions(instructions)
    .tools(tools)
    .effect_host(Arc::new(
        lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
    ))
    .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
    .process_env_store(Arc::new(
        lash::persistence::InMemoryProcessExecutionEnvStore::new(),
    ))
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .trace_sink(Arc::new(JsonlTraceSink::new(trace_path)))
    .trace_level(TraceLevel::Extended)
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        "slack-clone-live-rlm",
        Uuid::new_v4().to_string(),
    ))
    .context("build RLM live-E2E core")
}

const SWAP_INSTRUCTIONS: &str = r#"
You are one participant in a deterministic two-agent nonce exchange in one shared channel.
The first user turn gives you your private nonce and your agent label. Never guess
or alter a nonce. Use post_channel_message to publish your own nonce verbatim,
clearly labeled with your agent label. Use read_channel to obtain the peer's
published nonce. Only after reading it from the channel, call submit_peer_nonce
with the peer nonce verbatim. Never submit your own nonce. If the peer has not
posted yet, finish briefly; a later turn will ask you to continue. Tool use is
required; prose is not a submission.
"#;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadChannelArgs {}

#[derive(Debug, Serialize, JsonSchema)]
struct ReadChannelOutput {
    messages: Vec<TranscriptMessage>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PostMessageArgs {
    message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PostMessageOutput {
    posted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SubmitNonceArgs {
    nonce: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SubmitNonceOutput {
    accepted: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
struct TranscriptMessage {
    ts: String,
    author: String,
    text: String,
}

#[derive(Clone, Debug, Serialize)]
struct ToolEvent {
    agent: String,
    tool: String,
    value: String,
}

#[derive(Clone, Debug, Default, Serialize)]
struct SwapState {
    submissions: BTreeMap<String, String>,
    tool_events: Vec<ToolEvent>,
}

struct SwapTools {
    agent: String,
    channel: String,
    api: Arc<SlackApi>,
    state: Arc<Mutex<SwapState>>,
}

#[async_trait]
impl StaticToolExecute for SwapTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        match call.name {
            "read_channel" => self.read_channel().await,
            "post_channel_message" => match serde_json::from_value(call.args.clone()) {
                Ok(args) => self.post_message(args).await,
                Err(error) => ToolResult::err_fmt(format_args!("invalid arguments: {error}")),
            },
            "submit_peer_nonce" => match serde_json::from_value(call.args.clone()) {
                Ok(args) => self.submit(args),
                Err(error) => ToolResult::err_fmt(format_args!("invalid arguments: {error}")),
            },
            other => ToolResult::err_fmt(format_args!("unknown tool: {other}")),
        }
    }
}

impl SwapTools {
    async fn read_channel(&self) -> ToolResult {
        let output = match read_transcript(&self.api, &self.channel).await {
            Ok(messages) => ReadChannelOutput { messages },
            Err(error) => return ToolResult::err_fmt(format_args!("{error:#}")),
        };
        self.record(
            "read_channel",
            format!("messages={}", output.messages.len()),
        );
        into_tool_result(&output)
    }

    async fn post_message(&self, args: PostMessageArgs) -> ToolResult {
        let text = format!("{}: {}", self.agent, args.message.trim());
        let request = ChatPostMessageRequest {
            channel: self.channel.clone(),
            text: text.clone(),
            username: Some(self.agent.clone()),
            thread_ts: None,
            reply_broadcast: None,
            metadata: None,
        };
        if let Err(error) = self.api.chat_post_message(&request).await {
            return ToolResult::err_fmt(format_args!("{error:#}"));
        }
        self.record("post_channel_message", text);
        into_tool_result(&PostMessageOutput { posted: true })
    }

    fn submit(&self, args: SubmitNonceArgs) -> ToolResult {
        let nonce = args.nonce.trim().to_string();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state
            .submissions
            .entry(self.agent.clone())
            .or_insert_with(|| nonce.clone());
        state.tool_events.push(ToolEvent {
            agent: self.agent.clone(),
            tool: "submit_peer_nonce".to_string(),
            value: nonce,
        });
        into_tool_result(&SubmitNonceOutput { accepted: true })
    }

    fn record(&self, tool: &str, value: String) {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .tool_events
            .push(ToolEvent {
                agent: self.agent.clone(),
                tool: tool.to_string(),
                value,
            });
    }
}

fn swap_tools(
    agent: &str,
    channel: &str,
    api: Arc<SlackApi>,
    state: Arc<Mutex<SwapState>>,
) -> Arc<dyn ToolProvider> {
    let definitions = vec![
        ToolDefinition::typed::<ReadChannelArgs, ReadChannelOutput>(
            "tool:slack_clone.live.read_channel",
            "read_channel",
            "Read the shared channel oldest-first. Use this to obtain the peer's published nonce.",
        )
        .with_lashlang_binding(LashlangToolBinding::new(["channel"], "history")),
        ToolDefinition::typed::<PostMessageArgs, PostMessageOutput>(
            "tool:slack_clone.live.post_channel_message",
            "post_channel_message",
            "Post to the shared channel. Publish your own nonce verbatim here.",
        )
        .with_lashlang_binding(LashlangToolBinding::new(["channel"], "post")),
        ToolDefinition::typed::<SubmitNonceArgs, SubmitNonceOutput>(
            "tool:slack_clone.live.submit_peer_nonce",
            "submit_peer_nonce",
            "Submit the peer nonce after reading it from the shared channel. Exact value only.",
        )
        .with_lashlang_binding(LashlangToolBinding::new(["exchange"], "submit")),
    ];
    Arc::new(StaticToolProvider::new(
        definitions,
        SwapTools {
            agent: agent.to_string(),
            channel: channel.to_string(),
            api,
            state,
        },
    ))
}

fn into_tool_result(value: &impl Serialize) -> ToolResult {
    match serde_json::to_value(value) {
        Ok(value) => ToolResult::ok(value),
        Err(error) => ToolResult::err_fmt(format_args!("serialize tool output: {error}")),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
enum FailureReason {
    SpendCapExceeded {
        total_usd: f64,
        cap_usd: f64,
    },
    SmokeProbeFailed {
        probe: String,
    },
    TurnTimedOut {
        agent: String,
    },
    TurnBudgetExhausted {
        agent: String,
    },
    OutputTokenCeilingExhausted {
        agent: String,
    },
    OracleMismatch {
        submitted_by_a: Option<String>,
        expected_from_b: String,
        submitted_by_b: Option<String>,
        expected_from_a: String,
    },
    UiAssertionFailed {
        detail: String,
    },
    Harness {
        detail: String,
    },
}

impl FailureReason {
    fn harness(error: impl std::fmt::Display) -> Self {
        Self::Harness {
            detail: error.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct AttemptReport {
    attempt: usize,
    channel: String,
    nonce_a: String,
    nonce_b: String,
    turns_a: usize,
    turns_b: usize,
    submissions: BTreeMap<String, String>,
    transcript: Vec<TranscriptMessage>,
    passed: bool,
    failure: Option<FailureReason>,
    spend_after_usd: f64,
}

async fn run_attempt(
    config: &Config,
    ledger: &SpendLedger,
    api: Arc<SlackApi>,
    attempt: usize,
) -> AttemptReport {
    let attempt_dir = config.artifact_dir.join(format!("attempt-{attempt}"));
    let _ = std::fs::create_dir_all(&attempt_dir);
    let nonce_a = fresh_nonce();
    let nonce_b = fresh_nonce();
    let (channel, channel_name) = match create_attempt_channel(&config.base_url, attempt).await {
        Ok(channel) => channel,
        Err(error) => return failed_attempt(attempt, nonce_a, nonce_b, ledger, error),
    };
    let state = Arc::new(Mutex::new(SwapState::default()));
    let rlm = rlm_core(
        provider(config, ledger),
        match model_spec(&config.rlm_model, config.output_token_cap) {
            Ok(model) => model,
            Err(error) => return failed_attempt(attempt, nonce_a, nonce_b, ledger, error),
        },
        config.output_token_cap,
        SWAP_INSTRUCTIONS,
        swap_tools("Agent A", &channel, Arc::clone(&api), Arc::clone(&state)),
        attempt_dir.join("rlm.trace.jsonl"),
    );
    let standard = standard_core(
        provider(config, ledger),
        match model_spec(&config.standard_model, config.output_token_cap) {
            Ok(model) => model,
            Err(error) => return failed_attempt(attempt, nonce_a, nonce_b, ledger, error),
        },
        config.output_token_cap,
        MAX_MODEL_TURNS_PER_SESSION_TURN,
        SWAP_INSTRUCTIONS,
        Some(swap_tools(
            "Agent B",
            &channel,
            Arc::clone(&api),
            Arc::clone(&state),
        )),
        attempt_dir.join("standard.trace.jsonl"),
    );
    let (rlm, standard) = match (rlm, standard) {
        (Ok(rlm), Ok(standard)) => (rlm, standard),
        (Err(error), _) | (_, Err(error)) => {
            return failed_attempt(attempt, nonce_a, nonce_b, ledger, error);
        }
    };
    let session_a = match rlm.session(format!("swap-{attempt}-a")).open().await {
        Ok(session) => session,
        Err(error) => return failed_attempt(attempt, nonce_a, nonce_b, ledger, error),
    };
    let session_b = match standard.session(format!("swap-{attempt}-b")).open().await {
        Ok(session) => session,
        Err(error) => return failed_attempt(attempt, nonce_a, nonce_b, ledger, error),
    };
    let prompt_a = format!(
        "You are Agent A. Your private nonce is {nonce_a}. Begin the exchange now and keep using the channel tools until you have submitted Agent B's nonce."
    );
    let prompt_b = format!(
        "You are Agent B. Your private nonce is {nonce_b}. Begin the exchange now and keep using the channel tools until you have submitted Agent A's nonce."
    );
    let (result_a, result_b) = tokio::join!(
        run_agent_turn(&session_a, "Agent A", prompt_a),
        run_agent_turn(&session_b, "Agent B", prompt_b),
    );
    let mut turns_a = 1;
    let mut turns_b = 1;
    let mut failure = result_a.err().or_else(|| result_b.err());
    if failure.is_none() {
        let first_pass = state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let continue_a = !first_pass.submissions.contains_key("Agent A");
        let continue_b = !first_pass.submissions.contains_key("Agent B");
        let (result_a, result_b) = tokio::join!(
            async {
                if continue_a {
                    run_agent_turn(
                        &session_a,
                        "Agent A",
                        "Continuation: the peer has now had a full turn to publish. Read the channel and submit the exact Agent B nonce. If your own nonce is absent, publish it again first."
                            .to_string(),
                    )
                    .await
                } else {
                    Ok(())
                }
            },
            async {
                if continue_b {
                    run_agent_turn(
                        &session_b,
                        "Agent B",
                        "Continuation: the peer has now had a full turn to publish. Read the channel and submit the exact Agent A nonce. If your own nonce is absent, publish it again first."
                            .to_string(),
                    )
                    .await
                } else {
                    Ok(())
                }
            },
        );
        turns_a += usize::from(continue_a);
        turns_b += usize::from(continue_b);
        failure = result_a.err().or_else(|| result_b.err());
    }
    if ledger.is_exceeded() {
        failure = Some(spend_failure(ledger));
    }
    let session_a_view = session_a.read_view();
    let session_b_view = session_b.read_view();
    let _ = write_json(
        &attempt_dir.join("session-agent-a.json"),
        &session_a_view.messages(),
    );
    let _ = write_json(
        &attempt_dir.join("session-agent-b.json"),
        &session_b_view.messages(),
    );
    let snapshot = state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let transcript = read_transcript(&api, &channel).await.unwrap_or_default();
    let _ = write_json(&attempt_dir.join("tool-state.json"), &snapshot);
    let _ = write_json(&attempt_dir.join("transcript.json"), &transcript);
    let expected_b = nonce_b.clone();
    let oracle_passed = snapshot.submissions.get("Agent A") == Some(&expected_b)
        && snapshot.submissions.get("Agent B") == Some(&nonce_a);
    if failure.is_none() && !oracle_passed {
        failure = Some(FailureReason::OracleMismatch {
            submitted_by_a: snapshot.submissions.get("Agent A").cloned(),
            expected_from_b: expected_b,
            submitted_by_b: snapshot.submissions.get("Agent B").cloned(),
            expected_from_a: nonce_a.clone(),
        });
    }
    let ui_result = run_browser_assertion(config, &attempt_dir, &channel_name, &nonce_a, &nonce_b);
    if failure.is_none()
        && let Err(detail) = ui_result
    {
        failure = Some(FailureReason::UiAssertionFailed { detail });
    }
    let report = AttemptReport {
        attempt,
        channel: channel_name,
        nonce_a,
        nonce_b,
        turns_a,
        turns_b,
        submissions: snapshot.submissions,
        transcript,
        passed: failure.is_none(),
        failure,
        spend_after_usd: ledger.snapshot().total_usd,
    };
    let _ = write_json(&attempt_dir.join("attempt.json"), &report);
    report
}

fn failed_attempt(
    attempt: usize,
    nonce_a: String,
    nonce_b: String,
    ledger: &SpendLedger,
    error: impl std::fmt::Display,
) -> AttemptReport {
    AttemptReport {
        attempt,
        channel: String::new(),
        nonce_a,
        nonce_b,
        turns_a: 0,
        turns_b: 0,
        submissions: BTreeMap::new(),
        transcript: Vec::new(),
        passed: false,
        failure: Some(FailureReason::harness(error)),
        spend_after_usd: ledger.snapshot().total_usd,
    }
}

async fn run_agent_turn(
    session: &lash::LashSession,
    agent: &str,
    prompt: String,
) -> std::result::Result<(), FailureReason> {
    let turn = tokio::time::timeout(TURN_TIMEOUT, session.turn(TurnInput::text(prompt)).run())
        .await
        .map_err(|_| FailureReason::TurnTimedOut {
            agent: agent.to_string(),
        })?
        .map_err(FailureReason::harness)?;
    match turn.result.outcome {
        lash::TurnOutcome::Stopped(lash::TurnStop::MaxTurns) => {
            Err(FailureReason::TurnBudgetExhausted {
                agent: agent.to_string(),
            })
        }
        lash::TurnOutcome::Stopped(lash::TurnStop::Incomplete) => {
            Err(FailureReason::OutputTokenCeilingExhausted {
                agent: agent.to_string(),
            })
        }
        lash::TurnOutcome::Stopped(_) => Err(FailureReason::Harness {
            detail: format!("{agent} turn stopped: {:?}", turn.result.outcome),
        }),
        lash::TurnOutcome::Finished(_) | lash::TurnOutcome::AgentFrameSwitch { .. } => Ok(()),
    }
}

fn spend_failure(ledger: &SpendLedger) -> FailureReason {
    let spend = ledger.snapshot();
    FailureReason::SpendCapExceeded {
        total_usd: spend.total_usd,
        cap_usd: spend.cap_usd,
    }
}

fn fresh_nonce() -> String {
    Uuid::new_v4().simple().to_string().to_ascii_uppercase()
}

#[derive(Deserialize)]
struct IdentifyResponse {
    user_id: String,
}

#[derive(Deserialize)]
struct CreateChannelResponse {
    id: String,
    name: String,
}

async fn create_attempt_channel(base_url: &str, attempt: usize) -> Result<(String, String)> {
    let http = reqwest::Client::new();
    let identity: IdentifyResponse = http
        .post(format!("{base_url}/platform/identify"))
        .json(&json!({ "name": format!("FIG-1388 harness {attempt}") }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let requested_name = format!(
        "fig1388-{attempt}-{}",
        fresh_nonce()[..8].to_ascii_lowercase()
    );
    let channel: CreateChannelResponse = http
        .post(format!("{base_url}/platform/channels"))
        .json(&json!({ "name": requested_name, "user_id": identity.user_id }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok((channel.id, channel.name))
}

async fn read_transcript(api: &SlackApi, channel: &str) -> Result<Vec<TranscriptMessage>> {
    let history = api
        .conversations_history(&HistoryQuery::latest(channel, 999))
        .await?;
    let mut messages: Vec<_> = history
        .messages
        .into_iter()
        .map(|message| TranscriptMessage {
            ts: message.ts,
            author: message
                .username
                .or(message.user)
                .or(message.bot_id)
                .unwrap_or_else(|| "unknown".to_string()),
            text: message.text,
        })
        .collect();
    messages.reverse();
    Ok(messages)
}

fn run_browser_assertion(
    config: &Config,
    attempt_dir: &Path,
    channel_name: &str,
    nonce_a: &str,
    nonce_b: &str,
) -> std::result::Result<(), String> {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/slack-clone-live-model-ui.py");
    let status = Command::new("uv")
        .arg("run")
        .arg(script)
        .arg("--base-url")
        .arg(&config.base_url)
        .arg("--artifact-dir")
        .arg(attempt_dir)
        .arg("--channel-name")
        .arg(channel_name)
        .arg("--nonce-a")
        .arg(nonce_a)
        .arg("--nonce-b")
        .arg(nonce_b)
        .status()
        .map_err(|error| format!("start browser assertion: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("browser assertion exited with {status}"))
    }
}

#[derive(Debug, Serialize)]
struct RunReport {
    schema: &'static str,
    rlm_model: String,
    standard_model: String,
    max_spend_usd: f64,
    maximum_provider_calls: usize,
    maximum_rlm_calls: usize,
    maximum_standard_calls: usize,
    input_token_cap_per_call: usize,
    output_token_cap_per_call: usize,
    worst_case_spend_usd: f64,
    smoke_probes: Vec<SmokeProbe>,
    attempts: Vec<AttemptReport>,
    passed_attempt: Option<usize>,
    spend: SpendSnapshot,
    passed: bool,
    failure: Option<FailureReason>,
}

pub async fn run() -> Result<()> {
    let Some(config) = Config::from_env_and_args()? else {
        return Ok(());
    };
    std::fs::create_dir_all(&config.artifact_dir)
        .with_context(|| format!("create artifact dir {}", config.artifact_dir.display()))?;
    let ledger = SpendLedger::new(
        config.max_spend_usd,
        config.artifact_dir.join("spend-ledger.json"),
    );
    let smoke = match run_smoke_probes(&config, &ledger).await {
        Ok(smoke) => smoke,
        Err(failure) => {
            let report = final_report(
                &config,
                &ledger,
                Vec::new(),
                Vec::new(),
                None,
                Some(failure),
            );
            write_json(&config.artifact_dir.join("run-summary.json"), &report)?;
            bail!("live-model structural smoke probes failed; see run-summary.json");
        }
    };
    if config.smoke_only {
        let report = final_report(&config, &ledger, smoke, Vec::new(), None, None);
        write_json(&config.artifact_dir.join("run-summary.json"), &report)?;
        println!(
            "slack-clone live smoke PASS: calls={} cost_usd={:.6}",
            report.spend.records.len(),
            report.spend.total_usd
        );
        return Ok(());
    }
    let api = Arc::new(SlackApi::new(
        &config.base_url,
        "slack-clone-local-dev-token",
    )?);
    let mut attempts = Vec::new();
    let mut passed_attempt = None;
    for attempt in 1..=MAX_ATTEMPTS {
        let report = run_attempt(&config, &ledger, Arc::clone(&api), attempt).await;
        let passed = report.passed;
        let spend_exceeded = matches!(report.failure, Some(FailureReason::SpendCapExceeded { .. }))
            || ledger.is_exceeded();
        attempts.push(report);
        let interim = final_report(
            &config,
            &ledger,
            smoke.clone(),
            attempts.clone(),
            passed.then_some(attempt),
            None,
        );
        write_json(&config.artifact_dir.join("run-summary.json"), &interim)?;
        if passed {
            passed_attempt = Some(attempt);
            break;
        }
        if spend_exceeded {
            break;
        }
    }
    let failure = if passed_attempt.is_none() {
        attempts.last().and_then(|attempt| attempt.failure.clone())
    } else {
        None
    };
    let report = final_report(&config, &ledger, smoke, attempts, passed_attempt, failure);
    write_json(&config.artifact_dir.join("run-summary.json"), &report)?;
    if report.passed {
        println!(
            "slack-clone live nonce swap PASS: attempt={} cost_usd={:.6}",
            report.passed_attempt.unwrap_or_default(),
            report.spend.total_usd
        );
        Ok(())
    } else {
        bail!(
            "slack-clone live nonce swap failed after {} attempt(s)",
            report.attempts.len()
        )
    }
}

fn final_report(
    config: &Config,
    ledger: &SpendLedger,
    smoke_probes: Vec<SmokeProbe>,
    attempts: Vec<AttemptReport>,
    passed_attempt: Option<usize>,
    failure: Option<FailureReason>,
) -> RunReport {
    RunReport {
        schema: "lash.slack-clone.live-model-e2e.v1",
        rlm_model: config.rlm_model.clone(),
        standard_model: config.standard_model.clone(),
        max_spend_usd: config.max_spend_usd,
        maximum_provider_calls: maximum_provider_calls(),
        maximum_rlm_calls: maximum_rlm_calls(),
        maximum_standard_calls: maximum_standard_calls(),
        input_token_cap_per_call: MAX_INPUT_TOKENS_PER_CALL,
        output_token_cap_per_call: config.output_token_cap,
        worst_case_spend_usd: worst_case_spend_usd(config.output_token_cap),
        smoke_probes,
        attempts,
        passed_attempt,
        spend: ledger.snapshot(),
        passed: failure.is_none() && (passed_attempt.is_some() || config.smoke_only),
        failure,
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_priced_and_unknown_models_are_not() {
        assert!(price_for(DEFAULT_RLM_MODEL).is_some());
        assert!(price_for(DEFAULT_STANDARD_MODEL).is_some());
        assert!(price_for("vendor/unknown").is_none());
    }

    #[test]
    fn default_cap_derives_a_bounded_output_ceiling() {
        let cap = derived_output_token_cap(DEFAULT_MAX_SPEND_USD).expect("default cap");
        assert!((MIN_OUTPUT_TOKENS..=MAX_OUTPUT_TOKENS).contains(&cap));
        assert!(worst_case_spend_usd(cap) <= DEFAULT_MAX_SPEND_USD);
    }

    #[test]
    fn continuation_budget_stays_within_per_agent_ceiling() {
        assert_eq!(
            MAX_SESSION_TURNS_PER_AGENT * MAX_MODEL_TURNS_PER_SESSION_TURN,
            MAX_MODEL_TURNS_PER_AGENT
        );
    }

    #[test]
    fn nonces_are_fresh_long_alphanumeric_tokens() {
        let a = fresh_nonce();
        let b = fresh_nonce();
        assert_ne!(a, b);
        assert!(a.len() >= 16);
        assert!(a.chars().all(|character| character.is_ascii_alphanumeric()));
    }
}
