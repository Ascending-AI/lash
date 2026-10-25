use std::sync::Arc;

use super::*;

/// Model and execution projections of one immutable environment body.
#[derive(Clone)]
pub struct ResolvedToolSurface {
    pub surface: Arc<crate::RecordedSurface>,
    pub catalog: Arc<crate::ToolCatalog>,
}

impl PluginSession {
    /// The most recently adopted round, including when restored from a recorded
    /// version. Reads never enumerate sources or change the environment.
    pub(crate) fn installed_environment(
        &self,
    ) -> Result<crate::session::ToolCatalogHandle, PluginError> {
        self.environment_projections
            .lock_recover()
            .installed
            .clone()
            .ok_or_else(|| PluginError::Session("no recorded environment has been adopted".into()))
    }

    pub fn resolved_tool_catalog(&self) -> Result<Arc<crate::ToolCatalog>, PluginError> {
        Ok(self.installed_environment()?.tool_catalog())
    }

    pub fn shared_tool_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        let (installed, empty) = {
            let projections = self.environment_projections.lock_recover();
            (
                projections.installed.clone(),
                projections.empty_catalog.clone(),
            )
        };
        Ok(installed.map(|handle| handle.catalog()).unwrap_or(empty))
    }

    pub fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        Ok(self.shared_tool_catalog()?.as_ref().clone())
    }

    /// Derive the existing model/package projections from one immutable body.
    pub fn project_environment(
        &self,
        body: &lash_core_store::environment::EnvironmentBody,
    ) -> Result<ResolvedToolSurface, PluginError> {
        let surface = body
            .recorded_surface()
            .map_err(|refusal| PluginError::ToolSurfaceRefused { refusal })?;
        let catalog = Arc::new(self.surface_catalog(&surface)?);
        Ok(ResolvedToolSurface {
            surface: Arc::new(surface),
            catalog,
        })
    }

    /// The catalog `surface`'s tools offer, validated as a resident catalog:
    /// schemas usable, contracts present and isolation engines registered.
    pub fn surface_catalog(
        &self,
        surface: &crate::RecordedSurface,
    ) -> Result<crate::ToolCatalog, PluginError> {
        for (effect, provider) in surface
            .tools()
            .iter()
            .map(|(operation, entry)| (operation.effect_name().to_string(), entry.provider()))
            .chain(
                surface
                    .deferred()
                    .map(|provider| ("deferred".to_owned(), provider)),
            )
        {
            if provider.registry() == lash_vocabulary::claims::RegistryKind::Session
                && self.host.retired_plugins.contains_key(provider.owner())
            {
                return Err(PluginError::Runtime(
                    super::super::RetirementRefusal::Serves {
                        registry: lash_vocabulary::claims::RegistryKind::Session,
                        id: provider.owner().to_owned(),
                        effect,
                    }
                    .into_runtime_error(),
                ));
            }
        }
        let definitions = surface.definitions().cloned().collect::<Vec<_>>();
        let contracts: std::collections::BTreeMap<_, _> = definitions
            .iter()
            .map(|definition| {
                (
                    definition.manifest.id.clone(),
                    Arc::new(definition.contract.clone()),
                )
            })
            .collect();
        let tools = definitions
            .iter()
            .map(|definition| definition.manifest.clone())
            .collect();
        let resolve_contract: lash_protocol_types::ToolContractResolver =
            Arc::new(move |manifest: &ToolManifest| contracts.get(&manifest.id).cloned());
        crate::build_tool_catalog(crate::ToolCatalogBuildInput {
            tools,
            resolve_contract: Some(resolve_contract),
            contributions: Vec::new(),
            isolation_engines: self.host.isolation_engines(),
        })
        .map_err(catalog_build_error)
    }
}

/// A catalog build refusal as the plugin error it is.
pub(crate) fn catalog_build_error(err: lash_vocabulary::ToolCatalogBuildError) -> PluginError {
    match err {
        source @ (lash_vocabulary::ToolCatalogBuildError::UnusableSchema { .. }
        | lash_vocabulary::ToolCatalogBuildError::MissingOutputDeclaration { .. }) => {
            PluginError::UnusableToolSchema {
                source: Box::new(source),
            }
        }
        lash_vocabulary::ToolCatalogBuildError::MissingContract { tool_id, operation } => {
            PluginError::ResidentToolContractUnavailable { tool_id, operation }
        }
        lash_vocabulary::ToolCatalogBuildError::DuplicateId { tool_id } => {
            PluginError::ResidentToolDuplicateId { tool_id }
        }
        lash_vocabulary::ToolCatalogBuildError::DuplicateOperation {
            operation,
            first_tool,
            second_tool,
        } => PluginError::ResidentToolDuplicateOperation {
            operation,
            first_tool,
            second_tool,
        },
        lash_vocabulary::ToolCatalogBuildError::InvalidOperation { tool_id, message } => {
            PluginError::ResidentToolInvalidOperation { tool_id, message }
        }
        lash_vocabulary::ToolCatalogBuildError::RegistrationRefused { refusal } => {
            PluginError::ToolRegistrationRefused {
                source: Box::new(refusal),
            }
        }
    }
}
