impl ToolRegistry {
    pub(crate) fn resolve_catalog_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest(name)?;
        let is_member = self
            .state
            .read_recover()
            .surface
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
            .surface
            .get(tool_id)
            .filter(|entry| !entry.is_orphaned())
            .map(ToolRegistryEntry::view_manifest)
    }

    /// Resolve the source for a registry entry, distinguishing "unknown tool"
    /// from "known but orphaned" so callers can fail with a precise error.
    fn resolve_execution_source(
        &self,
        tool_id: &ToolId,
    ) -> Result<(Arc<dyn ToolSourceExecutor>, ToolManifest), ToolOutcome> {
        let Some(manifest) = self.resolve_manifest_by_id(tool_id) else {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Unknown tool id: {tool_id}"
            )));
        };
        let is_member = {
            let state = self
                .state
                .read_recover();
            state
                .surface
                .get(tool_id)
                .map(ToolRegistryEntry::is_member)
                .unwrap_or(true)
        };
        if !is_member {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Tool id `{tool_id}` is unavailable"
            )));
        }
        let binding = {
            let state = self
                .state
                .read_recover();
            state.surface.get(tool_id).map(|entry| entry.binding.clone())
        };
        let source_key = match binding {
            Some(ToolBinding::Bound { source_key }) => source_key,
            Some(ToolBinding::Orphaned) => {
                return Err(ToolOutcome::err_fmt(format_args!(
                    "Tool id `{tool_id}` is unavailable: it was restored from a persisted session \
                     but its source is not currently registered"
                )));
            }
            None => return Err(ToolOutcome::err_fmt(format_args!("Unknown tool id: {tool_id}"))),
        };
        let source = {
            self.sources
                .read_recover()
                .get(&source_key)
                .cloned()
        };
        source
            .map(|source| (source, manifest))
            .ok_or_else(|| {
                ToolOutcome::err_fmt(format_args!("Tool source missing for tool id `{tool_id}`"))
            })
    }

    fn resolve_granted_execution_source(
        &self,
        grant: &ToolExecutionGrant,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ToolOutcome> {
        let tool_id = &grant.manifest.id;
        let Some(source_id) = grant.source_id.as_deref() else {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Granted tool id `{tool_id}` is missing an explicit tool source"
            )));
        };
        let sources = self.sources.read_recover();
        let leaf_source_key = ToolSourceKey::Leaf(source_id.to_string());
        let source = match sources.get(&leaf_source_key) {
            Some(source) => Arc::clone(source),
            None if source_id == PLUGIN_TOOL_SOURCE_ID => {
                let mut matches = sources
                    .iter()
                    .filter(|(source_key, _)| matches!(source_key, ToolSourceKey::Leaf(_)))
                    .map(|(_, source)| source)
                    .filter(|source| source.resolve_manifest_by_id(tool_id).is_some());
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
        if source.resolve_manifest_by_id(tool_id).is_none() {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Tool source `{source_id}` does not resolve granted tool id `{tool_id}`"
            )));
        }
        Ok(source)
    }

    pub(crate) async fn execute_orchestrating_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> ToolOutcome {
        let (source, _) = match self.resolve_execution_source(tool_id) {
            Ok(resolved) => resolved,
            Err(result) => return result,
        };
        if source.registration_kind() != ToolRegistrationKind::Orchestrating {
            return ToolOutcome::err_fmt(format_args!(
                "Tool id `{tool_id}` is not an orchestrating registration"
            ));
        }
        source.execute_orchestrating(tool_id, args, context).await
    }
}

