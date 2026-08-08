//! Compiled sources for the Rust snippets on `docs/execution-modes.html`.

use std::sync::Arc;

use lash::ModelSpec;
use lash::provider::ProviderHandle;

async fn standard_mode(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:standard-core
    // `LashCore::standard_builder()` selects native provider tool-calling, the default mode.
    let core = lash::LashCore::standard_builder()
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .build()?;

    // A plain `open()` runs the session in standard mode.
    let session = core.session("chat-1").open().await?;
    // docs:end:standard-core
    Ok(())
}

async fn rlm_mode(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:rlm-core
    // Build an RLM core for Lashlang-driven turns.
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .build()?;

    let session = core.session("task-1").open().await?;
    // docs:end:rlm-core
    Ok(())
}
