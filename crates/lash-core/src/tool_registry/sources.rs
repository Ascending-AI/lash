struct ToolProviderSource {
    id: String,
    provider: Arc<dyn ToolProvider>,
}

impl ToolProviderSource {
    fn new(id: impl Into<String>, provider: Arc<dyn ToolProvider>) -> Self {
        Self {
            id: id.into(),
            provider,
        }
    }
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
            .write()
            .expect("tool provider group lock poisoned") = tools;
        manifests
    }

    fn provider_index_for(&self, name: &str) -> Option<usize> {
        self.resolve_manifest(name).and_then(|_| {
            self.tools
                .read()
                .expect("tool provider group lock poisoned")
                .values()
                .find(|(manifest, _)| manifest.name == name)
                .map(|(_, provider_idx)| *provider_idx)
        })
    }

    fn provider_index_for_id(&self, id: &ToolId) -> Option<usize> {
        self.resolve_manifest_by_id(id).and_then(|_| {
            self.tools
                .read()
                .expect("tool provider group lock poisoned")
                .get(id)
                .map(|(_, provider_idx)| *provider_idx)
        })
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
            .read()
            .expect("tool provider group lock poisoned")
            .values()
            .find(|(manifest, _)| manifest.name == name)
        {
            return Some(manifest.clone());
        }
        for (provider_idx, provider) in self.providers.iter().enumerate() {
            if let Some(manifest) = provider.resolve_manifest(name) {
                self.tools
                    .write()
                    .expect("tool provider group lock poisoned")
                    .insert(manifest.id.clone(), (manifest.clone(), provider_idx));
                return Some(manifest);
            }
        }
        None
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        if let Some((manifest, _)) = self
            .tools
            .read()
            .expect("tool provider group lock poisoned")
            .get(id)
        {
            return Some(manifest.clone());
        }
        for (provider_idx, provider) in self.providers.iter().enumerate() {
            if let Some(manifest) = provider.resolve_manifest_by_id(id) {
                self.tools
                    .write()
                    .expect("tool provider group lock poisoned")
                    .insert(id.clone(), (manifest.clone(), provider_idx));
                return Some(manifest);
            }
        }
        None
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let provider_idx = self.provider_index_for(name)?;
        self.providers[provider_idx].resolve_contract(name)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let (manifest, provider_idx) = self
            .resolve_manifest_by_id(id)
            .and_then(|manifest| {
                self.tools
                    .read()
                    .expect("tool provider group lock poisoned")
                    .get(id)
                    .map(|(_, provider_idx)| (manifest, *provider_idx))
            })?;
        let _ = manifest;
        self.providers[provider_idx].resolve_contract_by_id(id)
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
}

#[async_trait::async_trait]
impl ToolSourceExecutor for ToolProviderSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.provider.tool_manifests()
    }

    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        self.provider.resolve_manifest(name)
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.provider.resolve_manifest_by_id(id)
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.provider.resolve_contract(name)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        self.provider.resolve_contract_by_id(id)
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
}

/// How a registry entry is connected to its tool source.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ToolBinding {
    /// Resolvable through the registered source with this id.
    Bound(String),
    /// Persisted in a session snapshot but not resolvable from any currently
    /// registered source. Remains a non-member; execution fails loudly;
    /// rebinds when a source resolves the same id (the live name may evolve).
    Orphaned,
}

impl ToolBinding {
    fn source_id(&self) -> Option<&str> {
        match self {
            Self::Bound(id) => Some(id),
            Self::Orphaned => None,
        }
    }
}
