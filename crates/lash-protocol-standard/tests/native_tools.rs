use lash_core::plugin::PluginSessionRequest;
use std::sync::Arc;

use lash_core::facade_support::PluginHost;

#[expect(
    clippy::expect_used,
    reason = "test support: the session fixture always admits a tool catalog view for the root session"
)]
fn tool_names(session: &lash_core::facade_support::PluginSession) -> Vec<String> {
    session
        .resolved_tool_catalog()
        .expect("tool catalog")
        .tool_names()
        .as_ref()
        .clone()
}

#[expect(
    clippy::expect_used,
    reason = "test support: the fixture host build always succeeds for these session fixtures"
)]
fn standard_session_with_access(
    session_id: &str,
    tool_access: lash_core::SessionToolAccess,
) -> Arc<lash_core::facade_support::PluginSession> {
    let fixture: Arc<dyn lash_core::ToolProvider> = Arc::new(lash_core::testing::FixtureTools);
    PluginHost::new(vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("native-tools-fixture"),
            lash_core::facade_support::PluginSpec::new().with_tool_provider(fixture),
        )),
    ])
    .build_session(PluginSessionRequest::creation(
        lash_core::SessionId::fixture(session_id),
        lash_core::plugin::SessionAuthorityContext {
            tool_access,
            ..Default::default()
        },
    ))
    .expect("standard protocol session")
}

#[test]
fn standard_protocol_distinguishes_ambient_from_restricted_empty_access() {
    let ambient =
        standard_session_with_access("standard-ambient", lash_core::SessionToolAccess::ambient());
    assert!(tool_names(&ambient).contains(&lash_core::testing::FIXTURE_ECHO_TOOL.to_string()));

    let restricted = standard_session_with_access(
        "standard-restricted-empty",
        lash_core::SessionToolAccess::restricted([]).expect("restricted empty is valid"),
    );
    assert!(tool_names(&restricted).is_empty());
}

/// A catalogue tool named `batch` could never be called while the sugar names
/// it in every request, so it is refused; withheld, the name is free.
#[test]
fn a_catalogue_tool_named_batch_is_refused_while_the_sugar_is_offered() {
    fn catalog_with_batch_tool(
        config: lash_protocol_standard::StandardProtocolConfig,
    ) -> Result<Vec<String>, lash_core::PluginError> {
        let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(NamedBatch);
        let session = PluginHost::new(vec![
            Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::with_config(config)),
            Arc::new(lash_core::plugin::StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("catalogue-batch"),
                lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
            )),
        ])
        .build_session(PluginSessionRequest::creation("root", Default::default()))?;
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
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for NamedBatch {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![named_batch().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "batch").then(|| Arc::new(named_batch().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("catalogue batch")).into()
    }
}
