use lash_core::plugin::{PluginError, ToolCatalogContext};
use lash_core::{ToolCatalog, facade_support::ToolCatalogContribution};
use lash_lashlang_runtime::required_tool_executable;

use crate::dialect::SessionDialect;

/// RLM catalog assembly. The catalog is a flat callable set: every member is
/// rendered as a full prompt doc under its call-path. RLM contributes
/// no removals; it validates that each member carries an explicit
/// `lash.tool` binding the session's dialect can call by module path.
pub(crate) fn rlm_tool_catalog(
    ctx: ToolCatalogContext,
    dialect: &SessionDialect,
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
    reason = "catalog registration validates every tool binding against the session's dialect, so tool_call_path only errs on an unregistered manifest"
)]
pub(crate) fn rlm_prompt_tool_docs(
    tool_catalog: &ToolCatalog,
    dialect: &crate::dialect::SessionDialect,
    features: crate::protocol::RlmPromptFeatures,
) -> String {
    let mut entries = Vec::new();
    let mut modules =
        std::collections::BTreeMap::<&str, (&lash_core::ToolModule, Vec<String>)>::new();
    for tool in tool_catalog
        .tools
        .iter()
        .filter(|tool| features.decomposition || tool.manifest.name != "continue_as")
    {
        let contract = &tool.contract;
        let call_path = dialect
            .tool_call_path(&tool.manifest)
            .expect("RLM tool catalog registration validates the session dialect's binding");
        let input = contract.input_shape();
        let output = contract.output_shape();
        let signature = dialect
            .language()
            .tool_signature(&call_path, &input, &output);
        // The signature carries every field's name and type. The rows under
        // it add what a type cannot say, for the fields that say it.
        let mut sections = vec![format!("`{signature}`")];
        let description = tool.manifest.description.trim();
        if !description.is_empty() {
            sections.push(description.to_string());
        }
        for (title, shape) in [("Parameters", &input), ("Return fields", &output)] {
            let rows = dialect.noted_field_rows(shape);
            if !rows.is_empty() {
                sections.push(format!("{title}:\n{}", rows.join("\n")));
            }
        }
        // Authored examples are Lashlang source; the dialect spells them,
        // and leaves out the ones it cannot.
        let examples = contract
            .compact_examples()
            .iter()
            .filter_map(|example| dialect.render_tool_example(example))
            .collect::<Vec<_>>();
        if !examples.is_empty() {
            sections.push(format!("Examples: {}", examples.join("; ")));
        }
        let doc = sections.join("\n");
        if let Some(module) = tool.manifest.module.as_deref() {
            modules
                .entry(module.name.as_str())
                .or_insert_with(|| (module, Vec::new()))
                .1
                .push(doc);
        } else {
            entries.push(doc);
        }
    }
    entries.extend(
        modules
            .into_values()
            .map(|(module, tools)| format!("#### {}\n\n{}", module.name, tools.join("\n\n"))),
    );
    entries.join("\n\n")
}