#[async_trait::async_trait]
impl ToolProvider for ToolRegistry {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        let state = self
            .state
            .read_recover();
        state
            .surface
            .by_id
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
                .surface
                .get_by_name(name)
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
            if let Some(existing) = state.surface.get(&manifest.id) {
                return (existing.binding.source_key() == Some(&source_id))
                    .then(|| existing.view_manifest());
            }
            if let Some((_, existing)) = state.surface.get_by_name(&manifest.name) {
                return (existing.binding.source_key() == Some(&source_id))
                    .then(|| existing.view_manifest());
            }
            if state
                .surface
                .insert(bound_tool_entry(
                    manifest.clone(),
                    source_id,
                    source.registration_kind(),
                    &self.hidden_tool_names,
                ))
                .is_err()
            {
                return None;
            }
            state.generation += 1;
            state.surface.debug_assert_invariant();
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
                .surface
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
            if let Some((_, existing)) = state.surface.get_by_name(&manifest.name) {
                return (existing.binding.source_key() == Some(&source_id))
                    .then(|| existing.view_manifest());
            }
            if state
                .surface
                .insert(bound_tool_entry(
                    manifest.clone(),
                    source_id,
                    source.registration_kind(),
                    &self.hidden_tool_names,
                ))
                .is_err()
            {
                return None;
            }
            state.generation += 1;
            state.surface.debug_assert_invariant();
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
        let source_key = {
            let state = self
                .state
                .read_recover();
            state
                .surface
                .get(id)
                .and_then(|entry| entry.binding.source_key().cloned())
        }?;
        self.sources
            .read_recover()
            .get(&source_key)?
            .resolve_contract_by_id(&manifest.id)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        let (source, _) = self.resolve_execution_source(&call.tool_id)?;
        source.prepare_tool_call(call).await
    }

    async fn prepare_granted_tool_call(
        &self,
        grant: &ToolExecutionGrant,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        if call.tool_id != grant.manifest.id {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Granted prepare id `{}` does not match call id `{}`",
                grant.manifest.id, call.tool_id
            )));
        }
        let source = self.resolve_granted_execution_source(grant)?;
        source.prepare_tool_call(call).await
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let Some(manifest) = self.resolve_manifest(call.name) else {
            return ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name));
        };
        self.execute_by_id(&manifest.id, call.args, call.context)
            .await
    }



    fn attempt_may_defer(&self, tool_id: &ToolId) -> bool {
        self.resolve_execution_source(tool_id)
            .is_ok_and(|(source, _)| source.attempt_may_defer(tool_id))
    }

    async fn execute_attempt_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> crate::ToolAttemptOutcome {
        let (source, _) = match self.resolve_execution_source(tool_id) {
            Ok(resolved) => resolved,
            Err(result) => return crate::ToolAttemptOutcome::from_tool_result(result),
        };
        source.execute_attempt_by_id(tool_id, args, context).await
    }

    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        let (source, manifest) = match self.resolve_execution_source(tool_id) {
            Ok(resolved) => resolved,
            Err(result) => return result,
        };
        let _ = manifest;
        source
            .execute_by_id(tool_id, args, context)
            .await
    }

    async fn execute_internal_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::InternalProcessContext<'_>,
    ) -> ToolOutcome {
        let (source, manifest) = match self.resolve_execution_source(tool_id) {
            Ok(resolved) => resolved,
            Err(result) => return result,
        };
        if manifest.activation != crate::ToolActivation::Internal {
            return ToolOutcome::err_fmt(format_args!(
                "tool id `{tool_id}` is not activated for internal execution"
            ));
        }
        source
            .execute_internal_by_id(tool_id, args, context)
            .await
    }

    async fn execute_granted(
        &self,
        grant: &ToolExecutionGrant,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        let source = match self.resolve_granted_execution_source(grant) {
            Ok(source) => source,
            Err(result) => return result,
        };
        source
            .execute_by_id(&grant.manifest.id, args, context)
            .await
    }

    async fn execute_granted_attempt(
        &self,
        grant: &ToolExecutionGrant,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> crate::ToolAttemptOutcome {
        let source = match self.resolve_granted_execution_source(grant) {
            Ok(source) => source,
            Err(result) => return crate::ToolAttemptOutcome::from_tool_result(result),
        };
        source
            .execute_attempt_by_id(&grant.manifest.id, args, context)
            .await
    }
}
