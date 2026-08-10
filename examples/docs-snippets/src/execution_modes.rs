//! Compiled sources for the Rust snippets on `docs/execution-modes.html`.

use std::sync::Arc;

use lash::ModelSpec;
use lash::provider::ProviderHandle;

async fn standard_mode(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:standard-core
    // `LashCore::standard_builder(lash::TurnBudget::Unbounded)` selects native provider tool-calling, the default mode.
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
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
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .build()?;

    let session = core.session("task-1").open().await?;
    // docs:end:rlm-core
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_execution_mode_builders_resolve() {
        standard_mode(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("standard-mode snippet must build");
        rlm_mode(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("RLM snippet must build");
    }
}
