use lash_core::plugin::{PluginError, ToolCatalogContext};
use lash_core::{ToolCatalog, facade_support::ToolCatalogContribution};
use lash_lashlang_runtime::required_tool_typescript_executable;

use crate::dialect::TypescriptDialect;

/// RLM catalog assembly. The catalog is a flat callable set: every member is
/// rendered as a full prompt doc under its call-path. RLM contributes
/// no removals; it validates that each member carries an explicit
/// `typescript.tool` binding so a cell can call it by module path.
pub(crate) fn rlm_tool_catalog(
    ctx: ToolCatalogContext,
    dialect: &TypescriptDialect,
) -> Result<ToolCatalogContribution, PluginError> {
    let _build_tool_catalog = lash_core::facade_support::build_tool_catalog;
    validate_rlm_language_bindings(&ctx.tools, dialect.language())?;
    Ok(ToolCatalogContribution::default())
}

/// Being a member *is* being presented.
///
/// Registration already requires the binding on every tool, so the dialect's path
/// is always available.
#[expect(
    clippy::expect_used,
    reason = "catalog registration validates the TypeScript tool binding, so tool_call_path only errs on an unregistered manifest"
)]
pub(crate) fn rlm_prompt_tool_docs(
    tool_catalog: &ToolCatalog,
    dialect: &crate::dialect::TypescriptDialect,
    features: crate::protocol::RlmPromptFeatures,
) -> String {
    let entries = tool_catalog
        .tools
        .iter()
        .filter(|tool| features.decomposition || tool.manifest.name != "continue_as")
        .map(|tool| {
            let contract = &tool.contract;
            let call_path = dialect
                .tool_call_path(&tool.manifest)
                .expect("RLM tool catalog registration validates the TypeScript binding");
            let mut compact =
                contract.compact_contract_with_signature_name(&tool.manifest, &call_path);
            // Authored examples are Lashlang source; the dialect spells them.
            compact.examples = compact
                .examples
                .iter()
                .map(|example| dialect.render_tool_example(example))
                .collect();
            compact.parameters.retain(has_field_description);
            if !schema_nests(contract.output_schema.canonical(), 0) {
                compact.return_fields.retain(has_field_description);
            }
            let markdown = compact.render_markdown();
            let (_, notes) = markdown.split_once('\n').unwrap_or((&markdown, ""));
            let signature = dialect.language().tool_signature(
                &call_path,
                contract.input_schema.canonical(),
                contract.output_schema.canonical(),
            );
            format!("`{signature}`\n{notes}")
        })
        .collect::<Vec<_>>();
    entries.join("\n\n")
}

fn has_field_description(row: &serde_json::Value) -> bool {
    row.get("description")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|description| !description.trim().is_empty())
}

fn schema_nests(schema: &serde_json::Value, depth: usize) -> bool {
    let container = schema.get("properties").is_some() || schema.get("items").is_some();
    if container && depth >= 1 {
        return true;
    }
    if schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|properties| {
            properties
                .values()
                .any(|field| schema_nests(field, depth + 1))
        })
    {
        return true;
    }
    if schema
        .get("items")
        .is_some_and(|items| schema_nests(items, depth + 1))
    {
        return true;
    }
    ["anyOf", "oneOf", "allOf"].iter().any(|key| {
        schema
            .get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|variants| variants.iter().any(|variant| schema_nests(variant, depth)))
    })
}

