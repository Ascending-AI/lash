use std::collections::BTreeSet;
use std::sync::Arc;

use super::*;

/// A surface built from what a run's providers advertise under its config
/// (S4): the surface to record, and the catalog its model calls offer.
#[derive(Clone)]
pub struct ResolvedToolSurface {
    pub surface: Arc<crate::RecordedSurface>,
    pub catalog: Arc<crate::ToolCatalog>,
}

impl PluginSession {
    /// The surface this plugin session's providers offer under its admitted
    /// configuration.
    pub fn pin_resolved_tool_surface(&self) -> Result<ResolvedToolSurface, PluginError> {
        self.build_tool_surface(&self.admitted_plugin_config())
    }

    pub fn resolved_tool_catalog(&self) -> Result<Arc<crate::ToolCatalog>, PluginError> {
        Ok(self.pin_resolved_tool_surface()?.catalog)
    }

    /// Project every Tool Catalog member to a JSON record for host-owned
    /// discovery (e.g. the production `tools.search` path in agent-workbench).
    pub fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        let catalog = self.resolved_tool_catalog()?;
        Ok(crate::tool_registry::project_tool_catalog(
            catalog.entries().iter().cloned(),
        ))
    }

    /// Build the surface in one place (S4): every provider advertises under
    /// `config`, the catalog contributors remove candidates, two tools under
    /// one model-facing name are refused naming both tools and providers,
    /// and what remains is the surface a run records, with the session's
    /// deferred provider.
    ///
    /// # Errors
    ///
    /// A provider's advertisement, a contributor or the catalog build
    /// refused, or the surface collides ([`PluginError::ToolSurfaceRefused`]).
    pub fn build_tool_surface(
        &self,
        config: &super::AdmittedPluginConfig,
    ) -> Result<ResolvedToolSurface, PluginError> {
        let view = super::OwnerConfigView::session(config.clone());
        let advertised = self.capabilities().tool_registry.advertise(&view)?;
        let mut contributions = collect_owned_sync(
            self.composition().tool_catalog_contributors(),
            ToolCatalogContext {
                owner: self.owner.clone(),
                tools: advertised
                    .iter()
                    .map(|entry| entry.definition.manifest.clone())
                    .collect(),
                config: view,
                extensions: self.extensions.clone(),
            },
            |hook, ctx| hook(ctx),
        )?
        .into_iter()
        .map(|owned| owned.value)
        .collect::<Vec<_>>();
        contributions.push(self.tool_catalog_overlay.clone());
        let removed = contributions
            .into_iter()
            .flat_map(|contribution| contribution.remove)
            .collect::<BTreeSet<_>>();
        let entries = advertised
            .into_iter()
            .filter(|entry| !removed.contains(&entry.definition.manifest.operation))
            .collect::<Vec<_>>();
        let surface = crate::RecordedSurface::new(entries, self.composition().deferred_key())
            .map_err(|refusal| PluginError::ToolSurfaceRefused {
                refusal: Box::new(refusal),
            })?;
        let surface = surface.with_packages(self.advertised_packages(config)?);
        let catalog = self.surface_catalog(&surface)?;
        Ok(ResolvedToolSurface {
            surface: Arc::new(surface),
            catalog: Arc::new(catalog),
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
        let tools = definitions
            .iter()
            .map(|definition| definition.manifest.clone())
            .collect();
        let resolve_contract: lash_protocol_types::ToolContractResolver =
            Arc::new(move |manifest: &ToolManifest| {
                definitions
                    .iter()
                    .find(|definition| definition.manifest.id == manifest.id)
                    .map(|definition| Arc::new(definition.contract.clone()))
            });
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
        source @ lash_vocabulary::ToolCatalogBuildError::UnusableSchema { .. } => {
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
