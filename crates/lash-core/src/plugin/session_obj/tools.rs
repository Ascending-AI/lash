use crate::SessionId;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::*;

impl PluginSession {
    pub fn resolved_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<crate::ToolCatalog>, PluginError> {
        let tools = self.tools.tool_manifests();
        let contract_provider = Arc::clone(&self.tools);
        let resolve_contract: lash_sansio::ToolContractResolver =
            Arc::new(move |manifest: &ToolManifest| {
                contract_provider.resolve_contract_by_id(&manifest.id)
            });
        let catalog = self.resolve_tool_catalog(ToolCatalogContext {
            session_id: SessionId::from(session_id.to_string()),
            tools,
            resolve_contract: Some(Arc::clone(&resolve_contract)),
            tool_access: self.tool_access.clone(),
            subagent: self.subagent.clone(),
            extensions: self.extensions.clone(),
        })?;
        self.tool_registry
            .validate_resident_catalog_routes(&catalog)?;
        Ok(Arc::new(catalog))
    }

    /// Project every Tool Catalog member to a JSON record for host-owned
    /// discovery (e.g. the production `tools.search` path in agent-workbench).
    pub fn tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError> {
        let catalog = self.resolved_tool_catalog(session_id)?;
        Ok(crate::tool_registry::project_tool_catalog(
            catalog.tools.iter().cloned(),
        ))
    }

    pub fn resolve_tool_catalog(
        &self,
        ctx: ToolCatalogContext,
    ) -> Result<crate::ToolCatalog, PluginError> {
        let mut contributions = collect_owned_sync(
            &self.contributions.tool_catalog_contributors,
            ToolCatalogContext {
                session_id: ctx.session_id.clone(),
                tools: ctx.tools.clone(),
                resolve_contract: ctx.resolve_contract.clone(),
                tool_access: ctx.tool_access.clone(),
                subagent: ctx.subagent.clone(),
                extensions: ctx.extensions.clone(),
            },
            |hook, ctx| hook(ctx),
        )?
        .into_iter()
        .map(|owned| owned.value)
        .collect::<Vec<_>>();
        contributions.push(self.tool_catalog_overlay.clone());
        let (tools, resolve_contract) = if ctx.tool_access.tools.is_empty() {
            (ctx.tools, ctx.resolve_contract)
        } else {
            let definitions = Arc::new(ctx.tool_access.tools.clone());
            (
                definitions.iter().map(|tool| tool.manifest()).collect(),
                Some(Arc::new(move |manifest: &ToolManifest| {
                    definitions
                        .iter()
                        .find(|tool| {
                            tool.manifest.id == manifest.id && tool.manifest.name == manifest.name
                        })
                        .map(|tool| Arc::new(tool.contract()))
                }) as lash_sansio::ToolContractResolver),
            )
        };
        let authority_hidden_tools = tools
            .iter()
            .filter(|tool| ctx.tool_access.hides(&tool.name))
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>();
        if !authority_hidden_tools.is_empty() {
            contributions.push(ToolCatalogContribution {
                remove: authority_hidden_tools.into_iter().collect(),
            });
        }
        crate::build_tool_catalog(crate::ToolCatalogBuildInput {
            tools,
            resolve_contract,
            contributions,
        })
        .map_err(|err| match err {
            lash_sansio::ToolCatalogBuildError::MissingContract { tool_id, name } => {
                PluginError::ResidentToolContractUnavailable { tool_id, name }
            }
            lash_sansio::ToolCatalogBuildError::DuplicateId { tool_id } => {
                PluginError::ResidentToolDuplicateId { tool_id }
            }
            lash_sansio::ToolCatalogBuildError::DuplicateName { name } => {
                PluginError::ResidentToolDuplicateName { name }
            }
        })
    }
}
