use lash_core::plugin::{PluginError, ToolCatalogContext};
use lash_core::{ToolActivation, ToolCatalog, facade_support::ToolCatalogContribution};
use lash_lashlang_runtime::{
    required_tool_lashlang_executable, required_tool_typescript_executable,
};

/// RLM catalog assembly. The catalog is a flat callable set: every member is
/// rendered as a full prompt doc under its Lashlang call-path. RLM contributes
/// no removals; it only validates that each member carries an explicit
/// `lashlang.tool` and `typescript.tool` bindings so either dialect can call
/// it by module path.
pub(crate) fn rlm_tool_catalog(
    ctx: ToolCatalogContext,
) -> Result<ToolCatalogContribution, PluginError> {
    validate_rlm_language_bindings(&ctx)?;
    Ok(ToolCatalogContribution::default())
}

/// Render every catalog member as a full prompt doc under its Lashlang
/// call-path. Being a member *is* being presented.
pub(crate) fn rlm_prompt_tool_docs(tool_catalog: &ToolCatalog) -> String {
    tool_catalog
        .tools
        .iter()
        .filter(|tool| tool.manifest.activation != ToolActivation::Internal)
        .filter_map(|tool| {
            let contract = tool_catalog.resolve_contract(&tool.manifest.name)?;
            let binding = required_tool_lashlang_executable(&tool.manifest)
                .expect("RLM tool catalog registration must validate explicit Lashlang bindings");
            Some(
                contract
                    .compact_contract_with_signature_name(&tool.manifest, &binding.call_path())
                    .render_markdown(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn validate_rlm_language_bindings(ctx: &ToolCatalogContext) -> Result<(), PluginError> {
    for tool in &ctx.tools {
        if tool.activation == ToolActivation::Internal {
            continue;
        }
        required_tool_lashlang_executable(tool)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        required_tool_typescript_executable(tool)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{
        ToolContract, ToolDefinition, facade_support::ToolCatalogBuildInput,
        facade_support::build_tool_catalog,
    };
    use lash_lashlang_runtime::{LashlangSurface, LashlangToolBinding, ToolDefinitionLashlangExt};
    use serde_json::json;
    use std::sync::Arc;

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
            .with_lashlang_binding(LashlangToolBinding::new(["web"], "fetch")),
            ToolDefinition::raw(
                "tool:test/read_file",
                "read_file",
                "Read a file",
                ToolContract::default_input_schema(),
                json!({ "type": "string" }),
            )
            .with_lashlang_binding(LashlangToolBinding::new(["files"], "read")),
        ];
        let contracts: std::collections::BTreeMap<_, _> = tools
            .iter()
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect();
        let manifests = tools.iter().map(|tool| tool.manifest()).collect::<Vec<_>>();
        let contribution = rlm_tool_catalog(ToolCatalogContext {
            session_id: "session".to_string(),
            tools: manifests.clone(),
            resolve_contract: Some(Arc::new({
                let contracts = contracts.clone();
                move |name| contracts.get(name).cloned()
            })),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
        })
        .unwrap();
        assert!(contribution.is_empty(), "RLM contributes no removals");
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |name| contracts.get(name).cloned())),
            contributions: vec![contribution],
        });

        assert!(catalog.has_callable_tool("fetch_url"));
        assert!(catalog.has_callable_tool("read_file"));
        let docs = rlm_prompt_tool_docs(&catalog);
        assert!(docs.contains("web.fetch"), "{docs}");
        assert!(docs.contains("files.read"), "{docs}");
        // No legacy catalogue notes or tier filtering.
        assert!(!docs.contains("Catalogued capabilities:"), "{docs}");
    }

    #[test]
    fn rlm_catalog_rejects_members_without_lashlang_binding() {
        let missing = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        );

        let err = rlm_tool_catalog(ToolCatalogContext {
            session_id: "session".to_string(),
            tools: vec![missing.manifest()],
            resolve_contract: None,
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
        })
        .expect_err("missing binding should fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `lashlang.tool` binding"),
            "{err}"
        );
    }

    #[test]
    fn rlm_catalog_ignores_internal_members_without_lashlang_bindings() {
        let internal = ToolDefinition::raw(
            "tool:test/internal_runner",
            "internal_runner",
            "Runtime-owned process body",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        )
        .with_activation(ToolActivation::Internal);

        rlm_tool_catalog(ToolCatalogContext {
            session_id: "session".to_string(),
            tools: vec![internal.manifest()],
            resolve_contract: None,
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
        })
        .expect("internal process bodies are not Lashlang-callable catalog members");
    }

    #[test]
    fn rlm_catalog_rejects_members_without_typescript_binding() {
        let mut missing = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["plan"], "update"));
        missing
            .manifest
            .bindings
            .remove(lash_lashlang_runtime::TYPESCRIPT_TOOL_BINDING_KEY);

        let err = rlm_tool_catalog(ToolCatalogContext {
            session_id: "session".to_string(),
            tools: vec![missing.manifest()],
            resolve_contract: None,
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
        })
        .expect_err("missing TypeScript binding should fail RLM registration");

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
        .with_lashlang_binding(LashlangToolBinding::new(["plan"], "update"));

        let contracts: std::collections::BTreeMap<_, _> = [update_plan.clone()]
            .iter()
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect();
        let manifests = vec![update_plan.manifest()];
        let contribution = rlm_tool_catalog(ToolCatalogContext {
            session_id: "session".to_string(),
            tools: manifests.clone(),
            resolve_contract: Some(Arc::new({
                let contracts = contracts.clone();
                move |name| contracts.get(name).cloned()
            })),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
        })
        .expect("RLM catalog validates explicit binding");
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |name| contracts.get(name).cloned())),
            contributions: vec![contribution],
        });

        let docs = rlm_prompt_tool_docs(&catalog);
        assert!(docs.len() <= 768, "plan.update docs exceeded budget");
        assert!(docs.contains("plan.update("), "{docs}");
        assert!(
            docs.contains("plan: list[record{step: str, status: str}]"),
            "{docs}"
        );
        assert!(!docs.contains("update_plan("), "{docs}");

        let host_environment = LashlangSurface::default()
            .host_environment(&catalog)
            .expect("explicit binding builds host environment");
        let program = lashlang::parse(
            r#"await plan.update({ plan: [{ step: "Patch", status: "pending" }] })?"#,
        )
        .expect("module call parses");
        lashlang::LinkedModule::link(program, host_environment).expect("module call links");
    }
}
