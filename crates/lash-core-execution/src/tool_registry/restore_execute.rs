use super::*;

impl ToolRegistry {
    /// Verify that every effective resident definition retains an executable
    /// route in this pinned registry. This inspects only the admitted surface
    /// and its captured source arcs; it never prepares or executes a call.
    pub(crate) fn validate_resident_catalog_routes(
        &self,
        catalog: &crate::ToolCatalog,
    ) -> Result<(), crate::PluginError> {
        for definition in &catalog.tools {
            let tool_id = &definition.manifest.id;
            let name = &definition.manifest.name;
            let unavailable = |reason: String| crate::PluginError::ResidentToolRouteUnavailable {
                tool_id: tool_id.clone(),
                name: name.clone(),
                reason,
            };
            let authority = self.inner.read_recover();
            let source_key = {
                let entry = authority.state.surface.get(tool_id).ok_or_else(|| {
                    unavailable("the id is absent from the pinned surface".into())
                })?;
                if !entry.is_member() {
                    return Err(unavailable(
                        "the pinned surface does not admit the id as a member".into(),
                    ));
                }
                entry.binding.source_key().cloned().ok_or_else(|| {
                    unavailable("the pinned entry is not bound to a live source".into())
                })?
            };
            if !authority.sources.contains_key(&source_key) {
                return Err(unavailable(format!(
                    "bound source `{source_key}` is absent from the pinned registry"
                )));
            }
        }
        Ok(())
    }

    pub fn resolve_catalog_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let (tool_id, source) = {
            let authority = self.inner.read_recover();
            let (tool_id, entry) = authority.state.surface.get_by_name(name)?;
            if !entry.is_member() {
                return None;
            }
            let source_key = entry.binding.source_key()?;
            let source = Arc::clone(authority.sources.get(source_key)?);
            (tool_id.clone(), source)
        };
        source.resolve_contract_by_id(&tool_id)
    }

    /// Resolve the source for a registry entry, distinguishing "unknown tool"
    /// from "known but orphaned" so callers can fail with a precise error.
    fn resolve_bound_source(
        &self,
        tool_id: &ToolId,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ToolOutcome> {
        let authority = self.inner.read_recover();
        let Some(entry) = authority.state.surface.get(tool_id) else {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Unknown tool id: {tool_id}"
            )));
        };
        if !entry.is_member() {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Tool id `{tool_id}` is unavailable"
            )));
        }
        let source_key = match &entry.binding {
            ToolBinding::Bound { source_key } => source_key,
            ToolBinding::Orphaned => {
                return Err(ToolOutcome::err_fmt(format_args!(
                    "Tool id `{tool_id}` is unavailable: it was restored from a persisted session \
                     but its source is not currently registered"
                )));
            }
        };
        authority.sources.get(source_key).cloned().ok_or_else(|| {
            ToolOutcome::err_fmt(format_args!("Tool source missing for tool id `{tool_id}`"))
        })
    }

    fn resolve_granted_execution_source(
        &self,
        tool_id: &ToolId,
        source_id: Option<&str>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ToolOutcome> {
        let Some(source_id) = source_id else {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Granted tool id `{tool_id}` is missing an explicit tool source"
            )));
        };
        let sources = {
            let authority = self.inner.read_recover();
            authority
                .granted_sources
                .as_ref()
                .unwrap_or(&authority.sources)
                .clone()
        };
        let leaf_source_key = ToolSourceKey::new(source_id);
        let source = match sources.get(&leaf_source_key) {
            Some(source) => Arc::clone(source),
            None if source_id == PLUGIN_TOOL_SOURCE_ID => {
                let mut matches = sources.values().filter(|source| {
                    source
                        .resolve_manifest_by_id(tool_id)
                        .is_some_and(|manifest| manifest.id == *tool_id)
                });
                let Some(source) = matches.next().cloned() else {
                    return Err(ToolOutcome::err_fmt(format_args!(
                        "Tool source `{source_id}` missing for granted tool id `{tool_id}`"
                    )));
                };
                if matches.next().is_some() {
                    return Err(ToolOutcome::err_fmt(format_args!(
                        "Tool source `{source_id}` is ambiguous for granted tool id `{tool_id}`"
                    )));
                }
                source
            }
            None => {
                return Err(ToolOutcome::err_fmt(format_args!(
                    "Tool source `{source_id}` missing for granted tool id `{tool_id}`"
                )));
            }
        };
        if !source
            .resolve_manifest_by_id(tool_id)
            .is_some_and(|manifest| manifest.id == *tool_id)
        {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Tool source `{source_id}` does not resolve granted tool id `{tool_id}`"
            )));
        }
        Ok(source)
    }

    fn resolve_execution_source_for_route(
        &self,
        tool_id: &ToolId,
        route: &crate::tool_provider::ToolExecutionRoute,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ToolOutcome> {
        match route {
            crate::tool_provider::ToolExecutionRoute::Catalog => self.resolve_bound_source(tool_id),
            crate::tool_provider::ToolExecutionRoute::Granted { source_id } => {
                self.resolve_granted_execution_source(tool_id, source_id.as_deref())
            }
        }
    }

    pub(crate) fn attempt_may_defer_for_grant(
        &self,
        tool_id: &ToolId,
        source_id: Option<&str>,
    ) -> bool {
        let Ok(source) = self.resolve_granted_execution_source(tool_id, source_id) else {
            return false;
        };
        source.execution().attempt_may_defer(tool_id)
    }
}

#[async_trait::async_trait]
impl ToolProvider for ToolRegistry {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        let authority = self.inner.read_recover();
        authority
            .state
            .surface
            .by_id
            .values()
            .filter(|entry| entry.is_member())
            .map(ToolRegistryEntry::view_manifest)
            .collect()
    }

    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        self.inner
            .read_recover()
            .state
            .surface
            .get_by_name(name)
            .map(|(_, entry)| entry.view_manifest())
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.inner
            .read_recover()
            .state
            .surface
            .get(id)
            .map(ToolRegistryEntry::view_manifest)
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let tool_id = self
            .inner
            .read_recover()
            .state
            .surface
            .get_by_name(name)
            .map(|(tool_id, _)| tool_id.clone())?;
        self.resolve_contract_by_id(&tool_id)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let source = {
            let authority = self.inner.read_recover();
            let entry = authority.state.surface.get(id)?;
            let source_key = entry.binding.source_key()?;
            Arc::clone(authority.sources.get(source_key)?)
        };
        source.resolve_contract_by_id(id)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        let source =
            self.resolve_execution_source_for_route(&call.tool_id, call.context.execution_route())?;
        source.prepare_tool_call(call).await
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let source = match self
            .resolve_execution_source_for_route(call.tool_id(), call.context.execution_route())
        {
            Ok(source) => source,
            Err(result) => return result.into(),
        };
        source.execution().execute(call).await
    }

    fn attempt_may_defer(&self, tool_id: &ToolId) -> bool {
        self.resolve_bound_source(tool_id)
            .is_ok_and(|source| source.execution().attempt_may_defer(tool_id))
    }
}
