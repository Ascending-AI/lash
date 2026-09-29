use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use lash::direct::GenerationOptions;
use lash::provider::{
    CacheControlDialect, ModelCapability, ProviderHandle, ProviderOptions, ProviderReliability,
    ReasoningCapability, ReasoningEncoding, ReasoningSelection, SamplingCapability,
};
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall, ToolDefinition,
    ToolOutcome, ToolProvider,
};
use lash::tracing::{JsonlTraceSink, TraceLevel};
use lash::{LashCore, ModelSpec, PromptLayerSink as _};
use lash_provider_openai::{
    OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider, ProviderRoutingPrefs,
};
use uuid::Uuid;

use super::{
    Config, DEFAULT_RLM_MODEL, DEFAULT_STANDARD_MODEL, EchoArgs, EchoOutput,
    MAX_MODEL_TURNS_PER_SESSION_TURN, MeteredProvider, SpendLedger,
};

pub(super) fn provider(config: &Config, ledger: &SpendLedger) -> ProviderHandle {
    let options = ProviderOptions {
        reliability: ProviderReliability::disabled(),
        max_output_tokens: Some(config.output_token_cap as u64),
        ..ProviderOptions::default()
    };
    // Sonnet 5 advertises tools but not parallel_tool_calls; lash sends that
    // field only when the host sets it, and this host does not.
    let mut compat = OpenAiCompat::openrouter();
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

struct EchoTool;

#[async_trait::async_trait]
impl StaticToolExecute for EchoTool {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            match serde_json::from_value::<EchoArgs>(call.args.clone()) {
                Ok(args) if call.name() == "structural_echo" => {
                    ToolOutcome::ok(serde_json::json!(EchoOutput { value: args.value }))
                }
                Ok(_) => ToolOutcome::err(serde_json::json!("unknown tool")),
                Err(error) => ToolOutcome::err_fmt(format_args!("invalid arguments: {error}")),
            }
        })
        .await
        .into()
    }
}

/// The structural-echo tool the standard smoke probe asks the model to call.
pub(super) fn echo_tools() -> Arc<dyn ToolProvider> {
    Arc::new(StaticToolProvider::new(
        vec![ToolDefinition::typed::<EchoArgs, EchoOutput>(
            "tool:slack_clone.structural_echo",
            "structural_echo",
            "Return the supplied value unchanged. You must call this when requested.",
        )],
        EchoTool,
    ))
}

/// A live-E2E core on a substrate of its own: a SQLite memory store set
/// journaled by the local restate-server the process's cores share, with the
/// core's endpoint served and registered there (ADR 0104) under a namespace of
/// its own (ADR 0111), so the swap's two concurrently running cores share the
/// server.
///
/// Dereferences to the core. Fields drop in order: the core, then its
/// deployment, then its hold on the server.
pub(super) struct LiveCore {
    core: LashCore,
    _deployment: crate::local_restate::LocalDeployment,
    _server: Arc<crate::local_restate::LocalRestateServer>,
}

impl std::ops::Deref for LiveCore {
    type Target = LashCore;

    fn deref(&self) -> &LashCore {
        &self.core
    }
}

/// A core's substrate on the shared restate-server: its namespace there,
/// and the engine over a fresh SQLite memory store set that reaches it.
struct LiveEngine {
    server: Arc<crate::local_restate::LocalRestateServer>,
    restate: crate::local_restate::LocalRestate,
    engine: Arc<lash::restate::RestateEngine>,
}

/// The shared restate-server, in a namespace of `label`'s own.
async fn live_engine(label: &str) -> Result<LiveEngine> {
    let server = crate::local_restate::LocalRestateServer::shared("slack-live").await?;
    let restate = server.core(label)?;
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .map_err(|error| anyhow::anyhow!("open a SQLite memory store set: {error}"))?;
    let engine = restate.engine(Arc::new(stores));
    Ok(LiveEngine {
        server,
        restate,
        engine,
    })
}

