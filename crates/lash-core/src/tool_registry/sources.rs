struct OrchestratingToolSource {
    definition: crate::tool_provider::orchestration::OrchestratingToolDef,
}

impl OrchestratingToolSource {
    fn new(definition: crate::tool_provider::orchestration::OrchestratingToolDef) -> Self {
        Self { definition }
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for OrchestratingToolSource {
    fn id(&self) -> &str {
        "orchestrating"
    }

    fn source_key(&self) -> ToolSourceKey {
        ToolSourceKey::Orchestrating(self.definition.manifest().id)
    }

    fn registration_kind(&self) -> ToolRegistrationKind {
        ToolRegistrationKind::Orchestrating
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        vec![self.definition.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (self.definition.manifest().name == name).then(|| self.definition.contract())
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        self.definition.prepare_tool_call(call).await
    }

    async fn execute(
        &self,
        _tool: &str,
        _args: &serde_json::Value,
        _context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        ToolOutcome::err_fmt(
            "orchestrating tools require direct OrchestrationContext dispatch",
        )
    }

    async fn execute_orchestrating(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> ToolOutcome {
        if self.definition.manifest().id != *tool_id {
            return ToolOutcome::err_fmt(format_args!("Unknown orchestrating tool id: {tool_id}"));
        }
        self.definition.execute(args, context).await
    }
}

fn resolve_contract_for_indexed_manifest(
    provider: &dyn ToolProvider,
    manifest: &ToolManifest,
) -> Option<Arc<ToolContract>> {
    // The source index is authoritative for the id-to-name pairing. Accept a
    // name-resolved contract only when its process-local identity matches that
    // indexed pair; otherwise preserve the provider's by-id outcome. This
    // prevents a stale name reassigned to another id from crossing tool
    // identity while keeping the indexed fast path allocation-free.
    if let Some(contract) = provider.resolve_contract(&manifest.name)
        && contract.matches_manifest_identity(manifest)
    {
        return Some(contract);
    }
    provider.resolve_contract_by_id(&manifest.id)
}

/// One or more providers behind a single registry source, indexed by tool id:
/// an unknown id is refused here rather than delegated to a provider.
#[derive(Default)]
struct ToolProviderIndex {
    by_id: BTreeMap<ToolId, (ToolManifest, usize)>,
    by_name: BTreeMap<String, ToolId>,
}

impl ToolProviderIndex {
    fn from_providers(providers: &[Arc<dyn ToolProvider>]) -> Self {
        let mut index = Self::default();
        for (provider_idx, provider) in providers.iter().enumerate() {
            for manifest in provider.tool_manifests() {
                index.by_id.insert(manifest.id.clone(), (manifest, provider_idx));
            }
        }
        index.rebuild_name_index();
        index
    }

    fn rebuild_name_index(&mut self) {
        self.by_name.clear();
        for (id, (manifest, _)) in &self.by_id {
            self.by_name
                .entry(manifest.name.clone())
                .or_insert_with(|| id.clone());
        }
    }

    fn insert(&mut self, manifest: ToolManifest, provider_idx: usize) {
        let id = manifest.id.clone();
        if let Some((previous, _)) = self.by_id.get(&id)
            && previous.name != manifest.name
            && self.by_name.get(&previous.name) == Some(&id)
        {
            self.by_name.remove(&previous.name);
        }
        self.by_id
            .insert(id.clone(), (manifest.clone(), provider_idx));
        self.by_name.entry(manifest.name).or_insert(id);
    }

    fn get_by_name(&self, name: &str) -> Option<&(ToolManifest, usize)> {
        let id = self.by_name.get(name)?;
        self.by_id.get(id)
    }
}

struct ToolProviderSource {
    id: String,
    tools: RwLock<ToolProviderIndex>,
    providers: Vec<Arc<dyn ToolProvider>>,
}

impl ToolProviderSource {
    fn new(id: impl Into<String>, providers: Vec<Arc<dyn ToolProvider>>) -> Self {
        Self {
            id: id.into(),
            tools: RwLock::new(ToolProviderIndex::from_providers(&providers)),
            providers,
        }
    }

    fn read_advertised_tools(&self) -> Vec<ToolManifest> {
        let index = ToolProviderIndex::from_providers(&self.providers);
        let manifests = index
            .by_id
            .values()
            .map(|(manifest, _)| manifest.clone())
            .collect::<Vec<_>>();
        *self
            .tools
            .write_recover() = index;
        manifests
    }

    fn provider_index_for(&self, name: &str) -> Option<usize> {
        self.resolve_manifest(name).and_then(|_| {
            self.tools
                .read_recover()
                .get_by_name(name)
                .map(|(_, provider_idx)| *provider_idx)
        })
    }

    fn provider_index_for_id(&self, id: &ToolId) -> Option<usize> {
        self.indexed_manifest_and_provider_by_id(id)
            .map(|(_, provider_idx)| provider_idx)
    }

    fn indexed_manifest_and_provider_by_id(
        &self,
        id: &ToolId,
    ) -> Option<(ToolManifest, usize)> {
        if let Some((manifest, provider_idx)) = self
            .tools
            .read_recover()
            .by_id
            .get(id)
        {
            return Some((manifest.clone(), *provider_idx));
        }
        for (provider_idx, provider) in self.providers.iter().enumerate() {
            if let Some(manifest) = provider.resolve_manifest_by_id(id) {
                self.tools
                    .write_recover()
                    .insert(manifest.clone(), provider_idx);
                return Some((manifest, provider_idx));
            }
        }
        None
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for ToolProviderSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.read_advertised_tools()
    }

    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        if let Some((manifest, _)) = self.tools.read_recover().get_by_name(name) {
            return Some(manifest.clone());
        }
        for (provider_idx, provider) in self.providers.iter().enumerate() {
            if let Some(manifest) = provider.resolve_manifest(name) {
                self.tools
                    .write_recover()
                    .insert(manifest.clone(), provider_idx);
                return Some(manifest);
            }
        }
        None
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.indexed_manifest_and_provider_by_id(id)
            .map(|(manifest, _)| manifest)
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let provider_idx = self.provider_index_for(name)?;
        self.providers[provider_idx].resolve_contract(name)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let (manifest, provider_idx) = self.indexed_manifest_and_provider_by_id(id)?;
        resolve_contract_for_indexed_manifest(self.providers[provider_idx].as_ref(), &manifest)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        let Some(provider_idx) = self.provider_index_for_id(&call.tool_id) else {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Unknown tool id: {}",
                call.tool_id
            )));
        };
        self.providers[provider_idx].prepare_tool_call(call).await
    }

    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        let Some(provider_idx) = self.provider_index_for(tool) else {
            return ToolOutcome::err_fmt(format_args!("Unknown tool: {tool}"));
        };
        self.providers[provider_idx]
            .execute(ToolCall {
                name: tool,
                args,
                context,
            })
            .await
    }


    fn attempt_may_defer(&self, tool_id: &ToolId) -> bool {
        self.provider_index_for_id(tool_id)
            .is_some_and(|index| self.providers[index].attempt_may_defer(tool_id))
    }

    async fn execute_attempt_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> crate::ToolAttemptOutcome {
        let Some(provider_idx) = self.provider_index_for_id(tool_id) else {
            return crate::ToolAttemptOutcome::from_tool_result(ToolOutcome::err_fmt(format_args!(
                "Unknown tool id: {tool_id}"
            )));
        };
        self.providers[provider_idx]
            .execute_attempt_by_id(tool_id, args, context)
            .await
    }

    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        let Some(provider_idx) = self.provider_index_for_id(tool_id) else {
            return ToolOutcome::err_fmt(format_args!("Unknown tool id: {tool_id}"));
        };
        self.providers[provider_idx]
            .execute_by_id(tool_id, args, context)
            .await
    }

    async fn execute_internal_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::InternalProcessContext<'_>,
    ) -> ToolOutcome {
        let Some(provider_idx) = self.provider_index_for_id(tool_id) else {
            return ToolOutcome::err_fmt(format_args!("Unknown tool id: {tool_id}"));
        };
        self.providers[provider_idx]
            .execute_internal_by_id(tool_id, args, context)
            .await
    }
}

/// How a registry entry is connected to its tool source.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ToolBinding {
    /// Resolvable through the registered source with this id.
    Bound { source_key: ToolSourceKey },
    /// Persisted in a session snapshot but not resolvable from any currently
    /// registered source. Remains a non-member; execution fails loudly;
    /// rebinds when a source resolves the same id (the live name may evolve).
    Orphaned,
}

impl ToolBinding {
    fn source_key(&self) -> Option<&ToolSourceKey> {
        match self {
            Self::Bound { source_key } => Some(source_key),
            Self::Orphaned => None,
        }
    }
}
