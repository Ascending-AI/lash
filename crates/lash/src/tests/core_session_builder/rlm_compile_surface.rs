//! The RLM factory's compile surface needs no session: it compiles over the
//! plugin host the caller builds, and the module it compiles publishes into
//! the backend's artifact store.

use super::*;
use lash_core::ToolDefinitionBindingExt as _;

fn test_tool_definition_with_tool_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    definition.with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct CompileSurfaceToolConfig {
    tool_name: String,
}

struct CompileSurfaceToolFactory {
    id: &'static str,
    default_tool_name: &'static str,
}

impl CompileSurfaceToolFactory {
    fn new(id: &'static str, default_tool_name: &'static str) -> Self {
        Self {
            id,
            default_tool_name,
        }
    }
}

impl lash_core::facade_support::PluginFactory for CompileSurfaceToolFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(
        &self,
        ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        let config = ctx
            .plugin_config
            .decode::<CompileSurfaceToolConfig>(self.id)
            .map_err(|err| lash_core::PluginError::Registration(err.to_string()))?;
        let tool_name = config
            .map(|config| config.tool_name)
            .unwrap_or_else(|| self.default_tool_name.to_string());
        Ok(Arc::new(CompileSurfaceToolPlugin {
            plugin_id: self.id,
            tool_name,
        }))
    }
}

impl lash_core::plugin::PluginMetadata for CompileSurfaceToolFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id)
    }
}

struct CompileSurfaceToolPlugin {
    plugin_id: &'static str,
    tool_name: String,
}

impl lash_core::facade_support::SessionPlugin for CompileSurfaceToolPlugin {
    fn id(&self) -> &'static str {
        self.plugin_id
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.tools().provider(Arc::new(CompileSurfaceToolProvider {
            tool_name: self.tool_name.clone(),
        }))?;
        Ok(())
    }
}

struct CompileSurfaceToolProvider {
    tool_name: String,
}

#[async_trait]
impl lash_core::ToolProvider for CompileSurfaceToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![compile_surface_tool_definition(&self.tool_name).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.tool_name)
            .then(|| Arc::new(compile_surface_tool_definition(&self.tool_name).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })) })
            .await
            .into()
    }
}

fn compile_surface_tool_definition(name: &str) -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name.to_string(),
            "Compile-surface test tool.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120)),
        name.to_string(),
    )
}

#[tokio::test]
async fn rlm_compile_surface_uses_core_plugins_extra_plugins_and_request_options() -> Result<()> {
    // The compile APIs are now operations over the RLM factory and a plugin host
    // the caller builds. The plugin host carries the core tool plugin plus any
    // extra tool plugins; the request's execution env plugin options configure
    // them (here `compile-extra-tool` resolves to `lookup`).
    let backend = sqlite_memory_store_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend.clone());
    let factory = Arc::new(rlm_factory(&backend));
    let plugin_host = lash_core::facade_support::PluginHost::new(
        vec![
            Arc::clone(&factory) as Arc<dyn PluginFactory>,
            Arc::new(CompileSurfaceToolFactory::new(
                "compile-core-tool",
                "compile_core_tool",
            )),
            Arc::new(CompileSurfaceToolFactory::new(
                "compile-extra-tool",
                "fallback",
            )),
        ],
        lash_core::ExecutionBudgets::recommended(),
    );
    let plugin_config = || {
        let mut config = lash_core::PluginConfig::default();
        config.insert(
            "compile-extra-tool",
            serde_json::to_value(CompileSurfaceToolConfig {
                tool_name: "lookup".to_string(),
            })
            .expect("compile plugin config serializes"),
        );
        lash_core::AdmittedPluginConfig::new(config, 0)
    };
    let request = crate::rlm::LashlangCompileSurfaceRequest::new(
        "compile-surface",
        lash_core::ProcessExecutionEnvSpec::new(
            plugin_config(),
            lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                crate::NoProgressBudget::bounded(12),
            ),
        ),
    );

    let surface = factory.lashlang_compile_surface(&plugin_host, request)?;

    assert!(surface.tool_catalog.has_callable_tool("compile_core_tool"));
    assert!(surface.tool_catalog.has_callable_tool("lookup"));
    assert!(!surface.tool_catalog.has_callable_tool("fallback"));
    assert!(
        surface
            .host_environment
            .resources
            .resolve_module_operation("Tools", "tools", "compile_core_tool")
            .is_some()
    );
    assert!(
        surface
            .host_environment
            .resources
            .resolve_module_operation("Tools", "tools", "lookup")
            .is_some()
    );

    let compiled = factory
        .compile_lashlang_module(
            &plugin_host,
            crate::rlm::LashlangModuleCompileRequest::new(
                "compile-module",
                r#"
const value = await tools.lookup({});
finish(value);
"#,
                lash_core::ProcessExecutionEnvSpec::new(
                    plugin_config(),
                    lash_core::SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                        crate::NoProgressBudget::bounded(12),
                    ),
                ),
            ),
        )
        .await
        .expect("compile module through the RLM factory");
    artifact_store
        .publish_module_artifact(
            &lash_core::testing::host_pin_claim_for_testing(),
            &compiled.artifact,
        )
        .await
        .expect("publish compiled module");
    assert!(
        artifact_store
            .get_module_artifact(&compiled.module_ref)
            .await
            .expect("load persisted module artifact")
            .is_some(),
        "explicit publication should persist through the configured artifact store"
    );
    Ok(())
}
