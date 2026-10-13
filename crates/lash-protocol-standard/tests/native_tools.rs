use lash_core::plugin::PluginSessionRequest;
use std::sync::Arc;

use lash_core::facade_support::PluginHost;

/// A catalogue tool named `batch` could never be called while the sugar names
/// it in every request, so it is refused; withheld, the name is free.
#[test]
fn a_catalogue_tool_named_batch_is_refused_while_the_sugar_is_offered() {
    fn catalog_with_batch_tool(
        config: lash_protocol_standard::StandardProtocolConfig,
    ) -> Result<Vec<String>, lash_core::PluginError> {
        let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(NamedBatch);
        let session = PluginHost::new(
            vec![
                Arc::new(
                    lash_protocol_standard::StandardProtocolPluginFactory::with_config(config),
                ),
                Arc::new(lash_core::plugin::StaticPluginFactory::new(
                    lash_core::plugin::PluginDeclaration::initial("catalogue-batch"),
                    lash_core::facade_support::PluginSpec::new()
                        .with_tool_provider(lash_core::hook_key!("tools"), tools),
                )),
            ],
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
                lash_core::facade_support::SystemClock,
            )),
        )
        .build_session(PluginSessionRequest::creation(
            "root",
            lash_core::plugin::SessionAuthorityContext::fixture(),
        ))?;
        Ok(session
            .resolved_tool_catalog()?
            .tool_names()
            .as_ref()
            .clone())
    }

    let offered =
        catalog_with_batch_tool(lash_protocol_standard::StandardProtocolConfig::default());
    assert!(
        matches!(
            offered,
            Err(lash_core::PluginError::ResidentToolDuplicateName { ref name }) if name == "batch"
        ),
        "{offered:?}"
    );
    let withheld = catalog_with_batch_tool(
        lash_protocol_standard::StandardProtocolConfig::default()
            .batch(lash_protocol_standard::BatchSugar::Disabled),
    );
    assert_eq!(withheld.ok(), Some(vec!["batch".to_string()]));
}

struct NamedBatch;

#[expect(clippy::expect_used, reason = "this fixture declares valid schemas")]
fn named_batch() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:catalogue_batch",
        "batch",
        "A catalogue tool that happens to be named batch.",
        serde_json::json!({ "type": "object" }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for NamedBatch {
    fn tool_manifests(&self, _config: &lash_core::OwnerConfigView) -> Vec<lash_core::ToolManifest> {
        vec![named_batch().manifest()]
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == "batch").then(|| Arc::new(named_batch().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("catalogue batch")).into()
    }
}
