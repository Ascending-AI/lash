// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_execution_contracts::plugin_error::PluginError;

/// FIG-4544. A standard session shows a tool's schema in two places: the
/// provider spec, which is the canonical schema itself, and the docs its tool
/// presenter writes for a host to hand back from discovery. An MCP-style
/// schema — open, nested, one field undescribed — reaches both whole.
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
            "tools.issues_search",
            "Search issues.",
            input_schema.clone(),
            serde_json::json!({"type":"string"}),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120)),
    ]);
    let preamble = StandardProtocolDriver {
        config: StandardProtocolConfig {
            batch: BatchSugar::Disabled,
            ..StandardProtocolConfig::default()
        },
    }
    .build_preamble(ProtocolBuildInput {
        packages: std::sync::Arc::new(lash_kernel_dialect::NoPackages),
        tool_catalog: Arc::new(catalog.clone()),
        plugin_extensions: Default::default(),
        writer_formats: lash_core::build_newest_writer_formats(),
    });
    assert_eq!(preamble.tool_specs.len(), 1);
    assert_eq!(
        preamble.tool_specs[0].input_schema.canonical(),
        &input_schema
    );

    let docs = StandardProtocolConfig::default()
        .tool_docs(&catalog, &lash_kernel_dialect::NoPackages)
        .expect("the standard presenter documents the catalog");
    assert_eq!(
        docs.text,
        concat!(
            "### tools__issues_search({ query: str, ",
            "filter?: record{labels?: list[str], state?: enum[\"open\", \"closed\"]}, ",
            "sort?: list[record{field: str}] }) -> str\n",
            "Search issues.\n",
            "Parameters:\n",
            "- `input.query` (min length 1)\n",
            "- `input.filter.state` — Issue state.\n",
            "- `input.sort[].field` (max length 32)"
        )
    );
}

/// FIG-6494: documentation cannot transform either provider-native schema.
#[test]
fn provider_native_schemas_are_byte_identical_to_the_canonical_contract() {
    let schema = serde_json::json!({
        "type":"object", "properties":{"filter":{"$ref":"#/$defs/Filter"}},
        "$defs":{"Filter":{"type":"object", "properties":{
            "and":{"type":"array", "items":{"$ref":"#/$defs/Filter"}},
            "value":{"type":["string","null"], "description":"A nullable value"}
        }, "additionalProperties":false}}
    });
    let tool =
        lash_core::ToolDefinition::raw("query", "tools.query", "Query", schema.clone(), schema)
            .expect("valid recursive schemas")
            .with_execution(std::time::Duration::from_secs(30));
    let canonical_input = serde_json::to_vec(tool.contract.input_schema.canonical()).unwrap();
    let canonical_output = serde_json::to_vec(tool.contract.output_schema.canonical()).unwrap();
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
    let config = StandardProtocolConfig {
        batch: BatchSugar::Disabled,
        ..Default::default()
    };
    let docs = config
        .tool_docs(&catalog, &lash_kernel_dialect::NoPackages)
        .expect("finite graph documentation");
    assert_eq!(
        docs.text.matches("type Filter =").count(),
        1,
        "{}",
        docs.text
    );
    assert!(docs.text.contains("and?: list[Filter]"), "{}", docs.text);
    let preamble = StandardProtocolDriver { config }.build_preamble(ProtocolBuildInput {
        packages: std::sync::Arc::new(lash_kernel_dialect::NoPackages),
        tool_catalog: Arc::new(catalog),
        plugin_extensions: Default::default(),
        writer_formats: lash_core::build_newest_writer_formats(),
    });
    assert_eq!(preamble.tool_specs.len(), 1);
    assert_eq!(
        serde_json::to_vec(preamble.tool_specs[0].input_schema.canonical()).unwrap(),
        canonical_input
    );
    assert_eq!(
        serde_json::to_vec(preamble.tool_specs[0].output_schema.canonical()).unwrap(),
        canonical_output
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120));
        tool.manifest.inline = inline;
        tool
    };
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("tools.search", true),
        tool("tools.hidden", false),
    ]);
    let manifests = catalog
        .entries()
        .iter()
        .map(|entry| entry.manifest.clone())
        .collect::<Vec<_>>();
    for discovery in [
        None,
        Some(lash_core::ToolDiscovery {
            operation: "tools.search".into(),
        }),
    ] {
        tool_presentation::validate_discovery(&manifests, discovery.as_ref()).unwrap();
        // The catalog's inline (or every) member, plus the `batch` sugar.
        let expected = if discovery.is_some() { 2 } else { 3 };
        let driver = StandardProtocolDriver {
            config: StandardProtocolConfig {
                discovery,
                ..StandardProtocolConfig::default()
            },
        };
        let preamble = driver.build_preamble(ProtocolBuildInput {
            packages: std::sync::Arc::new(lash_kernel_dialect::NoPackages),
            tool_catalog: Arc::new(catalog.clone()),
            plugin_extensions: Default::default(),
            writer_formats: lash_core::build_newest_writer_formats(),
        });
        assert_eq!(preamble.tool_specs.len(), expected);
    }
    for operation in ["tools.hidden", "tools.absent"] {
        assert!(matches!(
            tool_presentation::validate_discovery(
                &manifests,
                Some(&lash_core::ToolDiscovery {
                    operation: operation.into()
                })
            ),
            Err(PluginError::InvalidToolDiscovery { .. })
        ));
    }
}

