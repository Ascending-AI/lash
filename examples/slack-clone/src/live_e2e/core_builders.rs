use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use lash::direct::GenerationOptions;
use lash::provider::{
    CacheControlDialect, ModelCapability, ProviderHandle, ProviderOptions, ProviderReliability,
    ReasoningCapability, ReasoningEncoding, ReasoningSelection, SamplingCapability,
};
use lash::{LashCore, ModelSpec};
use lash_provider_openai::{
    OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider, ProviderRoutingPrefs,
};

use super::{
    Config, DEFAULT_RLM_MODEL, DEFAULT_STANDARD_MODEL, MAX_MODEL_TURNS_PER_SESSION_TURN,
    MeteredProvider, SpendLedger,
};

pub(super) fn provider(config: &Config, ledger: &SpendLedger) -> ProviderHandle {
    let options = ProviderOptions {
        reliability: ProviderReliability::disabled(),
        max_output_tokens: Some(config.output_token_cap as u64),
        ..ProviderOptions::default()
    };
    let mut compat = OpenAiCompat::openrouter();
    // Sonnet 5 advertises tools but not parallel_tool_calls; Lash gates that field behind request_fields.
    compat.request_fields = Some(false);
    compat.provider_routing = Some(ProviderRoutingPrefs {
        require_parameters: true,
        ..ProviderRoutingPrefs::default()
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

pub(super) fn model_spec(model: &str, output_cap: usize) -> Result<ModelSpec> {
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

pub(super) fn standard_core(
    provider: ProviderHandle,
    model: ModelSpec,
    output_cap: usize,
    turn_budget: usize,
    instructions: &str,
    tools: Option<Arc<dyn ToolProvider>>,
    trace_path: PathBuf,
) -> Result<LashCore> {
    let mut builder = LashCore::standard_builder(lash::TurnBudget::bounded(turn_budget))
        .without_queued_work()
        .provider(provider)
        .model(model)
        .generation(generation(output_cap))
        .instructions(instructions)
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
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
    if let Some(marker) = super::shutdown_marker::factory_from_env("slack-clone-live-e2e")
        .map_err(anyhow::Error::msg)?
    {
        builder = builder.plugin(marker);
    }
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "slack-clone-live-standard",
            Uuid::new_v4().to_string(),
        ))
        .context("build standard live-E2E core")
}

pub(super) fn rlm_core(
    provider: ProviderHandle,
    model: ModelSpec,
    output_cap: usize,
    instructions: &str,
    tools: Arc<dyn ToolProvider>,
    trace_path: PathBuf,
) -> Result<LashCore> {
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(30))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let mut builder = LashCore::rlm_builder(
        lash::TurnBudget::bounded(MAX_MODEL_TURNS_PER_SESSION_TURN),
        factory,
    )
    .without_queued_work()
    .provider(provider)
    .model(model)
    .generation(generation(output_cap))
    .instructions(instructions)
    .tools(tools)
    .effect_host(Arc::new(
        lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
    ))
    .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
    .process_env_store(Arc::new(
        lash::persistence::InMemoryProcessExecutionEnvStore::new(),
    ))
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .trace_sink(Arc::new(JsonlTraceSink::new(trace_path)))
    .trace_level(TraceLevel::Extended);
    if let Some(marker) = super::shutdown_marker::factory_from_env("slack-clone-live-e2e")
        .map_err(anyhow::Error::msg)?
    {
        builder = builder.plugin(marker);
    }
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "slack-clone-live-rlm",
            Uuid::new_v4().to_string(),
        ))
        .context("build RLM live-E2E core")
}