/// Serve `core`'s endpoint in `live`'s namespace and hand back the live core.
async fn serve_live_core(live: LiveEngine, core: LashCore) -> Result<LiveCore> {
    let worker = lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .context("live-E2E process worker config")?,
    )
    .context("build the live-E2E process worker")?;
    let deployment = live
        .restate
        .serve(&live.engine, live.engine.endpoint_builder(worker).build())
        .await?;
    Ok(LiveCore {
        core,
        _deployment: deployment,
        _server: live.server,
    })
}

/// Everything `standard_core` needs beside the provider and model: the turn
/// shape, optional tools and trace sink, and an optional shutdown witness.
pub(super) struct StandardCoreSpec<'a> {
    pub(super) output_cap: usize,
    pub(super) turn_budget: usize,
    pub(super) instructions: &'a str,
    pub(super) tools: Option<Arc<dyn ToolProvider>>,
    pub(super) trace_path: PathBuf,
    pub(super) shutdown_witness: Option<Arc<dyn lash::plugins::PluginFactory>>,
}

pub(super) async fn standard_core(
    provider: ProviderHandle,
    model: ModelSpec,
    spec: StandardCoreSpec<'_>,
) -> Result<LiveCore> {
    let live = live_engine("slack-live-standard").await?;
    let core = standard_core_over(
        lash::Backend::new(live.engine.clone()),
        provider,
        model,
        spec,
    )?;
    serve_live_core(live, core).await
}

/// [`standard_core`] over `backend`, which a test hands the Restate double's.
pub(super) fn standard_core_over(
    backend: lash::Backend,
    provider: ProviderHandle,
    model: ModelSpec,
    spec: StandardCoreSpec<'_>,
) -> Result<LashCore> {
    let mut builder =
        LashCore::standard_builder(backend, lash::TurnBudget::bounded(spec.turn_budget))
            .provider(provider)
            .model(model)
            .generation(generation(spec.output_cap))
            .instructions(spec.instructions)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .trace_sink(Arc::new(JsonlTraceSink::new(spec.trace_path)))
            .trace_level(TraceLevel::Extended);
    if let Some(tools) = spec.tools {
        builder = builder.tools(tools);
    }
    if let Some(marker) = super::shutdown_marker::factory_from_env("slack-clone-live-e2e")
        .map_err(anyhow::Error::msg)?
    {
        builder = builder.plugin(marker);
    }
    if let Some(witness) = spec.shutdown_witness {
        builder = builder.plugin(witness);
    }
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "slack-clone-live-standard",
            Uuid::new_v4().to_string(),
        ))
        .context("build standard live-E2E core")
}

pub(super) async fn rlm_core(
    provider: ProviderHandle,
    model: ModelSpec,
    output_cap: usize,
    instructions: &str,
    tools: Arc<dyn ToolProvider>,
    trace_path: PathBuf,
) -> Result<LiveCore> {
    let live = live_engine("slack-live-rlm").await?;
    let backend = lash::Backend::new(live.engine.clone());
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    let mut builder = LashCore::rlm_builder(
        backend,
        lash::TurnBudget::bounded(MAX_MODEL_TURNS_PER_SESSION_TURN),
        factory,
    )
    .provider(provider)
    .model(model)
    .generation(generation(output_cap))
    .instructions(instructions)
    .tools(tools)
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .trace_sink(Arc::new(JsonlTraceSink::new(trace_path)))
    .trace_level(TraceLevel::Extended);
    if let Some(marker) = super::shutdown_marker::factory_from_env("slack-clone-live-e2e")
        .map_err(anyhow::Error::msg)?
    {
        builder = builder.plugin(marker);
    }
    let core = builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "slack-clone-live-rlm",
            Uuid::new_v4().to_string(),
        ))
        .context("build RLM live-E2E core")?;
    serve_live_core(live, core).await
}
