//! Compiled sources for the Rust snippets on `docs/rlm.html`.

use lash::provider::ProviderHandle;

async fn rlm_core(provider: ProviderHandle, model_id: &str) -> anyhow::Result<()> {
    // docs:start:rlm-core
    use std::sync::Arc;

    use lash::TurnInput;

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(factory)
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
        .build()?;

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