fn validate_rlm_language_bindings(
    tools: &[lash_core::ToolManifest],
    language: &dyn crate::dialect::Dialect,
) -> Result<(), PluginError> {
    for tool in tools {
        let binding = required_tool_executable(tool)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        // Being a catalog member is being advertised under the call path the
        // dialect spells; a binding the dialect cannot address could only be
        // advertised as a callable nothing, so it is refused here instead.
        language.tool_call_path(&binding).map_err(|refusal| {
            PluginError::Registration(format!(
                "tool `{}` has a `lash.tool` binding the `{}` dialect cannot call: {refusal}",
                tool.name,
                language.language_id()
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
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
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120));

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                tools: vec![missing.manifest()],
                resolve_contract: None,
                tool_access: lash_core::SessionToolAccess::default(),
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect_err("missing binding should fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `lash.tool` binding"),
            "{err}"
        );
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["plan"], "update"));
        let binding = retired_only
            .manifest
            .bindings
            .remove(lash_lashlang_runtime::TOOL_BINDING_KEY)
            .expect("with_tool_binding wrote the canonical key");
        retired_only
            .manifest
            .bindings
            .insert("lashlang.tool".to_string(), binding);

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                tools: vec![retired_only.manifest()],
                resolve_contract: None,
                tool_access: lash_core::SessionToolAccess::default(),
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect_err("a manifest carrying only `lashlang.tool` must fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `lash.tool` binding"),
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["plan"], "update"));

        let contracts: std::collections::BTreeMap<_, _> = [update_plan.clone()]
            .iter()
            .map(|tool| (tool.manifest.id.clone(), Arc::new(tool.contract())))
            .collect();
        let manifests = vec![update_plan.manifest()];
        let contribution = rlm_tool_catalog(
            ToolCatalogContext {
                owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                tools: manifests.clone(),
                resolve_contract: Some(Arc::new({
                    let contracts = contracts.clone();
                    move |manifest| contracts.get(&manifest.id).cloned()
                })),
                tool_access: lash_core::SessionToolAccess::default(),
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
            docs.contains("plan: Array<{ step: string; status: string }>"),
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

    /// FIG-4544. An MCP server hands its schemas through as written, and
    /// almost none of them say `additionalProperties: false`. The tool docs
    /// must still show every field's name and type, nested ones included,
    /// whether or not the field has a description.
    #[test]
    fn an_mcp_style_open_schema_renders_every_field_name_and_type() {
        let search = ToolDefinition::raw(
            "tool:mcp/issues_search",
            "mcp__tracker__issues_search",
            "[MCP tracker] Search issues.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "filter": {
                        "type": "object",
                        "properties": {
                            "state": { "enum": ["open", "closed"] },
                            "labels": { "type": "array", "items": { "type": "string" } }
                        }
                    },
                    "sort": {
                        "type": "array",
                        "description": "Sort keys, most significant first.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "field": { "type": "string" },
                                "descending": { "type": "boolean", "default": false }
                            },
                            "required": ["field"]
                        }
                    },
                    "page": { "type": ["integer", "null"], "minimum": 1 }
                },
                "required": ["query"]
            }),
            json!({
                "type": "object",
                "properties": {
                    "issues": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "integer", "description": "Stable issue id." },
                                "title": { "type": "string" }
                            },
                            "required": ["id", "title"]
                        }
                    }
                },
                "required": ["issues"]
            }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["tracker"], "issues_search"));
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![search]);

        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert_eq!(
            docs,
            concat!(
                "`tracker.issues_search({ query: string; ",
                "filter?: { labels?: Array<string>; state?: \"open\" | \"closed\" }; ",
                "page?: number | null; ",
                "sort?: Array<{ field: string; descending?: boolean }> }): ",
                "Promise<{ issues: Array<{ id: number; title: string }> }>`\n",
                "[MCP tracker] Search issues.\n",
                "Parameters:\n",
                "- `query: string` (min length 1)\n",
                "- `page?: number | null` (>= 1)\n",
                "- `sort?: Array<Record<string, unknown>>` — Sort keys, most significant first.\n",
                "- `sort[].descending?: boolean` (default false)\n",
                "Return fields:\n",
                "- `issues[].id: number` — Stable issue id."
            )
        );
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["agents"], "spawn"))
    }

    #[test]
    fn typescript_tool_prose_is_rendered_verbatim() {
        let description = "Run a TypeScript child session in a <typescript> cell.";
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
    dialect: &crate::dialect::SessionDialect,
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
            .expect("valid declared tool schemas")
            .with_execution(std::time::Duration::from_secs(120))
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
                            discovery.as_ref(),
                        )
                        .joined()
                    } else {
                        dialect
                            .render_execution_section(
                                Default::default(),
                                &visible,
                                crate::plugin::RlmChannel::Cell,
                                discovery.as_ref(),
                            )
                            .unwrap()
                    };
                    let text = execution;
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
