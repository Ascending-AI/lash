use super::*;

/// Accept a provider's name-resolved contract only when its process-local
/// identity matches the manifest an id lookup established; otherwise preserve
/// the provider's by-id outcome.
///
/// This is the single enforcement point that keeps a name→contract hop from
/// crossing tool identity: a provider's own name resolution is live, so a
/// name captured by an earlier id lookup may have been reassigned to another
/// id by the time the contract is fetched. The pinned behaviour — a stale
/// name falls back to the by-id outcome and never returns another id's
/// contract — is exercised by
/// `indexed_contract_lookup_does_not_cross_identity_after_name_drift`.
fn resolve_contract_for_manifest(
    provider: &dyn ToolProvider,
    manifest: &ToolManifest,
) -> Option<Arc<ToolContract>> {
    if let Some(contract) = provider.resolve_contract(&manifest.name)
        && contract.matches_manifest_identity(manifest)
    {
        return Some(contract);
    }
    provider.resolve_contract_by_id(&manifest.id)
}

/// One provider's advertisement, indexed by tool id: an unknown id is refused
/// here rather than delegated to the provider.
///
/// The live [`ToolProviderSource`]'s copy is rebuilt once per advertise cycle
/// (`advertised_tools`); a fresh copy is also built inside
/// [`ToolProviderSourceCapture`] and handed to the
/// [`PinnedToolProviderSource`] the cycle produces. No lookup mutates it, so
/// "what does this name resolve to" cannot depend on lookup order.
#[derive(Clone, Default)]
struct ToolProviderIndex {
    by_id: BTreeMap<ToolId, ToolManifest>,
    by_name: BTreeMap<String, ToolId>,
}

impl ToolProviderIndex {
    fn from_provider(provider: &dyn ToolProvider) -> Self {
        let mut index = Self::default();
        for manifest in provider.tool_manifests() {
            index
                .by_name
                .entry(manifest.name.clone())
                .or_insert_with(|| manifest.id.clone());
            index.by_id.insert(manifest.id.clone(), manifest);
        }
        index
    }

    fn insert(&mut self, manifest: ToolManifest) {
        let id = manifest.id.clone();
        if let Some(previous) = self.by_id.get(&id)
            && previous.name != manifest.name
            && self.by_name.get(&previous.name) == Some(&id)
        {
            self.by_name.remove(&previous.name);
        }
        self.by_id.insert(id.clone(), manifest.clone());
        self.by_name.entry(manifest.name).or_insert(id);
    }
}

/// The live view over one provider, the registry source of its declared
/// identity (or of the handle a host attached it under): every tool it routes
/// is executed by that provider and no other.
///
/// `tools` is written once per advertise cycle — `read_advertised_tools`
/// rebuilds it from the provider's current manifests. Every lookup is pure
/// over it: a miss asks the provider and the answer is not recorded, so
/// whether an unadvertised id resolves cannot depend on whether a lookup ran
/// first.
pub(super) struct ToolProviderSource {
    key: ToolSourceKey,
    label: String,
    tools: Arc<RwLock<ToolProviderIndex>>,
    provider: Arc<dyn ToolProvider>,
}

struct ToolProviderSourceCapture {
    key: ToolSourceKey,
    index: ToolProviderIndex,
    provider: Arc<dyn ToolProvider>,
}

impl ToolSourceCapture for ToolProviderSourceCapture {
    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.index.by_id.values().cloned().collect()
    }

    fn advertised_ids(&self) -> BTreeSet<ToolId> {
        self.index.by_id.keys().cloned().collect()
    }

    fn freeze(
        self: Box<Self>,
        known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        let Self {
            key,
            mut index,
            provider,
        } = *self;
        let advertised_ids = index.by_id.keys().cloned().collect();
        for resident_id in known_resident_ids {
            if index.by_id.contains_key(resident_id) {
                continue;
            }
            if let Some(manifest) = provider.resolve_manifest_by_id(resident_id) {
                if manifest.id != *resident_id {
                    return Err(ReconfigureError::Validation(format!(
                        "source `{}` resolved tool id `{resident_id}` with mismatched \
                         manifest id `{}`",
                        key, manifest.id,
                    )));
                }
                index.insert(manifest);
            }
        }
        Ok(Arc::new(PinnedToolProviderSource::new(
            key,
            index,
            advertised_ids,
            provider,
        )))
    }
}

impl ToolProviderSource {
    pub(super) fn new(key: ToolSourceKey, provider: Arc<dyn ToolProvider>) -> Self {
        Self {
            label: key.to_string(),
            key,
            tools: Arc::new(RwLock::new(ToolProviderIndex::default())),
            provider,
        }
    }

    /// The advertise cycle: republish the index from the provider's current
    /// manifests and return them. This is the index's only writer; every
    /// lookup below is a pure read over the published snapshot.
    fn read_advertised_tools(&self) -> Vec<ToolManifest> {
        let index = ToolProviderIndex::from_provider(self.provider.as_ref());
        let manifests = index.by_id.values().cloned().collect::<Vec<_>>();
        *self.tools.write_recover() = index;
        manifests
    }

