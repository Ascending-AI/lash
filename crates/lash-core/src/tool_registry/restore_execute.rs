impl ToolRegistry {
    pub(crate) fn resolve_catalog_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest(name)?;
        let is_member = self
            .state
            .read_recover()
            .tools
            .get(&manifest.id)
            .is_some_and(ToolRegistryEntry::is_member);
        is_member
            .then(|| self.resolve_contract_by_id(&manifest.id))
            .flatten()
    }

    /// Try to rebind an orphaned entry to a source that now resolves the same
    /// tool id. Returns the live manifest on success; the advertised name may
    /// differ from the persisted manifest because names are model-facing
    /// aliases, not authority identity.
    fn try_rebind_orphan(&self, tool_id: &ToolId) -> Option<ToolManifest> {
        self.refresh_sources().ok()?;
        self.state
            .read_recover()
            .tools
            .get(tool_id)
            .filter(|entry| !entry.is_orphaned())
            .map(ToolRegistryEntry::view_manifest)
    }

    /// Resolve the source for a registry entry, distinguishing "unknown tool"
    /// from "known but orphaned" so callers can fail with a precise error.
    fn resolve_execution_source(
        &self,
        tool_id: &ToolId,
    ) -> Result<(Arc<dyn ToolSourceExecutor>, ToolManifest), ToolResult> {
        let Some(manifest) = self.resolve_manifest_by_id(tool_id) else {
            return Err(ToolResult::err_fmt(format_args!(
                "Unknown tool id: {tool_id}"
            )));
        };
        let is_member = {
            let state = self
                .state
                .read_recover();
            state
                .tools
                .get(tool_id)
                .map(ToolRegistryEntry::is_member)
                .unwrap_or(true)
        };
        if !is_member {
            return Err(ToolResult::err_fmt(format_args!(
                "Tool id `{tool_id}` is unavailable"
            )));
        }
        let binding = {
            let state = self
                .state
                .read_recover();
            state.tools.get(tool_id).map(|entry| entry.binding.clone())
        };
        let source_id = match binding {
            Some(ToolBinding::Bound(source_id)) => source_id,
            Some(ToolBinding::Orphaned) => {
                return Err(ToolResult::err_fmt(format_args!(
                    "Tool id `{tool_id}` is unavailable: it was restored from a persisted session \
                     but its source is not currently registered"
                )));
            }
            None => return Err(ToolResult::err_fmt(format_args!("Unknown tool id: {tool_id}"))),
        };
        let source = {
            self.sources
                .read_recover()
                .get(&source_id)
                .cloned()
        };
        source
            .map(|source| (source, manifest))
            .ok_or_else(|| {
                ToolResult::err_fmt(format_args!("Tool source missing for tool id `{tool_id}`"))
            })
    }

    fn resolve_granted_execution_source(
        &self,
        grant: &ToolExecutionGrant,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ToolResult> {
        let tool_id = &grant.manifest.id;
        let Some(source_id) = grant.source_id.as_deref() else {
            return Err(ToolResult::err_fmt(format_args!(
                "Granted tool id `{tool_id}` is missing an explicit tool source"
            )));
        };
        let source = self
            .sources
            .read_recover()
            .get(source_id)
            .cloned();
        let Some(source) = source else {
            return Err(ToolResult::err_fmt(format_args!(
                "Tool source `{source_id}` missing for granted tool id `{tool_id}`"
            )));
        };
        if source.resolve_manifest_by_id(tool_id).is_none() {
            return Err(ToolResult::err_fmt(format_args!(
                "Tool source `{source_id}` does not resolve granted tool id `{tool_id}`"
            )));
        }
        Ok(source)
    }
}

