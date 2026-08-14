pub const PLUGIN_TOOL_SOURCE_ID: &str = "plugins";

/// Ephemeral identity for a live tool-provider source.
///
/// Session hosts use this handle to correlate provider bookkeeping and later
/// remove the source from the same open session. The identity is scoped to the
/// registry that issued it: it is not a durable cross-session identifier, and
/// another session may issue the same string for a different provider.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolSourceHandle {
    id: String,
}

impl ToolSourceHandle {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    /// Return the source identity used by session hosts to correlate this
    /// provider with host-owned routing or lifecycle state.
    ///
    /// The value is stable for this handle but is valid only for the registry
    /// that issued it. Do not persist it across session rebuilds or use it with
    /// another session.
    pub fn id(&self) -> &str {
        &self.id
    }
}

fn is_member_default() -> bool {
    true
}

fn is_default_member(member: &bool) -> bool {
    *member
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolStateEntry {
    manifest: ToolManifest,
    /// True when this tool was not resolvable from any registered source at
    /// export time (e.g. a detached MCP server). Orphaned entries keep their
    /// last-known manifest, are excluded from the Tool Catalog (non-members
    /// until their source returns), and rebind automatically when a source
    /// re-advertises the same tool id.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    orphaned: bool,
    /// Catalog membership. Members are callable; non-members do not exist to
    /// the model. Hosts toggle this via `set_tool_membership`.
    #[serde(default = "is_member_default", skip_serializing_if = "is_default_member")]
    member: bool,
    /// Persisted registration-lane hint. Missing values from pre-cutover
    /// snapshots decode as leaf registrations; on rebind the live source is
    /// authoritative and re-derives the effective lane.
    #[serde(default, skip_serializing_if = "is_leaf_registration")]
    registration_kind: ToolRegistrationKind,
}

impl ToolStateEntry {
    #[cfg(test)]
    pub(crate) fn new(manifest: ToolManifest) -> Self {
        Self {
            manifest,
            orphaned: false,
            member: true,
            registration_kind: ToolRegistrationKind::Leaf,
        }
    }

    /// The stored manifest as exposed to callers.
    pub fn manifest(&self) -> ToolManifest {
        self.manifest.clone()
    }

    fn stored_manifest(&self) -> &ToolManifest {
        &self.manifest
    }

    pub fn is_orphaned(&self) -> bool {
        self.orphaned
    }

    /// Whether this entry is currently a Tool Catalog member. Orphaned entries
    /// are never members.
    pub fn is_member(&self) -> bool {
        self.member && !self.orphaned
    }
}

fn is_leaf_registration(kind: &ToolRegistrationKind) -> bool {
    *kind == ToolRegistrationKind::Leaf
}

#[derive(Clone, Debug, Default)]
pub struct ToolState {
    generation: u64,
    tools: Arc<BTreeMap<ToolId, ToolStateEntry>>,
}

impl ToolState {
    pub(crate) fn new(generation: u64, tools: BTreeMap<ToolId, ToolStateEntry>) -> Self {
        Self {
            generation,
            tools: Arc::new(tools),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Edit a manifest in an explicit `ToolRegistry::apply_state` delta.
    ///
    /// Automatic rebuilds replace stored manifests with their live versions,
    /// so this is not a persistent source-curation mechanism.
    pub fn manifest_mut(&mut self, id: &ToolId) -> Option<&mut ToolManifest> {
        Arc::make_mut(&mut self.tools)
            .get_mut(id)
            .map(|entry| &mut entry.manifest)
    }

    /// Lets store and durable-substrate implementors test whether this `ToolState` is empty while
    /// snapshotting or restoring durable session state.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Reports the number of entries to store and durable-substrate implementors while snapshotting
    /// or restoring durable session state.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Iterates over the entries in the order consumed by store and durable-substrate implementors
    /// while snapshotting or restoring durable session state.
    pub fn iter(&self) -> impl Iterator<Item = (&ToolId, &ToolStateEntry)> {
        self.tools.iter()
    }

    /// Delete a tool in an explicit `ToolRegistry::apply_state` delta.
    ///
    /// Deletion intentionally removes the entry for that delta only. Use
    /// [`Self::set_membership`] for curation that must survive a rebuild from
    /// live sources.
    pub fn remove(&mut self, id: &ToolId) -> Option<ToolStateEntry> {
        Arc::make_mut(&mut self.tools).remove(id)
    }

    pub(crate) fn entries(&self) -> &BTreeMap<ToolId, ToolStateEntry> {
        self.tools.as_ref()
    }
}

impl Serialize for ToolState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct ToolStateRef<'a> {
            generation: u64,
            tools: &'a BTreeMap<ToolId, ToolStateEntry>,
        }

        ToolStateRef {
            generation: self.generation,
            tools: self.tools.as_ref(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ToolStateOwned {
            generation: u64,
            tools: BTreeMap<ToolId, ToolStateEntry>,
        }

        let owned = ToolStateOwned::deserialize(deserializer)?;
        Ok(Self {
            generation: owned.generation,
            tools: Arc::new(owned.tools),
        })
    }
}

#[async_trait::async_trait]
pub(crate) trait ToolSourceExecutor: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn source_key(&self) -> ToolSourceKey {
        ToolSourceKey::Leaf(self.id().to_string())
    }
    fn registration_kind(&self) -> ToolRegistrationKind {
        ToolRegistrationKind::Leaf
    }
    fn advertised_tools(&self) -> Vec<ToolManifest>;
    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        self.advertised_tools()
            .into_iter()
            .find(|manifest| manifest.name == name)
    }
    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.advertised_tools()
            .into_iter()
            .find(|manifest| manifest.id == *id)
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>>;
    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest_by_id(id)?;
        self.resolve_contract(&manifest.name)
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }
    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult;
    async fn execute_orchestrating(
        &self,
        _tool_id: &ToolId,
        _args: &serde_json::Value,
        _context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> ToolResult {
        ToolResult::err_fmt("leaf tools cannot execute in the orchestrating registration lane")
    }
    fn supports_attempt_context(&self, _tool_id: &ToolId) -> bool {
        false
    }
    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
    async fn execute_attempt_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> crate::ToolAttemptResult {
        let _ = (args, context);
        crate::ToolAttemptResult::from_tool_result(ToolResult::err_fmt(format_args!(
            "AttemptContext execution is unsupported for tool id `{tool_id}`"
        )))
    }
    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let Some(manifest) = self.resolve_manifest_by_id(tool_id) else {
            return ToolResult::err_fmt(format_args!("Unknown tool id: {tool_id}"));
        };
        self.execute(&manifest.name, args, context).await
    }
    async fn execute_internal_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::InternalProcessContext<'_>,
    ) -> ToolResult {
        self.execute_by_id(tool_id, args, context.__tool_context()).await
    }
}