fn validate_rlm_language_bindings(
    tools: &[lash_core::ToolManifest],
    language: &dyn crate::dialect::Dialect,
) -> Result<(), PluginError> {
    for tool in tools {
        let typescript = required_tool_typescript_executable(tool)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        // Being a catalog member is being advertised, and the TypeScript
        // execution section advertises the binding's call path as a typed
        // declaration the model calls verbatim. A path the dialect resolves to
        // anything but a tool call — a module segment no cell can write, an ECMA
        // global namespace, a refused method name — can only be advertised as a
        // callable nothing, so it is refused here instead (FIG-1444).
        let call_path = typescript.call_path();
        language.ensure_tool_call_path_addressable(&call_path).map_err(|err| {
            PluginError::Registration(format!(
                "tool `{}` has a `typescript.tool` binding no TypeScript cell can call as `{call_path}`: {err}",
                tool.name
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::typescript_test_dialect;
    use lash_core::{
        ToolContract, ToolDefinition, facade_support::build_tool_catalog,
        test_support::ToolCatalogBuildInput,
    };
    use lash_lashlang_runtime::{LashlangSurface, ToolBinding, ToolDefinitionBindingExt};
    use lash_sansio::SessionId;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn rlm_catalog_renders_all_members_under_call_path() {
        let tools = [
            ToolDefinition::raw(
                "tool:test/fetch_url",
                "fetch_url",
                "Fetch URL",
                ToolContract::default_input_schema(),
                json!({ "type": "string" }),
            )
            .with_tool_binding(ToolBinding::new(["web"], "fetch")),
            ToolDefinition::raw(
                "tool:test/read_file",
                "read_file",
                "Read a file",
                ToolContract::default_input_schema(),
                json!({ "type": "string" }),
            )
            .with_tool_binding(ToolBinding::new(["files"], "read")),
        ];
        let contracts: std::collections::BTreeMap<_, _> = tools
            .iter()
            .map(|tool| (tool.manifest.id.clone(), Arc::new(tool.contract())))
            .collect();
        let manifests = tools.iter().map(|tool| tool.manifest()).collect::<Vec<_>>();
        let contribution = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: SessionId::from("session"),
                tools: manifests.clone(),
                resolve_contract: Some(Arc::new({
                    let contracts = contracts.clone();
                    move |manifest| contracts.get(&manifest.id).cloned()
                })),
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .unwrap();
        assert!(contribution.is_empty(), "RLM contributes no removals");
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |manifest| {
                contracts.get(&manifest.id).cloned()
            })),
            contributions: vec![contribution],
        })
        .expect("complete resident definitions");

        assert!(catalog.has_callable_tool("fetch_url"));
        assert!(catalog.has_callable_tool("read_file"));
        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert!(docs.contains("web.fetch"), "{docs}");
        assert!(docs.contains("files.read"), "{docs}");
        // No legacy catalogue notes or tier filtering.
        assert!(!docs.contains("Catalogued capabilities:"), "{docs}");
    }

    #[test]
    fn native_rlm_and_validation_share_one_pinned_definition_under_registry_drift() {
        let definition = ToolDefinition::raw(
            "tool:test/pinned",
            "pinned_tool",
            "Pinned schema authority",
            json!({
                "type": "object",
                "properties": { "pinned": { "type": "string" } },
                "required": ["pinned"],
                "additionalProperties": false
            }),
            json!({ "type": "string" }),
        )
        .with_tool_binding(ToolBinding::new(["authority"], "pinned"));
        let drifted = ToolDefinition::raw(
            "tool:test/pinned",
            "pinned_tool",
            "Drifted provider schema",
            json!({
                "type": "object",
                "properties": { "drifted": { "type": "integer" } },
                "required": ["drifted"],
                "additionalProperties": false
            }),
            json!({ "type": "integer" }),
        )
        .with_tool_binding(ToolBinding::new(["authority"], "pinned"));
        let resolutions = Arc::new(AtomicUsize::new(0));
        let resolution_count = Arc::clone(&resolutions);
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![definition.manifest()],
            resolve_contract: Some(Arc::new(move |_| {
                let generation = resolution_count.fetch_add(1, Ordering::SeqCst);
                Some(Arc::new(if generation == 0 {
                    definition.contract()
                } else {
                    drifted.contract()
                }))
            })),
            contributions: Vec::new(),
        })
        .expect("first resident definition is pinned");

        let native = catalog.model_tool_specs();
        assert!(
            native[0]
                .input_schema
                .canonical()
                .pointer("/properties/pinned")
                .is_some()
        );
        assert!(
            native[0]
                .input_schema
                .canonical()
                .pointer("/properties/drifted")
                .is_none()
        );
        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert!(docs.contains("pinned"), "{docs}");
        let resources = lash_lashlang_runtime::lashlang_resources_from_tool_catalog(&catalog)
            .expect("pinned contract imports into RLM bindings");
        let operation = resources
            .resolve_operation("Authority", "pinned")
            .expect("resident operation");
        assert!(
            matches!(operation.input_ty, lashlang::TypeExpr::Object(ref fields)
            if fields.iter().any(|field| field.name == "pinned")
                && fields.iter().all(|field| field.name != "drifted"))
        );
        assert!(
            lash_sansio::validate_tool_input(
                &catalog.tools[0].contract,
                &json!({ "pinned": "yes" })
            )
            .is_ok()
        );
        assert!(
            lash_sansio::validate_tool_input(&catalog.tools[0].contract, &json!({ "drifted": 1 }))
                .is_err()
        );
        assert_eq!(
            resolutions.load(Ordering::SeqCst),
            1,
            "every projection and validation reuses the captured contract"
        );
    }

    #[test]
    fn rlm_catalog_rejects_members_without_tool_binding() {
        let missing = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        );

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: SessionId::from("session"),
                tools: vec![missing.manifest()],
                resolve_contract: None,
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect_err("missing binding should fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `typescript.tool` binding"),
            "{err}"
        );
    }

    /// Membership is advertisement, so a binding whose call path a TypeScript
    /// cell cannot write is refused at registration rather than rendered as a
    /// declaration nothing can call (FIG-1444). `delete` is a module root no
    /// cell can spell; `Math` is a root the lowerer resolves as an ECMA global
    /// namespace.
    #[test]
    fn rlm_catalog_rejects_typescript_call_paths_no_cell_can_address() {
        for module in ["delete", "Math"] {
            let unaddressable = ToolDefinition::raw(
                "tool:test/purge",
                "purge",
                "Purge",
                ToolContract::default_input_schema(),
                json!({ "type": "string" }),
            )
            .with_tool_binding(ToolBinding::new([module], "run"));

            let err = rlm_tool_catalog(
                ToolCatalogContext {
                    session_id: SessionId::from("session"),
                    tools: vec![unaddressable.manifest()],
                    resolve_contract: None,
                    tool_access: lash_core::SessionToolAccess::default(),
                    subagent: None,
                    extensions: Default::default(),
                },
                &typescript_test_dialect(),
            )
            .expect_err("an unaddressable TypeScript call path must fail registration");

            assert!(
                err.to_string().contains("no TypeScript cell can call"),
                "{err}"
            );
            assert!(err.to_string().contains(&format!("{module}.run")), "{err}");
            // The refusal must lead with why the path is unadvertisable. The
            // probe's own diagnostic answers a different question — `Math.*`
            // fails it as `TS_AWAIT_UNSUPPORTED`, which reads as "drop the
            // await" — so it belongs after the reason, never in place of it.
            assert!(
                err.to_string().contains("does not dispatch a tool"),
                "{err}"
            );
        }
    }

    /// The retired `lashlang.tool` key is not a reader alias: a manifest that
    /// carries only it has no binding at all, and registration must say so
    /// rather than silently accepting the dead key (FIG-3273).
    #[test]
    fn rlm_catalog_rejects_members_with_only_retired_binding_key() {
        let mut retired_only = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        )
        .with_tool_binding(ToolBinding::new(["plan"], "update"));
        let binding = retired_only
            .manifest
            .bindings
            .remove(lash_lashlang_runtime::TYPESCRIPT_TOOL_BINDING_KEY)
            .expect("with_tool_binding wrote the canonical key");
        retired_only
            .manifest
            .bindings
            .insert("lashlang.tool".to_string(), binding);

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: SessionId::from("session"),
                tools: vec![retired_only.manifest()],
                resolve_contract: None,
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect_err("a manifest carrying only `lashlang.tool` must fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `typescript.tool` binding"),
            "{err}"
        );
    }

    #[test]
    fn member_rlm_tool_docs_render_and_link_module_call() {
        let update_plan = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update the visible plan",
            json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string" },
                                "status": { "type": "string" }
                            },
                            "required": ["step", "status"]
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
            json!({ "type": "string" }),
        )
        .with_tool_binding(ToolBinding::new(["plan"], "update"));

        let contracts: std::collections::BTreeMap<_, _> = [update_plan.clone()]
            .iter()
            .map(|tool| (tool.manifest.id.clone(), Arc::new(tool.contract())))
            .collect();
        let manifests = vec![update_plan.manifest()];
        let contribution = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: SessionId::from("session"),
                tools: manifests.clone(),
                resolve_contract: Some(Arc::new({
                    let contracts = contracts.clone();
                    move |manifest| contracts.get(&manifest.id).cloned()
                })),
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect("RLM catalog validates explicit binding");
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |manifest| {
                contracts.get(&manifest.id).cloned()
            })),
            contributions: vec![contribution],
        })
        .expect("complete resident definitions");

        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert!(docs.len() <= 768, "plan.update docs exceeded budget");
        assert!(docs.contains("plan.update("), "{docs}");
        assert!(
            docs.contains("plan: Array<Record<string, unknown>>"),
            "{docs}"
        );
        assert!(!docs.contains("update_plan("), "{docs}");

        let host_environment = LashlangSurface::default()
            .host_environment(&catalog)
            .expect("explicit binding builds host environment");
        let program = lash_typescript::parse(
            r#"await plan.update({ plan: [{ step: "Patch", status: "pending" }] });"#,
        )
        .expect("module call lowers");
        lashlang::LinkedModule::link(program, host_environment).expect("module call links");
    }
    fn tool_with_prose(description: &str, schema_description: &str) -> ToolDefinition {
        ToolDefinition::raw(
            "tool:test/spawn_agent",
            "spawn_agent",
            description,
            json!({
                "type": "object",
                "properties": {
                    "output": { "type": "object", "description": schema_description }
                },
                "additionalProperties": false
            }),
            json!({ "type": "string" }),
        )
        .with_tool_binding(ToolBinding::new(["agents"], "spawn"))
    }

    #[test]
    fn typescript_named_tool_prose_registers() {
        let tool = tool_with_prose(
            "Run a TypeScript subagent in a <typescript> cell.",
            "A TypeScript process definition value, for example `on_button`.",
        );
        let contract = Arc::new(tool.contract());
        let name = tool.name().to_string();
        rlm_tool_catalog(
            ToolCatalogContext {
                session_id: SessionId::from("session"),
                tools: vec![tool.manifest()],
                resolve_contract: Some(Arc::new(move |requested| {
                    (requested.name == name).then(|| Arc::clone(&contract))
                })),
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect("TypeScript prose must register");
    }

    #[test]
    fn typescript_tool_prose_is_rendered_verbatim() {
        let description = "Run a TypeScript subagent in a <typescript> cell.";
        let schema_description = "A TypeScript process definition value.";
        let tool = tool_with_prose(description, schema_description);
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![tool.manifest()],
            resolve_contract: Some(Arc::new({
                let contract = Arc::new(tool.contract());
                move |_| Some(Arc::clone(&contract))
            })),
            contributions: vec![ToolCatalogContribution::default()],
        })
        .expect("complete resident definition");
        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert!(docs.contains(description), "{docs}");
        assert!(docs.contains(schema_description), "{docs}");
    }
}

