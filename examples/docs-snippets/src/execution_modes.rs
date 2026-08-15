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
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    // A plain `open()` runs the session in standard mode.
    let session = core.session("chat-1").open().await?;
    // docs:end:standard-core
    Ok(())
}

async fn rlm_mode(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:rlm-core
    // Build one RLM core; each session chooses its durable source dialect.
    use lash::rlm::{RlmDialect, RlmSessionBuilderExt as _};

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
            lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
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
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    // Absence is the durable Lashlang default.
    assert_eq!(RlmDialect::Lashlang.language_id(), "lashlang");
    let lashlang = core.session("task-lashlang").open().await?;
    assert_eq!(
        lashlang.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("lashlang")
    );

    // TypeScript is selected at creation and remains pinned on rehydrate.
    assert_eq!(RlmDialect::Typescript.language_id(), "typescript");
    let typescript = core
        .session("task-typescript")
        .rlm_dialect(RlmDialect::Typescript)?
        .open()
        .await?;
    assert_eq!(
        typescript.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("typescript")
    );

    // A host that offers the choice — a create form, a flag, an environment
    // variable — reads the menu from the registered dialects rather than
    // writing its own list, and refuses an unregistered id instead of quietly
    // defaulting it: the dialect is pinned for the session's whole life.
    assert_eq!(
        RlmDialect::ALL.map(RlmDialect::language_id),
        ["lashlang", "typescript"]
    );
    assert_eq!(
        RlmDialect::from_language_id("typescript"),
        Some(RlmDialect::Typescript)
    );
    assert_eq!(RlmDialect::from_language_id("lashscript"), None);
    assert_eq!(
        RlmDialect::registered_language_ids(),
        "`lashlang`, `typescript`"
    );
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
