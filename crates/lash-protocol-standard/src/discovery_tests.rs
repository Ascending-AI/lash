// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

/// FIG-4544. A standard session shows a tool's schema in two places: the
/// provider spec, which is the canonical schema itself, and the catalog's
/// schema docs, which discovery and catalog projections hand back. An
/// MCP-style schema — open, nested, one field undescribed — reaches both whole.
#[test]
fn an_open_nested_schema_reaches_the_provider_spec_and_the_schema_docs_whole() {
    let input_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "minLength": 1 },
            "filter": {
                "type": "object",
                "properties": {
                    "state": { "enum": ["open", "closed"], "description": "Issue state." },
                    "labels": { "type": "array", "items": { "type": "string" } }
                }
            },
            "sort": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "field": { "type": "string", "maxLength": 32 } },
                    "required": ["field"]
                }
            }
        },
        "required": ["query"]
    });
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
        lash_core::ToolDefinition::raw(
            "tool:mcp/issues_search",
            "issues_search",
            "Search issues.",
            input_schema.clone(),
            serde_json::json!({"type":"string"}),
        )
        .expect("valid declared tool schemas"),
    ]);
    let preamble = StandardProtocolDriver {
        config: StandardProtocolConfig {
            batch: BatchSugar::Disabled,
            ..StandardProtocolConfig::default()
        },
    }
    .build_preamble(ProtocolBuildInput {
        tool_catalog: Arc::new(catalog.clone()),
        plugin_extensions: Default::default(),
        trigger_events: Default::default(),
        writer_formats: lash_core::build_newest_writer_formats(),
    });
    assert_eq!(preamble.tool_specs.len(), 1);
    assert_eq!(
        preamble.tool_specs[0].input_schema.canonical(),
        &input_schema
    );

    let entry = &catalog.tools[0];
    assert_eq!(
        entry
            .contract
            .compact_contract_shared(&entry.manifest)
            .render_signature(),
        concat!(
            "issues_search({ query: str min length 1, ",
            "filter?: record{labels?: list[str], state?: enum[\"open\", \"closed\"]}, ",
            "sort?: list[record{field: str}] }) -> str\n",
            "Parameters:\n",
            "- `query: str min length 1`\n",
            "- `filter.labels?: list[str]`\n",
            "- `filter.state?: enum[\"open\", \"closed\"]` — Issue state.\n",
            "- `sort[].field: str max length 32`"
        )
    );
}

#[test]
fn standard_discovery_filters_provider_specs_and_requires_an_inline_member() {
    let tool = |name: &str, inline| {
        let mut tool = lash_core::ToolDefinition::raw(
            name,
            name,
            name,
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"string"}),
        )
        .expect("valid declared tool schemas");
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
        // The catalog's inline (or every) member, plus the `batch` sugar.
        let expected = if discovery.is_some() { 2 } else { 3 };
        let driver = StandardProtocolDriver {
            config: StandardProtocolConfig {
                discovery,
                ..StandardProtocolConfig::default()
            },
        };
        let preamble = driver.build_preamble(ProtocolBuildInput {
            tool_catalog: Arc::new(catalog.clone()),
            plugin_extensions: Default::default(),
            trigger_events: Default::default(),
            writer_formats: lash_core::build_newest_writer_formats(),
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
