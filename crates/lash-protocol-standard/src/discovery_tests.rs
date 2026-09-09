use super::*;
#[test]
fn standard_discovery_filters_provider_specs_and_requires_an_inline_member() {
    let tool = |name: &str, inline| {
        let mut tool = lash_core::ToolDefinition::raw(
            name,
            name,
            name,
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"string"}),
        );
        tool.manifest.inline = inline;
        tool
    };
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("tools.search", true),
        tool("hidden", false),
    ]);
    let manifests = catalog
        .tools
        .iter()
        .map(|entry| entry.manifest.clone())
        .collect::<Vec<_>>();
    for discovery in [
        None,
        Some(lash_core::ToolDiscovery {
            operation: "tools.search".into(),
        }),
    ] {
        validate_discovery(&manifests, discovery.as_ref()).unwrap();
        let expected = if discovery.is_some() { 1 } else { 2 };
        let driver = StandardProtocolDriver {
            config: StandardProtocolConfig { discovery },
        };
        let preamble = driver.build_preamble(ProtocolBuildInput {
            tool_catalog: Arc::new(catalog.clone()),
            plugin_extensions: Default::default(),
            trigger_events: Default::default(),
            extra_prompt_contributions: Vec::new(),
        });
        assert_eq!(preamble.tool_specs.len(), expected);
    }
    for operation in ["hidden", "absent"] {
        assert!(matches!(
            validate_discovery(
                &manifests,
                Some(&lash_core::ToolDiscovery {
                    operation: operation.into()
                })
            ),
            Err(PluginError::InvalidToolDiscovery { .. })
        ));
    }
}
