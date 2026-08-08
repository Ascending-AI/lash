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
        lash::rlm::RlmProtocolPluginConfig::default(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model_id)
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .configure_plugins(|plugins| {
            plugins.push(Arc::new(AppPluginFactory) as Arc<dyn PluginFactory>);
        })
        .build()?;
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
