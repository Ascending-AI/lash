//! H1's node-side production provider fixture. Both cheap and live hosts
//! build the same core; every input enters through the public send API.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use lash::provider::{ProviderHandle, ProviderOptions, ProviderReliability};
use lash_provider_openai::{OpenAiCompat, OpenAiCompatibleProvider};

pub const PROFILE: &str = "e2e/model";
pub const INTRO: &str = "H1 transcript.";

pub fn core(
    backend: lash::Backend,
    url: &str,
    reliability: ProviderReliability,
) -> Result<lash::LashCore> {
    build(builder(backend, url, reliability)?)
}

pub fn core_with_tools(
    backend: lash::Backend,
    url: &str,
    reliability: ProviderReliability,
    tools: super::e2e_tools::ToolFixtureArgs,
) -> Result<lash::LashCore> {
    build(builder(backend, url, reliability)?.plugin(Arc::new(tools)))
}

fn build(builder: lash::LashCoreBuilder) -> Result<lash::LashCore> {
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "h1-provider-host",
            format!("h1-{}", std::process::id()),
        ))
        .map_err(|error| anyhow!("build H1 provider host: {error}"))
}

fn builder(
    backend: lash::Backend,
    url: &str,
    reliability: ProviderReliability,
) -> Result<lash::LashCoreBuilder> {
    let provider = OpenAiCompatibleProvider::new("fixture-key", url)
        .with_compat(OpenAiCompat::local())
        .with_options(ProviderOptions {
            reliability,
            ..Default::default()
        });
    let model = lash::LlmProfileMetadata::builder(PROFILE)
        .context_window_tokens(16_000)
        .build()?;
    Ok(lash::LashCore::standard_builder(backend)
        .llm_profiles(Arc::new(lash::LlmProfileRegistry::new().register(
            PROFILE,
            lash::RegisteredLlmProfile::new(model, ProviderHandle::new(provider.into_components())),
        )?))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024)))
}

pub fn spec() -> Result<lash::SessionSpec> {
    Ok(lash::SessionSpec::new(
        PROFILE,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    )
    .plugin(
        lash::standard::STANDARD_PROTOCOL_PLUGIN_ID,
        lash::standard::StandardTurnOptions {
            prompt: Some(lash::standard::StandardPrompt {
                intro: Some(INTRO.into()),
                omit_builtin_guidance: true,
                ..Default::default()
            }),
            render: None,
        },
    )?)
}

pub async fn session(core: &lash::LashCore, id: &str) -> Result<lash::LashSession> {
    let spec = spec()?;
    let id = lash::SessionId::fixture(id);
    core.session(id.clone())
        .create(lash::SessionCreation::root(spec))
        .await?;
    Ok(core.session(id).open().await?)
}
