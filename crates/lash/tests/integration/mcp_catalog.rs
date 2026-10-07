#![cfg(feature = "mcp")]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::Arc;

use lash::mcp::{McpPluginFactory, McpServerConfig, McpStdioTransport};
use lash::plugins::PluginFactory;

/// The session's prompt sections composed for a turn call that offers every
/// tool of `catalog` natively (ADR 0133).
#[expect(
    clippy::expect_used,
    reason = "a test helper fails loudly on a prompt that does not compose"
)]
async fn turn_prompt(
    session: &lash_core::plugin::PluginSession,
    catalog: &lash_core::ToolCatalog,
) -> String {
    use lash::plugins::{OfferedTools, PromptCall, PromptCut, PromptCutParts, PromptRenderPool};
    use lash::prompt::{PromptPlan, PromptPurpose};
    let cut = PromptCut::new(PromptCutParts {
        call: PromptCall {
            session_id: lash::SessionId::from("advertised-surface"),
            frame: None,
            run: None,
            turn: None,
            iteration: 0,
            call: 0,
            purpose: PromptPurpose::Turn,
        },
        config: session.admitted_plugin_config(),
        session: None,
        offered: OfferedTools {
            native: catalog.tool_names().to_vec(),
            callable: Vec::new(),
            catalog: Arc::new(catalog.clone()),
        },
        model: Default::default(),
        history: Default::default(),
        namespaces: Default::default(),
    });
    session
        .prompt_catalog()
        .compose(
            &PromptPlan::default(),
            &PromptPurpose::Turn,
            Arc::new(cut),
            PromptRenderPool::shared(),
        )
        .await
        .expect("the prompt composes")
        .initial_instructions
        .unwrap_or_default()
}

/// L3 (FIG-4859): a reopened session's recorded tool surface is not rewritten
/// when the server's advertised tools changed since it was recorded — the
/// recorded catalog still serves and every missing or moved entry is judged
/// by its typed drift.
#[tokio::test]
async fn recorded_tool_surface_is_preserved_when_advertised_tools_change() {
    fn peer(server: &str, tool: &str) -> McpServerConfig {
        let initialize = serde_json::json!({
            "jsonrpc":"2.0", "id":0, "result": {
                "protocolVersion":"2025-11-25", "capabilities":{"tools":{}},
                "serverInfo":{"name":server,"version":"1"},
                "instructions": format!("the {tool}-era peer instructions")
            }
        });
        let tools = serde_json::json!({"jsonrpc":"2.0", "id":1, "result":{"tools":[
            {"name":tool, "description":format!("the {tool} tool"), "inputSchema":{"type":"object"}}
        ]}});
        McpServerConfig::stdio(McpStdioTransport::new("sh", vec![
            "-c".to_string(),
            "read -r _; printf '%s\\n' \"$INITIALIZE\"; read -r _; read -r _; printf '%s\\n' \"$TOOLS\"; cat >/dev/null".to_string(),
        ]).with_env([
            ("INITIALIZE", initialize.to_string()),
            ("TOOLS", tools.to_string()),
        ]))
    }

    let protocol = || {
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn PluginFactory>
    };
    let lookup = lash::mcp::mcp_tool_names("records", &["lookup"])["lookup"].clone();
    let search = lash::mcp::mcp_tool_names("records", &["search"])["search"].clone();

    // The session was created while the server advertised `lookup`.
    let created_factory = Arc::new(
        McpPluginFactory::new(BTreeMap::from([(
            "records".to_string(),
            peer("created-peer", "lookup"),
        )]))
        .await
        .expect("created peer connects"),
    );
    // The plugin configuration a real creation records: the protocol's
    // recorded `behaviour` is what its rematerialization must find.
    let created_host =
        lash_core::facade_support::PluginHost::new(vec![protocol(), created_factory.clone()]);
    let plugin_config = created_host
        .resolve_creation_plugin_config(
            Some(lash_protocol_standard::STANDARD_PROTOCOL_PLUGIN_ID),
            &lash_core::PluginOptions::default(),
            None,
            true,
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the creation config resolves");
    let created = created_host
        .build_session(lash_core::plugin::PluginSessionRequest::creation(
            "advertised-surface",
            lash_core::plugin::SessionAuthorityContext {
                plugin_config: lash_core::AdmittedPluginConfig::new(plugin_config, 0),
                ..Default::default()
            },
        ))
        .expect("created session");
    let recorded = created.resolved_tool_catalog().expect("recorded catalog");
    assert!(recorded.has_callable_tool(&lookup), "lookup advertised");
    let recorded_bytes = serde_json::to_vec(&*recorded).expect("record the catalog");
    let snapshot = created.export_state();
    let config = created.admitted_plugin_config();

    // The reopened deployment's server advertises `search` instead.
    let reopened_factory = Arc::new(
        McpPluginFactory::new(BTreeMap::from([(
            "records".to_string(),
            peer("reopened-peer", "search"),
        )]))
        .await
        .expect("reopened peer connects"),
    );
    let reopened =
        lash_core::facade_support::PluginHost::new(vec![protocol(), reopened_factory.clone()])
            .build_session(lash_core::plugin::PluginSessionRequest::rematerialization(
                "advertised-surface",
                &snapshot,
                lash_core::plugin::SessionAuthorityContext {
                    plugin_config: config,
                    ..Default::default()
                },
            ))
            .expect("reopened session");
    let live = reopened.resolved_tool_catalog().expect("live catalog");
    assert!(
        live.has_callable_tool(&search),
        "the live surface follows the advertisement"
    );
    assert!(!live.has_callable_tool(&lookup));

    // Serving the recorded surface consults the record, not the advertisement:
    // the recorded catalog names `lookup`, and judged against the live
    // catalog its drift is typed, not substituted.
    let restored: lash_core::ToolCatalog =
        serde_json::from_slice(&recorded_bytes).expect("restore the recorded catalog");
    assert!(restored.has_callable_tool(&lookup));
    assert!(!restored.has_callable_tool(&search));
    let recorded_definition = lash_core::ToolDefinition {
        manifest: restored
            .tools
            .iter()
            .find(|entry| entry.manifest.name == lookup)
            .expect("the recorded surface names lookup")
            .manifest
            .clone(),
        contract: (*restored
            .tools
            .iter()
            .find(|entry| entry.manifest.name == lookup)
            .expect("the recorded surface names lookup")
            .contract)
            .clone(),
    };
    let drift = lash_core::ToolSurfaceDrift::judge(&recorded_definition, &live)
        .expect("a recorded tool the advertisement dropped drifts");
    assert_eq!(drift.kind, lash_core::ToolSurfaceDriftKind::Missing);
    // The server's guidance section renders the guidance the offered
    // manifests pin: the recorded catalog carries the creating peer's
    // instructions, so rendering it serves them while the live catalog
    // serves the successor's.
    let recorded_prompt = turn_prompt(&reopened, &restored).await;
    assert!(
        recorded_prompt.contains("the lookup-era peer instructions"),
        "the record is served: {recorded_prompt}"
    );
    assert!(
        !recorded_prompt.contains("the search-era peer instructions"),
        "the advertisement does not leak in"
    );
    let live_prompt = turn_prompt(&reopened, &live).await;
    assert!(
        live_prompt.contains("the search-era peer instructions"),
        "the live surface is the advertisement's own"
    );
    assert_eq!(
        reopened.admitted_plugin_config(),
        created.admitted_plugin_config(),
        "the recorded config is preserved"
    );
    created_factory
        .shutdown()
        .await
        .expect("created peer shutdown");
    reopened_factory
        .shutdown()
        .await
        .expect("reopened peer shutdown");
}