/// FIG-6517: a presenter refusal keeps its identity and stable cause through
/// the plugin boundary and the durable command-failure projection.
#[test]
fn a_host_presentation_refusal_keeps_its_presenter_and_code_when_recorded() {
    let config = StandardProtocolConfig::default();
    let refusal = PresentationRefusal::Custom {
        code: "host.policy.denied".to_owned(),
        message: "This host does not disclose these tools.".to_owned(),
    };
    let code = "host.policy.denied";
    let error = tool_presentation_refused(&config, &refusal);
    let encoded = serde_json::to_value(&error).expect("serialize the plugin refusal");
    assert_eq!(encoded["type"], "tool_presentation_refused");
    assert_eq!(encoded["message"]["presenter"], "lash.tools.compact.v1");
    assert_eq!(encoded["message"]["code"], code);
    assert_eq!(encoded["message"]["message"], refusal.to_string());
    let replayed: PluginError =
        serde_json::from_value(encoded.clone()).expect("decode plugin refusal");
    assert_eq!(serde_json::to_value(&replayed).unwrap(), encoded);
    let durable = lash_execution_contracts::plugin_error::ToolIntentCommandFailure::from(&replayed);
    let durable: lash_execution_contracts::plugin_error::ToolIntentCommandFailure =
        serde_json::from_slice(&serde_json::to_vec(&durable).unwrap())
            .expect("decode durable cause");
    assert_eq!(serde_json::to_value(durable).unwrap(), encoded);
}

/// OP-INTERNAL: the Standard catalog excludes internal operations before
/// presentation and dispatch, independently of discovery and inline flags.
#[test]
fn internal_operations_never_enter_the_standard_catalog() {
    use lash_kernel_dialect::{
        Export, ExportView, OperationExport, PackagePath, Segment, Visibility,
    };
    struct View(std::collections::BTreeMap<Segment, Export>);
    impl ExportView for View {
        fn exports(
            &self,
            path: &PackagePath,
        ) -> Option<&std::collections::BTreeMap<Segment, Export>> {
            (path.to_string() == "dataframe").then_some(&self.0)
        }
        fn paths(&self) -> Vec<PackagePath> {
            vec![PackagePath::parse("dataframe").unwrap()]
        }
    }
    let make = |path| {
        lash_core::ToolDefinition::raw(
            path,
            path,
            path,
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"string"}),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(1))
    };
    let private = make("dataframe.run");
    let public = make("tools.search");
    let requirement = serde_json::from_value(serde_json::json!({
        "signature": lash_protocol_types::tool_signature(&private.contract.input_schema.canonical, &private.contract.output_schema.canonical),
        "contract": private.contract_id(),
    })).unwrap();
    let view = Arc::new(View(std::collections::BTreeMap::from([(
        Segment::new("run").unwrap(),
        Export::Operation(OperationExport {
            requirement,
            intents: Default::default(),
            visibility: Visibility::Internal,
        }),
    )])));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![private, public]);
    for discovery in [false, true] {
        let mut config = StandardProtocolConfig::default().batch(BatchSugar::Disabled);
        config.discovery = discovery.then(|| lash_core::ToolDiscovery {
            operation: "tools.search".into(),
        });
        let driver = StandardProtocolDriver {
            config: config.clone(),
        };
        let callable = driver.model_catalog(&catalog, view.as_ref());
        assert!(callable.entry_named("dataframe__run").is_none());
        assert_eq!(callable.entries().len(), 1);
        let preamble = driver.build_preamble(ProtocolBuildInput {
            packages: view.clone(),
            tool_catalog: Arc::new(catalog.clone()),
            plugin_extensions: Default::default(),
            writer_formats: lash_core::build_newest_writer_formats(),
        });
        assert!(
            preamble
                .tool_specs
                .iter()
                .all(|tool| tool.name != "dataframe__run")
        );
        let docs = config.tool_docs(&catalog, view.as_ref()).unwrap();
        assert!(!docs.text.contains("dataframe"));
        let prompt = driver.prompt_tools(Arc::new(catalog.clone()), view.as_ref());
        assert!(prompt.catalog().entry_named("dataframe__run").is_none());
    }
}
