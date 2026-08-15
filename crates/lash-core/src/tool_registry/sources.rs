struct ToolProviderSource {
    id: String,
    provider: Arc<dyn ToolProvider>,
    tools: RwLock<BTreeMap<ToolId, ToolManifest>>,
}

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
    ) -> Result<PreparedToolCall, ToolResult> {
        self.definition.prepare_tool_call(call).await
    }

    async fn execute(
        &self,
        _tool: &str,
        _args: &serde_json::Value,
        _context: &ToolContext<'_>,
    ) -> ToolResult {
        ToolResult::err_fmt(
            "orchestrating tools require direct OrchestrationContext dispatch",
        )
    }

    async fn execute_orchestrating(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> ToolResult {
        if self.definition.manifest().id != *tool_id {
            return ToolResult::err_fmt(format_args!("Unknown orchestrating tool id: {tool_id}"));
        }
        self.definition.execute(args, context).await
    }
}

impl ToolProviderSource {
    fn new(id: impl Into<String>, provider: Arc<dyn ToolProvider>) -> Self {
        Self {
            id: id.into(),
            provider,
            tools: RwLock::new(BTreeMap::new()),
        }
    }

    fn read_advertised_tools(&self) -> Vec<ToolManifest> {
        let manifests = self.provider.tool_manifests();
        *self.tools.write_recover() = manifests
            .iter()
            .cloned()
            .map(|manifest| (manifest.id.clone(), manifest))
            .collect();
        manifests
    }

    fn indexed_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        if let Some(manifest) = self
            .tools
            .read_recover()
            .get(id)
        {
            return Some(manifest.clone());
        }
        let manifest = self.provider.resolve_manifest_by_id(id)?;
        self.tools
            .write_recover()
            .insert(id.clone(), manifest.clone());
        Some(manifest)
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

struct ToolProviderGroupSource {
    id: String,
    tools: RwLock<BTreeMap<ToolId, (ToolManifest, usize)>>,
    providers: Vec<Arc<dyn ToolProvider>>,
}

impl ToolProviderGroupSource {
    fn new(id: impl Into<String>, providers: Vec<Arc<dyn ToolProvider>>) -> Self {
        let mut tools = BTreeMap::new();
        for (provider_idx, provider) in providers.iter().enumerate() {
            for manifest in provider.tool_manifests() {
                tools.insert(manifest.id.clone(), (manifest, provider_idx));
            }
        }
        Self {
            id: id.into(),
            tools: RwLock::new(tools),
            providers,
        }
    }

    fn read_advertised_tools(&self) -> Vec<ToolManifest> {
        let mut tools = BTreeMap::new();
        for (provider_idx, provider) in self.providers.iter().enumerate() {
            for manifest in provider.tool_manifests() {
                tools.insert(manifest.id.clone(), (manifest, provider_idx));
            }
        }
        let manifests = tools
            .values()
            .map(|(manifest, _)| manifest.clone())
            .collect::<Vec<_>>();
        *self
            .tools
            .write_recover() = tools;
        manifests
    }

    fn provider_index_for(&self, name: &str) -> Option<usize> {
        self.resolve_manifest(name).and_then(|_| {
            self.tools
                .read_recover()
                .values()
                .find(|(manifest, _)| manifest.name == name)
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
            .get(id)
        {
            return Some((manifest.clone(), *provider_idx));
        }
        for (provider_idx, provider) in self.providers.iter().enumerate() {
            if let Some(manifest) = provider.resolve_manifest_by_id(id) {
                self.tools
                    .write_recover()
                    .insert(id.clone(), (manifest.clone(), provider_idx));
                return Some((manifest, provider_idx));
            }
        }
        None
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for ToolProviderGroupSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.read_advertised_tools()
    }

    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        if let Some((manifest, _)) = self
            .tools
            .read_recover()
            .values()
            .find(|(manifest, _)| manifest.name == name)
        {
            return Some(manifest.clone());
        }
        for (provider_idx, provider) in self.providers.iter().enumerate() {
            if let Some(manifest) = provider.resolve_manifest(name) {
                self.tools
                    .write_recover()
                    .insert(manifest.id.clone(), (manifest.clone(), provider_idx));
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
    ) -> Result<PreparedToolCall, ToolResult> {
        let name = call.pending.tool_name.clone();
        let Some(provider_idx) = self.provider_index_for_id(&call.tool_id) else {
            return Err(ToolResult::err_fmt(format_args!(
                "Unknown tool id: {}",
                call.tool_id
            )));
        };
        let _ = name;
        self.providers[provider_idx].prepare_tool_call(call).await
    }

    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let Some(provider_idx) = self.provider_index_for(tool) else {
            return ToolResult::err_fmt(format_args!("Unknown tool: {tool}"));
        };
        self.providers[provider_idx]
            .execute(ToolCall {
                name: tool,
                args,
                context,
            })
            .await
    }

    fn supports_attempt_context(&self, tool_id: &ToolId) -> bool {
        self.provider_index_for_id(tool_id)
            .is_some_and(|index| self.providers[index].supports_attempt_context(tool_id))
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
    ) -> crate::ToolAttemptResult {
        let Some(provider_idx) = self.provider_index_for_id(tool_id) else {
            return crate::ToolAttemptResult::from_tool_result(ToolResult::err_fmt(format_args!(
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
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let Some(provider_idx) = self.provider_index_for_id(tool_id) else {
            return ToolResult::err_fmt(format_args!("Unknown tool id: {tool_id}"));
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
    ) -> ToolResult {
        let Some(provider_idx) = self.provider_index_for_id(tool_id) else {
            return ToolResult::err_fmt(format_args!("Unknown tool id: {tool_id}"));
        };
        self.providers[provider_idx]
            .execute_internal_by_id(tool_id, args, context)
            .await
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
        if let Some(manifest) = self
            .tools
            .read_recover()
            .values()
            .find(|manifest| manifest.name == name)
        {
            return Some(manifest.clone());
        }
        let manifest = self.provider.resolve_manifest(name)?;
        self.tools
            .write_recover()
            .insert(manifest.id.clone(), manifest.clone());
        Some(manifest)
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.indexed_manifest_by_id(id)
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.provider.resolve_contract(name)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.indexed_manifest_by_id(id)?;
        resolve_contract_for_indexed_manifest(self.provider.as_ref(), &manifest)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        self.provider.prepare_tool_call(call).await
    }

    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        self.provider
            .execute(ToolCall {
                name: tool,
                args,
                context,
            })
            .await
    }

    fn supports_attempt_context(&self, tool_id: &ToolId) -> bool {
        self.provider.supports_attempt_context(tool_id)
    }

    fn attempt_may_defer(&self, tool_id: &ToolId) -> bool {
        self.provider.attempt_may_defer(tool_id)
    }

    async fn execute_attempt_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> crate::ToolAttemptResult {
        self.provider
            .execute_attempt_by_id(tool_id, args, context)
            .await
    }

    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        self.provider
            .execute_by_id(tool_id, args, context)
            .await
    }

    async fn execute_internal_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::InternalProcessContext<'_>,
    ) -> ToolResult {
        self.provider
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