#[async_trait::async_trait]
impl ToolProvider for ToolRegistry {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        let state = self
            .state
            .read_recover();
        state
            .tools
            .values()
            .filter(|entry| entry.is_member())
            .map(ToolRegistryEntry::view_manifest)
            .collect()
    }

    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        let known = {
            let state = self
                .state
                .read_recover();
            state
                .tools
                .iter()
                .find(|(_, entry)| entry.manifest.name == name)
                .map(|(id, entry)| (id.clone(), entry.is_orphaned(), entry.view_manifest()))
        };
        match known {
            Some((_, false, manifest)) => return Some(manifest),
            Some((tool_id, true, off_manifest)) => {
                return Some(self.try_rebind_orphan(&tool_id).unwrap_or(off_manifest));
            }
            None => {}
        }

        let sources = self
            .sources
            .read_recover()
            .iter()
            .map(|(source_id, source)| (source_id.clone(), Arc::clone(source)))
            .collect::<Vec<_>>();
        for (source_id, source) in sources {
            let Some(manifest) = source.resolve_manifest(name) else {
                continue;
            };
            let manifest = manifest_with_compact_contract(source.as_ref(), manifest);
            let mut state = self
                .state
                .write_recover();
            if let Some(existing) = state.tools.get(&manifest.id) {
                return (existing.binding.source_id() == Some(source_id.as_str()))
                    .then(|| existing.view_manifest());
            }
            if let Some((_, existing)) = state
                .tools
                .iter()
                .find(|(_, entry)| entry.manifest.name == manifest.name)
            {
                return (existing.binding.source_id() == Some(source_id.as_str()))
                    .then(|| existing.view_manifest());
            }
            state.tools.insert(
                manifest.id.clone(),
                bound_tool_entry(manifest.clone(), source_id, &self.hidden_tool_names),
            );
            state.generation += 1;
            return Some(manifest);
        }
        None
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        let known = {
            let state = self
                .state
                .read_recover();
            state
                .tools
                .get(id)
                .map(|entry| (entry.is_orphaned(), entry.view_manifest()))
        };
        match known {
            Some((false, manifest)) => return Some(manifest),
            Some((true, off_manifest)) => {
                return Some(self.try_rebind_orphan(id).unwrap_or(off_manifest));
            }
            None => {}
        }

        let sources = self
            .sources
            .read_recover()
            .iter()
            .map(|(source_id, source)| (source_id.clone(), Arc::clone(source)))
            .collect::<Vec<_>>();
        for (source_id, source) in sources {
            let Some(mut manifest) = source.resolve_manifest_by_id(id) else {
                continue;
            };
            manifest = manifest_with_compact_contract(source.as_ref(), manifest);
            let mut state = self
                .state
                .write_recover();
            if let Some((_, existing)) = state
                .tools
                .iter()
                .find(|(_, entry)| entry.manifest.name == manifest.name)
            {
                return (existing.binding.source_id() == Some(source_id.as_str()))
                    .then(|| existing.view_manifest());
            }
            state.tools.insert(
                id.clone(),
                bound_tool_entry(manifest.clone(), source_id, &self.hidden_tool_names),
            );
            state.generation += 1;
            return Some(manifest);
        }
        None
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest(name)?;
        self.resolve_contract_by_id(&manifest.id)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest_by_id(id)?;
        let source_id = {
            let state = self
                .state
                .read_recover();
            state
                .tools
                .get(id)
                .and_then(|entry| entry.binding.source_id().map(str::to_string))
        }?;
        self.sources
            .read_recover()
            .get(&source_id)?
            .resolve_contract_by_id(&manifest.id)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        let (source, _) = self.resolve_execution_source(&call.tool_id)?;
        source.prepare_tool_call(call).await
    }

    async fn prepare_granted_tool_call(
        &self,
        grant: &ToolExecutionGrant,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        if call.tool_id != grant.manifest.id {
            return Err(ToolResult::err_fmt(format_args!(
                "Granted prepare id `{}` does not match call id `{}`",
                grant.manifest.id, call.tool_id
            )));
        }
        let source = self.resolve_granted_execution_source(grant)?;
        source.prepare_tool_call(call).await
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        let Some(manifest) = self.resolve_manifest(call.name) else {
            return ToolResult::err_fmt(format_args!("Unknown tool: {}", call.name));
        };
        self.execute_by_id(&manifest.id, call.args, call.context)
            .await
    }

    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let (source, manifest) = match self.resolve_execution_source(tool_id) {
            Ok(resolved) => resolved,
            Err(result) => return result,
        };
        let _ = manifest;
        source
            .execute_by_id(tool_id, args, context)
            .await
    }

    async fn execute_granted(
        &self,
        grant: &ToolExecutionGrant,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let source = match self.resolve_granted_execution_source(grant) {
            Ok(source) => source,
            Err(result) => return result,
        };
        source
            .execute_by_id(&grant.manifest.id, args, context)
            .await
    }
}
