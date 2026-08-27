//! Compiled sources for the Rust snippets on `docs/rlm.html`.

use lash::provider::ProviderHandle;

async fn rlm_core(provider: ProviderHandle, model_id: &str) -> anyhow::Result<()> {
    // docs:start:rlm-core
    use std::sync::Arc;

    use lash::TurnInput;

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(30))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .without_queued_work()
        .plugins(lash::plugins::runtime_plugin_stack())
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model_id)
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    let session = core.session("task-42").open().await?;
    let output = session
        .turn(TurnInput::text(
            "Inspect the task and finish a concise result.",
        ))
        .run()
        .await?;
    // docs:end:rlm-core
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_rlm_builder_resolves() {
        let result = rlm_core(crate::test_support::provider(), "docs-snippet-test").await;
        crate::test_support::assert_builder_resolved(result);
    }
}