pub(crate) fn validate_discovery(
    tools: &[lash_core::ToolManifest],
    discovery: Option<&lash_core::ToolDiscovery>,
    dialect: &crate::dialect::TypescriptDialect,
) -> Result<(), PluginError> {
    if let Some(discovery) = discovery
        && !tools.iter().any(|tool| {
            tool.inline
                && dialect.tool_call_path(tool).ok().as_deref()
                    == Some(discovery.operation.as_str())
        })
    {
        return Err(PluginError::InvalidToolDiscovery {
            operation: discovery.operation.clone(),
        });
    }
    Ok(())
}

pub(crate) fn with_discovery_sentence(
    mut execution: String,
    discovery: Option<&lash_core::ToolDiscovery>,
    dialect: &crate::dialect::TypescriptDialect,
) -> String {
    if let Some(discovery) = discovery {
        let suffix = if dialect.language_id() == "lashlang" {
            "?"
        } else {
            ""
        };
        let sentence = format!(
            " Other tools exist; find them with `await {}({{ ... }}){suffix}`.",
            discovery.operation
        );
        let at = execution.find("\n\n").unwrap_or(execution.len());
        execution.insert_str(at, &sentence);
    }
    execution
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
    #[test]
    fn discovery_filters_each_dialect_and_channel_and_requires_an_inline_operation() {
        let tool = |name: &str, inline| {
            let mut tool = lash_core::ToolDefinition::raw(
                name,
                name,
                format!("Description for {name}"),
                serde_json::json!({"type":"object"}),
                serde_json::json!({"type":"string"}),
            )
            .with_tool_binding(ToolBinding::new(["tools"], name));
            tool.manifest.inline = inline;
            tool
        };
        let catalog = ToolCatalog::from_tool_definitions(vec![
            tool("search", true),
            tool("hidden", false),
            tool("visible", true),
        ]);
        let manifests = catalog
            .tools
            .iter()
            .map(|entry| entry.manifest.clone())
            .collect::<Vec<_>>();
        {
            let dialect = crate::dialect::typescript_test_dialect();
            for native in [false, true] {
                for discovery in [
                    None,
                    Some(lash_core::ToolDiscovery {
                        operation: "tools.search".into(),
                    }),
                ] {
                    validate_discovery(&manifests, discovery.as_ref(), &dialect).unwrap();
                    let visible = if discovery.is_some() {
                        catalog.inline_tools()
                    } else {
                        catalog.clone()
                    };
                    let execution = if native {
                        crate::native::prompt::execution_section(
                            &dialect,
                            Default::default(),
                            &visible,
                        )
                    } else {
                        dialect
                            .render_execution_section(
                                Default::default(),
                                &visible,
                                crate::plugin::RlmChannel::Cell,
                            )
                            .unwrap()
                    };
                    let text = with_discovery_sentence(execution, discovery.as_ref(), &dialect);
                    let docs = rlm_prompt_tool_docs(&visible, &dialect, Default::default());
                    assert!(docs.contains("Description for search"));
                    assert!(docs.contains("Description for visible"));
                    assert_eq!(docs.contains("Description for hidden"), discovery.is_none());
                    assert_eq!(
                        text.contains("Other tools exist; find them with `await tools.search"),
                        discovery.is_some()
                    );
                    assert!(!text.contains("Use discovery if available"));
                }
            }
            for operation in ["tools.hidden", "tools.absent"] {
                assert!(matches!(
                    validate_discovery(
                        &manifests,
                        Some(&lash_core::ToolDiscovery {
                            operation: operation.into()
                        }),
                        &dialect
                    ),
                    Err(PluginError::InvalidToolDiscovery { .. })
                ));
            }
        }
    }
}
