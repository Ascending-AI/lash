//! Compiled sources for the Rust snippets on `docs/plugins-runtime.html`.

use std::sync::Arc;

use lash::LashCore;
use lash::plugins::{PluginError, PluginFactory, PluginSessionContext, SessionPlugin};
use lash::provider::ProviderHandle;

struct UpdatePlanPlugin;

impl SessionPlugin for UpdatePlanPlugin {
    fn id(&self) -> &'static str {
        "update_plan"
    }

    fn register(&self, _reg: &mut lash::plugins::PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}

struct UpdatePlanPluginFactory;

impl PluginFactory for UpdatePlanPluginFactory {
    fn id(&self) -> &'static str {
        "update_plan"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(UpdatePlanPlugin))
    }
}

async fn plugin_install(provider: ProviderHandle) -> anyhow::Result<()> {
    // docs:start:plugin-install
    use std::sync::Arc;

    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("anthropic/claude-sonnet-4.6")
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
        .plugin(Arc::new(UpdatePlanPluginFactory) as Arc<dyn PluginFactory>)
        .build()?;
    // docs:end:plugin-install
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_runtime_plugin_builder_resolves() {
        plugin_install(crate::test_support::provider())
            .await
            .expect("runtime-plugin snippet must build");
    }
}