    /// The indexed manifest for `id`, else the provider's resolution of it
    /// to a manifest that actually carries it. The provider answer is
    /// returned without being recorded: whether an unadvertised id resolves
    /// cannot depend on whether a lookup ran first.
    fn manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        if let Some(manifest) = self.tools.read_recover().by_id.get(id) {
            return Some(manifest.clone());
        }
        self.provider
            .resolve_manifest_by_id(id)
            .filter(|manifest| manifest.id == *id)
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for ToolProviderSource {
    fn id(&self) -> &str {
        &self.label
    }

    fn source_key(&self) -> ToolSourceKey {
        self.key.clone()
    }

    fn capture_execution_source(&self) -> Result<Box<dyn ToolSourceCapture>, ReconfigureError> {
        Ok(Box::new(ToolProviderSourceCapture {
            key: self.key.clone(),
            index: ToolProviderIndex::from_provider(self.provider.as_ref()),
            provider: Arc::clone(&self.provider),
        }))
    }

    fn snapshot_execution_source(
        &self,
        known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        self.capture_execution_source()?.freeze(known_resident_ids)
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.read_advertised_tools()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.manifest_by_id(id)
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.provider.resolve_contract(name)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.manifest_by_id(id)?;
        resolve_contract_for_manifest(self.provider.as_ref(), &manifest)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        if self.manifest_by_id(&call.tool_id).is_none() {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Unknown tool id: {}",
                call.tool_id
            )));
        }
        self.provider.prepare_tool_call(call).await
    }

    fn execution(&self) -> &dyn LeafToolSourceExecutor {
        self
    }
}

#[async_trait::async_trait]
impl LeafToolSourceExecutor for ToolProviderSource {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if self.manifest_by_id(call.tool_id()).is_none() {
            return ToolOutcome::err_fmt(format_args!("Unknown tool id: {}", call.tool_id()))
                .into();
        }
        self.provider.execute(call).await
    }
}

#[derive(Clone)]
struct PinnedToolProviderSource {
    key: ToolSourceKey,
    label: String,
    routes: BTreeMap<ToolId, ToolManifest>,
    by_name: BTreeMap<String, ToolId>,
    advertised_ids: BTreeSet<ToolId>,
    provider: Arc<dyn ToolProvider>,
}

impl PinnedToolProviderSource {
    fn new(
        key: ToolSourceKey,
        index: ToolProviderIndex,
        advertised_ids: BTreeSet<ToolId>,
        provider: Arc<dyn ToolProvider>,
    ) -> Self {
        Self {
            label: key.to_string(),
            key,
            routes: index.by_id,
            by_name: index.by_name,
            advertised_ids,
            provider,
        }
    }

    fn route(&self, id: &ToolId) -> Option<&ToolManifest> {
        self.routes.get(id)
    }

    fn route_by_name(&self, name: &str) -> Option<&ToolManifest> {
        self.route(self.by_name.get(name)?)
    }
}

/// Resident-only view of one provider captured by one advertisement.
///
/// The source layer has already resolved each id to a manifest.
/// Execution preserves the provider's authoritative by-id override; the
/// sealed invocation context gives only the default trait implementation the
/// captured source name, so it need not rematerialize a later advertisement.
/// Grant-authorized deferred calls retain the original live source.
#[async_trait::async_trait]
impl ToolSourceExecutor for PinnedToolProviderSource {
    fn id(&self) -> &str {
        &self.label
    }

    fn source_key(&self) -> ToolSourceKey {
        self.key.clone()
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(self.clone()))
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.routes
            .iter()
            .filter(|(id, _)| self.advertised_ids.contains(*id))
            .map(|(_, manifest)| manifest.clone())
            .collect()
    }

    fn advertised_ids(&self) -> BTreeSet<ToolId> {
        self.advertised_ids.clone()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.route(id).cloned()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let manifest = self.route_by_name(name)?;
        resolve_contract_for_manifest(self.provider.as_ref(), manifest)
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.route(id)?;
        resolve_contract_for_manifest(self.provider.as_ref(), manifest)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        if self.route(&call.tool_id).is_none() {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Unknown tool id: {}",
                call.tool_id
            )));
        }
        self.provider.prepare_tool_call(call).await
    }

    fn execution(&self) -> &dyn LeafToolSourceExecutor {
        self
    }
}

#[async_trait::async_trait]
impl LeafToolSourceExecutor for PinnedToolProviderSource {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let Some(route) = self.route(call.tool_id()) else {
            return ToolOutcome::err_fmt(format_args!("Unknown tool id: {}", call.tool_id()))
                .into();
        };
        let mut manifest = call.manifest().clone();
        manifest.name.clone_from(&route.name);
        let provider_call = ToolCall::new(&manifest, call.args, call.context);
        self.provider.execute(provider_call).await
    }
}

/// How a registry entry is connected to its tool source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ToolBinding {
    /// Resolvable through the registered source with this id.
    Bound { source_key: ToolSourceKey },
    /// Persisted in a session snapshot but not resolvable from any currently
    /// registered source. Remains a non-member; execution fails loudly;
    /// rebinds when a source resolves the same id (the live name may evolve).
    Orphaned,
}

impl ToolBinding {
    pub(super) fn source_key(&self) -> Option<&ToolSourceKey> {
        match self {
            Self::Bound { source_key } => Some(source_key),
            Self::Orphaned => None,
        }
    }
}
