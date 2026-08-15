//! Compiled sources for the Rust snippets on `docs/plugins.html`.

use std::sync::Arc;

use lash::plugins::{PluginError, PluginFactory, PluginSessionContext, SessionPlugin};
use lash::provider::ProviderHandle;

struct AppPlugin;

impl SessionPlugin for AppPlugin {
    fn id(&self) -> &'static str {
        "app"
    }

    fn register(&self, _reg: &mut lash::plugins::PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}

struct AppPluginFactory;

impl PluginFactory for AppPluginFactory {
    fn id(&self) -> &'static str {
        "app"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(AppPlugin))
    }
}

async fn plugin_core(provider: ProviderHandle, model_id: &str) -> anyhow::Result<()> {
    // docs:start:plugin-core
    use std::sync::Arc;

    use lash::plugins::PluginFactory;

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
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
        .configure_plugins(|plugins| {
            plugins.push(Arc::new(AppPluginFactory) as Arc<dyn PluginFactory>);
        })
        .build(crate::example_process_owner())?;
    // docs:end:plugin-core
    Ok(())
}

#[derive(Default)]
struct PlanState {
    steps: Vec<String>,
}

// docs:start:update-plan-plugin
use std::sync::Mutex;

use lash::plugins::PluginRegistrar;
use lash::tools::{ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolProvider, ToolResult};

const PLUGIN_ID: &str = "update_plan";

pub struct UpdatePlanPluginFactory;

impl PluginFactory for UpdatePlanPluginFactory {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(UpdatePlanPlugin {
            active: ctx.is_root_session(),
            state: Arc::new(Mutex::new(PlanState::default())),
        }))
    }
}

struct UpdatePlanPlugin {
    active: bool,
    state: Arc<Mutex<PlanState>>,
}

impl SessionPlugin for UpdatePlanPlugin {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        if !self.active {
            return Ok(());
        }
        reg.tools().provider(Arc::new(UpdatePlanTool {
            state: Arc::clone(&self.state),
        }))?;
        Ok(())
    }
}

struct UpdatePlanTool {
    state: Arc<Mutex<PlanState>>,
}

#[async_trait::async_trait]
impl ToolProvider for UpdatePlanTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![update_plan_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "update_plan").then(|| Arc::new(update_plan_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        // Validate call.args, mutate state, then return a typed payload.
        ToolResult::ok(serde_json::json!({ "generation": 1 }))
    }
}

fn update_plan_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:update_plan",
        "update_plan",
        "Publish or replace the current plan.",
        serde_json::json!({ "type": "object", "properties": {} }),
        serde_json::json!({}),
    )
}
// docs:end:update-plan-plugin

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_plugin_builder_resolves() {
        plugin_core(crate::test_support::provider(), "docs-snippet-test")
            .await
            .expect("plugin snippet must build");
    }

    #[test]
    fn bounded_reinspection_conflict_keeps_plugin_identity() {
        let error = PluginError::BeforeToolCallReplacementConflict {
            replacing_plugin_id: "normalizer".to_string(),
            repeated_plugin_id: "policy".to_string(),
        };
        let rendered = error.to_string();
        let PluginError::BeforeToolCallReplacementConflict {
            replacing_plugin_id,
            repeated_plugin_id,
        } = error
        else {
            panic!("expected a typed before-tool replacement conflict");
        };

        assert_eq!(replacing_plugin_id, "normalizer");
        assert_eq!(repeated_plugin_id, "policy");
        assert!(rendered.contains("normalizer") && rendered.contains("policy"));
        assert_eq!(
            rendered,
            "before_tool_call replacement from `normalizer` was replaced again by `policy` during bounded reinspection"
        );

        let error = PluginError::AfterToolCallReplacementConflict {
            replacing_plugin_id: "injector".to_string(),
            repeated_plugin_id: "output_policy".to_string(),
        };
        let rendered = error.to_string();
        let PluginError::AfterToolCallReplacementConflict {
            replacing_plugin_id,
            repeated_plugin_id,
        } = error
        else {
            panic!("expected a typed after-tool replacement conflict");
        };

        assert_eq!(replacing_plugin_id, "injector");
        assert_eq!(repeated_plugin_id, "output_policy");
        assert!(rendered.contains("injector") && rendered.contains("output_policy"));
        assert_eq!(
            rendered,
            "after_tool_call replacement from `injector` was replaced again by `output_policy` during bounded reinspection"
        );
    }
}
