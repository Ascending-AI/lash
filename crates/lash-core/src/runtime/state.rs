//! Runtime session state and persistence helpers.
//!
//! `RuntimeSessionState` is the runtime-private mutable state shape. Public
//! host/plugin reads use `SessionSnapshot` from the plugin API instead.

use crate::facade_support::{SessionGraphFacadeOps, ToolStateFacadeOps};
use lash_sansio::PromptUsage;

use crate::session_model::{Message, SessionPolicy, TokenUsage, plugin_message_to_message};
use crate::{PersistedTurnState, SessionSnapshot};

use super::usage::TokenLedgerEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointComponentCompleteness {
    Complete,
    Unproven,
}

#[derive(Clone, Debug)]
enum ResidentCheckpointComponentBody {
    ToolState {
        snapshot: Option<crate::ToolState>,
        generation: Option<u64>,
    },
    PluginSnapshot(Option<crate::PluginSessionSnapshot>),
    ExecutionState(Option<Vec<u8>>),
    Opaque(Option<Vec<u8>>),
}

#[derive(Clone, Debug)]
struct ResidentCheckpointComponent {
    descriptor: Option<crate::CheckpointComponentDescriptor>,
    body: ResidentCheckpointComponentBody,
    dirty: bool,
}

/// Runtime-owned checkpoint component listing with an explicit completeness proof.
///
/// Entries and well-known typed bodies are private so callers cannot mutate a
/// typed view without updating the authoritative keyed set. A value rebuilt
/// from the public `SessionSnapshot` is deliberately unproven: that projection
/// contains only well-known refs and cannot establish that unknown keys are
/// absent. A set becomes complete only when it was derived from a full hydrated
/// manifest (or created empty for a session known to have no prior checkpoint).
/// Commits assembled from an unproven set are refused with
/// [`crate::StoreError::IncompleteCheckpointComponentSet`]. In a complete set,
/// absence of a key is authoritative and means that component is deleted.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**
/// encounter this invariant through runtime commits and must preserve the full
/// keyed set rather than merging it with a previous checkpoint root.
#[derive(Clone, Debug)]
pub struct RuntimeCheckpointComponents {
    completeness: CheckpointComponentCompleteness,
    entries: std::collections::BTreeMap<String, ResidentCheckpointComponent>,
}

impl Default for RuntimeCheckpointComponents {
    fn default() -> Self {
        Self::unproven()
    }
}

impl RuntimeCheckpointComponents {
    pub(crate) fn complete_empty() -> Self {
        Self {
            completeness: CheckpointComponentCompleteness::Complete,
            entries: std::collections::BTreeMap::new(),
        }
    }

    pub(crate) fn unproven() -> Self {
        Self {
            completeness: CheckpointComponentCompleteness::Unproven,
            entries: std::collections::BTreeMap::new(),
        }
    }

    /// Requires the source state for a newly created destination session to
    /// carry a complete component-set proof.
    ///
    /// Only a set derived from a full hydrated manifest (or a known-empty new
    /// session) is complete. Public snapshot projections are `Unproven`
    /// because they omit unknown keys; promoting one would turn those omissions
    /// into deletions. The caller must propagate the typed
    /// [`crate::StoreError::IncompleteCheckpointComponentSet`] refusal rather
    /// than constructing a destination commit from partial state.
    pub(crate) fn complete_for_new_session(&self) -> Result<(), crate::StoreError> {
        match self.completeness {
            CheckpointComponentCompleteness::Complete => Ok(()),
            CheckpointComponentCompleteness::Unproven => {
                Err(crate::StoreError::IncompleteCheckpointComponentSet)
            }
        }
    }

    fn descriptor(blob_ref: crate::store::BlobRef) -> crate::CheckpointComponentDescriptor {
        crate::CheckpointComponentDescriptor {
            blob_ref,
            encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
        }
    }

    fn from_snapshot(snapshot: &SessionSnapshot) -> Self {
        let mut result = Self::unproven();
        if let Some(blob_ref) = snapshot.tool_state_ref.clone() {
            result.entries.insert(
                crate::store::TOOL_STATE_CHECKPOINT_COMPONENT.to_string(),
                ResidentCheckpointComponent {
                    descriptor: Some(Self::descriptor(blob_ref)),
                    body: ResidentCheckpointComponentBody::ToolState {
                        snapshot: None,
                        generation: snapshot.tool_state_generation,
                    },
                    dirty: false,
                },
            );
        }
        if let Some(blob_ref) = snapshot.plugin_snapshot_ref.clone() {
            result.entries.insert(
                crate::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT.to_string(),
                ResidentCheckpointComponent {
                    descriptor: Some(Self::descriptor(blob_ref)),
                    body: ResidentCheckpointComponentBody::PluginSnapshot(None),
                    dirty: false,
                },
            );
        }
        if let Some(blob_ref) = snapshot.execution_state_ref.clone() {
            result.entries.insert(
                crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                ResidentCheckpointComponent {
                    descriptor: Some(Self::descriptor(blob_ref)),
                    body: ResidentCheckpointComponentBody::ExecutionState(None),
                    dirty: false,
                },
            );
        }
        result
    }

    fn from_hydrated(
        checkpoint: &crate::store::HydratedSessionCheckpoint,
    ) -> Result<Self, crate::StoreError> {
        let manifest = checkpoint.manifest()?;
        let mut entries = std::collections::BTreeMap::new();
        for key in checkpoint.components.keys() {
            let descriptor = manifest.components.get(key).cloned().ok_or_else(|| {
                crate::StoreError::StoredDataCorrupt {
                    record_kind: "HydratedSessionCheckpoint",
                    message: format!("manifest projection lost component `{key}`"),
                }
            })?;
            let body = match key.as_str() {
                crate::store::TOOL_STATE_CHECKPOINT_COMPONENT => {
                    let snapshot = checkpoint
                        .decode_component::<crate::ToolState>(key)?
                        .ok_or_else(|| crate::StoreError::StoredDataCorrupt {
                            record_kind: "HydratedSessionCheckpoint",
                            message: format!("component `{key}` disappeared during decode"),
                        })?;
                    let generation = Some(snapshot.generation());
                    ResidentCheckpointComponentBody::ToolState {
                        snapshot: Some(snapshot),
                        generation,
                    }
                }
                crate::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT => {
                    ResidentCheckpointComponentBody::PluginSnapshot(
                        checkpoint.decode_component::<crate::PluginSessionSnapshot>(key)?,
                    )
                }
                crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT => {
                    ResidentCheckpointComponentBody::ExecutionState(
                        checkpoint.checked_component_body(key)?.map(<[u8]>::to_vec),
                    )
                }
                _ => ResidentCheckpointComponentBody::Opaque(
                    checkpoint.checked_component_body(key)?.map(<[u8]>::to_vec),
                ),
            };
            entries.insert(
                key.clone(),
                ResidentCheckpointComponent {
                    descriptor: Some(descriptor),
                    body,
                    dirty: false,
                },
            );
        }
        Ok(Self {
            completeness: CheckpointComponentCompleteness::Complete,
            entries,
        })
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn complete_refs_for_testing(
        refs: impl IntoIterator<Item = (String, crate::store::BlobRef)>,
    ) -> Self {
        let entries = refs
            .into_iter()
            .map(|(key, blob_ref)| {
                (
                    key,
                    ResidentCheckpointComponent {
                        descriptor: Some(Self::descriptor(blob_ref)),
                        body: ResidentCheckpointComponentBody::Opaque(None),
                        dirty: false,
                    },
                )
            })
            .collect();
        Self {
            completeness: CheckpointComponentCompleteness::Complete,
            entries,
        }
    }

    pub(crate) fn build_checkpoint(
        &self,
        turn_state: crate::PersistedTurnState,
        plugin_snapshot_revision: Option<u64>,
    ) -> Result<crate::store::HydratedSessionCheckpoint, crate::StoreError> {
        if self.completeness != CheckpointComponentCompleteness::Complete {
            return Err(crate::StoreError::IncompleteCheckpointComponentSet);
        }
        let mut components = std::collections::BTreeMap::new();
        for (key, component) in &self.entries {
            let pending = if component.dirty {
                let body = match &component.body {
                    ResidentCheckpointComponentBody::ToolState {
                        snapshot: Some(snapshot),
                        ..
                    } => crate::store::encode_checkpoint_component(key, snapshot)?,
                    ResidentCheckpointComponentBody::PluginSnapshot(Some(snapshot)) => {
                        crate::store::encode_checkpoint_component(key, snapshot)?
                    }
                    ResidentCheckpointComponentBody::ExecutionState(Some(bytes))
                    | ResidentCheckpointComponentBody::Opaque(Some(bytes)) => bytes.clone(),
                    _ => {
                        return Err(crate::StoreError::StoredDataCorrupt {
                            record_kind: "RuntimeCheckpointComponents",
                            message: format!("dirty component `{key}` has no body"),
                        });
                    }
                };
                crate::HydratedCheckpointComponent::changed(body)
            } else {
                let descriptor = component.descriptor.as_ref().ok_or_else(|| {
                    crate::StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeCheckpointComponents",
                        message: format!("unchanged component `{key}` has no durable ref"),
                    }
                })?;
                crate::HydratedCheckpointComponent::unchanged(descriptor)
            };
            components.insert(key.clone(), pending);
        }
        Ok(crate::store::HydratedSessionCheckpoint {
            turn_state,
            components,
            plugin_snapshot_revision,
        })
    }

    fn component(&self, key: &str) -> Option<&ResidentCheckpointComponent> {
        self.entries.get(key)
    }

    fn component_ref(&self, key: &str) -> Option<&crate::store::BlobRef> {
        self.component(key)
            .and_then(|component| component.descriptor.as_ref())
            .map(|descriptor| &descriptor.blob_ref)
    }

    fn tool_state_snapshot(&self) -> Option<&crate::ToolState> {
        match self.component(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT) {
            Some(ResidentCheckpointComponent {
                body: ResidentCheckpointComponentBody::ToolState { snapshot, .. },
                ..
            }) => snapshot.as_ref(),
            _ => None,
        }
    }

    fn tool_state_generation(&self) -> Option<u64> {
        match self.component(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT) {
            Some(ResidentCheckpointComponent {
                body: ResidentCheckpointComponentBody::ToolState { generation, .. },
                ..
            }) => *generation,
            _ => None,
        }
    }

    fn set_tool_state_snapshot(&mut self, snapshot: Option<crate::ToolState>) {
        let key = crate::store::TOOL_STATE_CHECKPOINT_COMPONENT.to_string();
        let Some(snapshot) = snapshot else {
            self.entries.remove(&key);
            return;
        };
        let generation = Some(snapshot.generation());
        let descriptor = self
            .entries
            .get(&key)
            .and_then(|entry| entry.descriptor.clone());
        self.entries.insert(
            key,
            ResidentCheckpointComponent {
                descriptor,
                body: ResidentCheckpointComponentBody::ToolState {
                    snapshot: Some(snapshot),
                    generation,
                },
                dirty: true,
            },
        );
    }

    fn plugin_snapshot(&self) -> Option<&crate::PluginSessionSnapshot> {
        match self.component(crate::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT) {
            Some(ResidentCheckpointComponent {
                body: ResidentCheckpointComponentBody::PluginSnapshot(snapshot),
                ..
            }) => snapshot.as_ref(),
            _ => None,
        }
    }

    fn set_plugin_snapshot(&mut self, snapshot: Option<crate::PluginSessionSnapshot>) {
        let key = crate::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT.to_string();
        let Some(snapshot) = snapshot else {
            self.entries.remove(&key);
            return;
        };
        let descriptor = self
            .entries
            .get(&key)
            .and_then(|entry| entry.descriptor.clone());
        self.entries.insert(
            key,
            ResidentCheckpointComponent {
                descriptor,
                body: ResidentCheckpointComponentBody::PluginSnapshot(Some(snapshot)),
                dirty: true,
            },
        );
    }

    fn execution_state_snapshot(&self) -> Option<&[u8]> {
        match self.component(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT) {
            Some(ResidentCheckpointComponent {
                body: ResidentCheckpointComponentBody::ExecutionState(snapshot),
                ..
            }) => snapshot.as_deref(),
            _ => None,
        }
    }

    fn set_execution_state_snapshot(&mut self, snapshot: Option<Vec<u8>>) {
        let key = crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string();
        let Some(snapshot) = snapshot else {
            self.entries.remove(&key);
            return;
        };
        let descriptor = self
            .entries
            .get(&key)
            .and_then(|entry| entry.descriptor.clone());
        self.entries.insert(
            key,
            ResidentCheckpointComponent {
                descriptor,
                body: ResidentCheckpointComponentBody::ExecutionState(Some(snapshot)),
                dirty: true,
            },
        );
    }

    fn discard_known_bodies(&mut self) {
        for component in self.entries.values_mut() {
            match &mut component.body {
                ResidentCheckpointComponentBody::ToolState { snapshot, .. } => *snapshot = None,
                ResidentCheckpointComponentBody::PluginSnapshot(snapshot) => *snapshot = None,
                ResidentCheckpointComponentBody::ExecutionState(snapshot) => *snapshot = None,
                ResidentCheckpointComponentBody::Opaque(_) => {}
            }
        }
    }

    fn adopt_manifest(&mut self, manifest: &crate::store::SessionCheckpoint) {
        self.entries
            .retain(|key, _| manifest.components.contains_key(key));
        for (key, descriptor) in &manifest.components {
            if let Some(component) = self.entries.get_mut(key) {
                component.descriptor = Some(descriptor.clone());
                component.dirty = false;
            } else {
                self.entries.insert(
                    key.clone(),
                    ResidentCheckpointComponent {
                        descriptor: Some(descriptor.clone()),
                        body: ResidentCheckpointComponentBody::Opaque(None),
                        dirty: false,
                    },
                );
            }
        }
        self.completeness = CheckpointComponentCompleteness::Complete;
    }
}

/// The runtime's view of a session: the persistable snapshot fields
/// **plus** scratch fields the runtime tracks but never persists
/// (head-revision CAS guard, pending dirty-write buffers, graph-flush
/// flag). Public serialization goes through [`RuntimeSessionState::to_snapshot`],
/// which drops runtime-only fields by construction.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeSessionState {
    pub session_id: String,
    pub policy: SessionPolicy,
    /// Derived cache of FrameOpen nodes; never serialized or persisted.
    #[serde(skip)]
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<String>,
    #[serde(default)]
    pub session_graph: crate::SessionGraph,
    #[serde(default)]
    pub turn_index: usize,
    #[serde(default)]
    pub token_usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_usage: Option<PromptUsage>,
    #[serde(default)]
    pub protocol_turn_options: crate::ProtocolTurnOptions,
    #[serde(skip, default)]
    #[doc(hidden)]
    pub checkpoint_components: RuntimeCheckpointComponents,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_revision: Option<u64>,
    /// Cost-accounting ledger. Every LLM call (parent turns, subagent
    /// children, compaction, observers, background helpers) contributes an
    /// entry keyed by `(source, model)`. Separate from `token_usage`
    /// which tracks context-window accounting only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<TokenLedgerEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<crate::store::BlobRef>,
    /// Store head revision observed by the runtime. Revision zero is the
    /// create/fork baseline; `checkpoint_ref` distinguishes an unpersisted
    /// empty runtime from a durable revision-zero fork.
    #[serde(skip)]
    pub head_revision: u64,
    /// Node ids known to exist durably. This is deliberately independent of
    /// the resident graph: partial residency omits durable off-path nodes,
    /// while host-side edits can add resident nodes before they commit.
    #[serde(skip)]
    #[doc(hidden)]
    pub persisted_node_ids: std::collections::HashSet<String>,
}

impl RuntimeSessionState {
    /// Construct empty runtime state with an explicitly chosen session policy.
    pub fn new(policy: SessionPolicy) -> Self {
        Self {
            session_id: "root".to_string(),
            policy,
            agent_frames: Vec::new(),
            current_frame_node_id: None,
            session_graph: crate::SessionGraph::default(),
            turn_index: 0,
            token_usage: TokenUsage::default(),
            last_prompt_usage: None,
            protocol_turn_options: crate::ProtocolTurnOptions::default(),
            checkpoint_components: RuntimeCheckpointComponents::complete_empty(),
            plugin_snapshot_revision: None,
            token_ledger: Vec::new(),
            checkpoint_ref: None,
            head_revision: 0,
            persisted_node_ids: std::collections::HashSet::new(),
        }
    }

    /// Builds a `RuntimeSessionState` from snapshot data for protocol and process-engine
    /// implementors while materializing or restoring protocol session state.
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Self {
        let checkpoint_components = RuntimeCheckpointComponents::from_snapshot(&snapshot);
        let agent_frames = snapshot
            .session_graph
            .agent_frame_records(&snapshot.session_id);
        let mut state = Self {
            session_id: snapshot.session_id,
            policy: snapshot.policy,
            agent_frames,
            current_frame_node_id: snapshot.current_frame_node_id,
            session_graph: snapshot.session_graph,
            turn_index: snapshot.turn_index,
            token_usage: snapshot.token_usage,
            last_prompt_usage: snapshot.last_prompt_usage,
            protocol_turn_options: snapshot.protocol_turn_options,
            checkpoint_components,
            plugin_snapshot_revision: snapshot.plugin_snapshot_revision,
            token_ledger: snapshot.token_ledger,
            checkpoint_ref: snapshot.checkpoint_ref,
            head_revision: 0,
            persisted_node_ids: std::collections::HashSet::new(),
        };
        state.ensure_agent_frame_initialized();
        state
    }

    /// Projects this `RuntimeSessionState` into snapshot form for protocol and process-engine
    /// implementors while materializing or restoring protocol session state.
    pub fn to_snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.session_id.clone(),
            policy: self.policy.clone(),
            agent_frames: self.session_graph.agent_frame_records(&self.session_id),
            current_frame_node_id: self.current_frame_node_id.clone(),
            session_graph: self.session_graph.clone(),
            turn_index: self.turn_index,
            token_usage: self.token_usage.clone(),
            last_prompt_usage: self.last_prompt_usage.clone(),
            protocol_turn_options: self.protocol_turn_options.clone(),
            tool_state_ref: self.tool_state_ref().cloned(),
            tool_state_generation: self.tool_state_generation(),
            plugin_snapshot_ref: self.plugin_snapshot_ref().cloned(),
            plugin_snapshot_revision: self.plugin_snapshot_revision,
            execution_state_ref: self.execution_state_ref().cloned(),
            token_ledger: self.token_ledger.clone(),
            checkpoint_ref: self.checkpoint_ref.clone(),
        }
    }

    /// Updates protocol-visible snapshot state while retaining the resident complete checkpoint
    /// component set. `SessionSnapshot` is only a well-known-key projection and therefore cannot
    /// replace the authoritative runtime-only component listing.
    pub fn apply_snapshot(&mut self, snapshot: &SessionSnapshot) {
        self.session_id = snapshot.session_id.clone();
        self.policy = snapshot.policy.clone();
        self.session_graph = snapshot.session_graph.clone();
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
        self.current_frame_node_id = snapshot.current_frame_node_id.clone();
        self.ensure_agent_frame_initialized();
        self.turn_index = snapshot.turn_index;
        self.token_usage = snapshot.token_usage.clone();
        self.last_prompt_usage = snapshot.last_prompt_usage.clone();
        self.protocol_turn_options = snapshot.protocol_turn_options.clone();
        self.plugin_snapshot_revision = snapshot.plugin_snapshot_revision;
        self.token_ledger = snapshot.token_ledger.clone();
        self.checkpoint_ref = snapshot.checkpoint_ref.clone();
    }

    /// Folds durable token-ledger entries into a per-source report for protocol and administration
    /// embedders without mutating the ledger.
    pub fn usage_report(&self) -> super::usage::SessionUsageReport {
        super::usage::SessionUsageReport::from_entries(&self.token_ledger)
    }

    pub(crate) fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.current_frame_node_id.as_deref().map_or_else(
            || self.session_graph.read_model(),
            |frame_node_id| self.session_graph.read_model_for_frame(frame_node_id),
        )
    }

    /// Replaces the current frame's readable message tail for protocol implementors restoring
    /// state; transient messages are excluded and the frame projection is refreshed.
    pub fn replace_active_read_state(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        if let Some(frame_node_id) = self.current_frame_node_id.as_deref() {
            self.session_graph
                .rewrite_active_read_tail_for_frame(frame_node_id, messages);
        } else {
            self.session_graph.rewrite_active_read_tail(messages);
        }
        self.refresh_current_frame_projection();
    }

    /// Appends non-transient messages in source order for protocol implementors restoring an
    /// incremental session delta, then refreshes the current frame projection.
    pub fn append_active_read_delta(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_active_read_delta(messages);
        self.refresh_current_frame_projection();
    }

    /// Appends non-transient conversation messages in source order for protocol implementors and
    /// refreshes the current frame projection.
    pub fn append_active_conversation_messages(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_active_read_delta(messages);
        self.refresh_current_frame_projection();
    }

    pub(crate) fn append_active_conversation_messages_with_clock(
        &mut self,
        messages: &[Message],
        clock: &dyn crate::Clock,
    ) {
        self.ensure_agent_frame_initialized_with_clock(clock);
        self.session_graph
            .append_active_conversation_messages_at(messages, clock.timestamp_rfc3339());
        self.refresh_current_frame_projection();
    }

    /// Exposes read view to protocol and process-engine implementors while materializing or
    /// restoring protocol session state.
    pub fn read_view(&self) -> crate::SessionReadView {
        crate::SessionReadView::from_persisted_state(self)
    }

    /// Exposes session graph to protocol and process-engine implementors while materializing or
    /// restoring protocol session state.
    pub fn session_graph(&self) -> &crate::SessionGraph {
        &self.session_graph
    }

    /// Exposes policy to protocol and process-engine implementors while materializing or restoring
    /// protocol session state.
    pub fn policy(&self) -> &SessionPolicy {
        self.effective_policy()
    }

    /// Durable reference for the well-known tool-state component.
    pub fn tool_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
    }

    /// Generation carried by the typed tool-state view.
    pub fn tool_state_generation(&self) -> Option<u64> {
        self.checkpoint_components.tool_state_generation()
    }

    /// Typed resident view of the well-known tool-state component.
    pub fn tool_state_snapshot(&self) -> Option<&crate::ToolState> {
        self.checkpoint_components.tool_state_snapshot()
    }

    /// Replace or explicitly delete the well-known tool-state component.
    pub fn set_tool_state_snapshot(&mut self, snapshot: Option<crate::ToolState>) {
        self.checkpoint_components.set_tool_state_snapshot(snapshot);
    }

    /// Durable reference for the well-known plugin-snapshot component.
    pub fn plugin_snapshot_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT)
    }

    /// Typed resident view of the well-known plugin-snapshot component.
    pub fn plugin_snapshot(&self) -> Option<&crate::PluginSessionSnapshot> {
        self.checkpoint_components.plugin_snapshot()
    }

    /// Replace or explicitly delete the well-known plugin-snapshot component.
    pub fn set_plugin_snapshot(&mut self, snapshot: Option<crate::PluginSessionSnapshot>) {
        self.checkpoint_components.set_plugin_snapshot(snapshot);
    }

    /// Durable reference for the well-known execution-state component.
    pub fn execution_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
    }

    /// Advances resident state to a store's committed head revision and realized timestamps, adopts
    /// durable artifact references, and clears transient snapshots so protocol and store
    /// implementors cannot reuse stale bytes.
    pub fn apply_persisted_commit_result(&mut self, result: crate::store::RuntimeCommitResult) {
        self.head_revision = result.head_revision;
        self.checkpoint_ref = Some(result.checkpoint_ref);
        self.session_graph
            .apply_realized_node_timestamps(&result.realized_node_timestamps);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
        self.plugin_snapshot_revision = result.manifest.plugin_snapshot_revision;
        self.checkpoint_components.adopt_manifest(&result.manifest);
        self.checkpoint_components.discard_known_bodies();
    }

    pub(crate) fn pending_graph_commit(&self) -> crate::GraphAppend {
        let nodes = self
            .session_graph
            .nodes
            .iter()
            .filter(|node| !self.persisted_node_ids.contains(&node.node_id))
            .cloned()
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            crate::GraphAppend {
                nodes: Vec::new(),
                leaf_node_id: self.session_graph.leaf_node_id.clone(),
            }
        } else {
            crate::GraphAppend {
                nodes,
                leaf_node_id: self.session_graph.leaf_node_id.clone(),
            }
        }
    }

    pub(crate) fn mark_node_ids_persisted<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.persisted_node_ids.extend(node_ids);
    }

    /// Clears in-memory tool, plugin, and execution-state snapshots for protocol implementors after
    /// their durable references have become authoritative.
    pub fn discard_runtime_snapshots(&mut self) {
        self.checkpoint_components.discard_known_bodies();
    }

    /// Updates execution state snapshot state for protocol and process-engine implementors while
    /// materializing or restoring protocol session state.
    pub fn set_execution_state_snapshot(&mut self, execution_state_snapshot: Option<Vec<u8>>) {
        // A materialized frame-switch outcome passes `None` here to clear the checkpoint. Clear
        // the durable ref with the resident body: every store interprets an absent body with a
        // present ref as an unchanged component, which would otherwise restore the old frame.
        self.checkpoint_components
            .set_execution_state_snapshot(execution_state_snapshot);
    }

    /// Exposes execution state snapshot to protocol and process-engine implementors while
    /// materializing or restoring protocol session state. Returns `None` when no execution state
    /// snapshot is present.
    pub fn execution_state_snapshot(&self) -> Option<&[u8]> {
        self.checkpoint_components.execution_state_snapshot()
    }

    /// Updates plugin snapshots state for protocol and process-engine implementors while
    /// materializing or restoring protocol session state.
    pub fn refresh_plugin_snapshots(&mut self, plugins: &crate::PluginSession) {
        let tool_registry = plugins.tool_registry();
        let generation = tool_registry.generation();
        if self.tool_state_ref().is_none() || self.tool_state_generation() != Some(generation) {
            let snapshot = tool_registry.export_state();
            self.set_tool_state_snapshot(Some(snapshot));
        }

        let revision = plugins.snapshot_revision_fingerprint();
        if self.plugin_snapshot_ref().is_none() || self.plugin_snapshot_revision != Some(revision) {
            store_plugin_snapshot(self, plugins.snapshot());
        }
        self.plugin_snapshot_revision = Some(revision);
    }
}

/// Persist a freshly captured plugin snapshot, logging and **retaining the prior
/// snapshot** when the capture fails.
///
/// A failed capture (`Err`) previously collapsed to `None` via `.ok()`, erasing
/// the last good snapshot — so the next cold rebuild would restore an empty
/// plugin surface even though a valid snapshot had been captured earlier. Keep
/// the prior value and surface the error instead.
pub(crate) fn store_plugin_snapshot(
    target: &mut RuntimeSessionState,
    captured: Result<crate::PluginSessionSnapshot, crate::PluginError>,
) {
    match captured {
        Ok(snapshot) => target.set_plugin_snapshot(Some(snapshot)),
        Err(err) => tracing::warn!(
            error = %err,
            "failed to capture plugin snapshot; retaining the prior snapshot",
        ),
    }
}

impl RuntimeSessionState {
    pub(crate) fn refresh_current_frame_projection(&mut self) {
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(str::to_string);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    /// Exposes current agent frame to protocol and process-engine implementors while materializing
    /// or restoring protocol session state. Returns `None` when no current agent frame is present.
    pub fn current_agent_frame(&self) -> Option<&crate::AgentFrameRecord> {
        self.agent_frames.iter().find(|frame| {
            Some(frame.frame_node_id.as_str()) == self.current_frame_node_id.as_deref()
        })
    }

    /// Exposes effective policy to protocol and process-engine implementors while materializing or
    /// restoring protocol session state.
    pub fn effective_policy(&self) -> &SessionPolicy {
        &self.policy
    }

    /// Exposes the protocol options captured for the current agent frame so protocol implementors
    /// restore the frame's durable turn configuration.
    pub fn effective_protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        &self.protocol_turn_options
    }

    /// Ensures protocol implementors restoring legacy state have a canonical initial agent frame
    /// before reading or mutating frame-scoped history.
    pub fn ensure_agent_frame_initialized(&mut self) {
        self.ensure_agent_frame_initialized_with_clock(&crate::SystemClock);
    }

    /// Ensures agent frame initialized with clock exists for protocol and process-engine
    /// implementors while materializing or restoring protocol session state.
    pub fn ensure_agent_frame_initialized_with_clock(&mut self, clock: &dyn crate::Clock) {
        if let Some(frame_node_id) = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
        {
            self.current_frame_node_id = Some(frame_node_id.to_string());
            self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
            return;
        }
        if self.session_graph.leaf_node_id.is_some() {
            self.current_frame_node_id = None;
            self.agent_frames.clear();
            return;
        }
        let assignment = crate::AgentFrameAssignment::from_policy(self.policy.clone());
        let frame_key = "initial-frame";
        let frame_node_id = crate::session_graph::frame_node_id(&self.session_id, frame_key);
        self.session_graph.append_frame_open_with_id_at(
            frame_node_id.clone(),
            frame_key.to_string(),
            crate::AgentFrameReason::initial(),
            assignment,
            self.protocol_turn_options.clone(),
            clock.timestamp_rfc3339(),
        );
        self.current_frame_node_id = Some(frame_node_id);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    /// Resets initial agent frame with clock for protocol and process-engine implementors while
    /// materializing or restoring protocol session state.
    pub fn reset_initial_agent_frame_with_clock(
        &mut self,
        assignment: crate::AgentFrameAssignment,
        protocol_turn_options: crate::ProtocolTurnOptions,
        clock: &dyn crate::Clock,
    ) {
        self.policy = assignment.policy.clone();
        self.protocol_turn_options = protocol_turn_options.clone();
        let frame_key = "initial-frame";
        let frame_node_id = crate::session_graph::frame_node_id(&self.session_id, frame_key);
        self.session_graph.append_frame_open_with_id_at(
            frame_node_id.clone(),
            frame_key.to_string(),
            crate::AgentFrameReason::initial(),
            assignment,
            protocol_turn_options,
            clock.timestamp_rfc3339(),
        );
        self.current_frame_node_id = Some(frame_node_id);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }
}

pub(crate) mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`RuntimeSessionState`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait RuntimeSessionStateFacadeOps {
        // APIT is intentionally non-dyn-compatible; this trait has one static-dispatch impl.
        fn empty_for(session_id: impl Into<String>, policy: SessionPolicy) -> RuntimeSessionState;

        fn turn_state(&self) -> PersistedTurnState;

        // APIT is intentionally non-dyn-compatible; this trait has one static-dispatch impl.
        fn turn_scope(&self, turn_id: impl Into<String>) -> crate::ExecutionScope;

        // APIT is intentionally non-dyn-compatible; this trait has one static-dispatch impl.
        fn queue_drain_scope(&self, drain_id: impl Into<String>) -> crate::ExecutionScope;

        fn process_execution_env_spec(
            &self,
            fallback_policy: &SessionPolicy,
        ) -> crate::ProcessExecutionEnvSpec;
    }

    impl RuntimeSessionStateFacadeOps for RuntimeSessionState {
        fn empty_for(session_id: impl Into<String>, policy: SessionPolicy) -> RuntimeSessionState {
            RuntimeSessionState {
                session_id: session_id.into(),
                ..RuntimeSessionState::new(policy)
            }
        }

        fn turn_state(&self) -> PersistedTurnState {
            PersistedTurnState {
                turn_index: self.turn_index,
                token_usage: self.token_usage.clone(),
                last_prompt_usage: self.last_prompt_usage.clone(),
                protocol_turn_options: self.protocol_turn_options.clone(),
            }
        }

        fn turn_scope(&self, turn_id: impl Into<String>) -> crate::ExecutionScope {
            crate::ExecutionScope::turn(&self.session_id, turn_id)
        }

        fn queue_drain_scope(&self, drain_id: impl Into<String>) -> crate::ExecutionScope {
            crate::ExecutionScope::queue_drain(&self.session_id, drain_id)
        }

        fn process_execution_env_spec(
            &self,
            fallback_policy: &SessionPolicy,
        ) -> crate::ProcessExecutionEnvSpec {
            self.current_agent_frame()
                .map(|frame| {
                    crate::ProcessExecutionEnvSpec::new(
                        frame.assignment.plugin_options.clone(),
                        self.policy.clone(),
                    )
                })
                .unwrap_or_else(|| {
                    crate::ProcessExecutionEnvSpec::new(
                        crate::PluginOptions::default(),
                        fallback_policy.clone(),
                    )
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sansio::sync::MutexExt;

    #[test]
    fn commit_operation_identity_depends_on_caller_boundary_not_head_revision() {
        let first = boundary_operation("session", "request-42", "append-session-nodes");
        let retry = boundary_operation("session", "request-42", "append-session-nodes");
        let next = boundary_operation("session", "request-43", "append-session-nodes");

        assert_eq!(first, retry);
        assert_ne!(first, next);
    }
    use std::sync::{Arc, Mutex};

    struct DynamicSnapshotTools {
        names: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for DynamicSnapshotTools {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            self.names
                .lock_recover()
                .iter()
                .map(|name| {
                    crate::ToolDefinition::raw(
                        format!("tool:{name}"),
                        name,
                        "dynamic snapshot tool",
                        crate::ToolDefinition::default_input_schema(),
                        serde_json::json!({}),
                    )
                    .manifest()
                })
                .collect()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            self.names
                .lock_recover()
                .iter()
                .any(|candidate| candidate == name)
                .then(|| {
                    Arc::new(
                        crate::ToolDefinition::raw(
                            format!("tool:{name}"),
                            name,
                            "dynamic snapshot tool",
                            crate::ToolDefinition::default_input_schema(),
                            serde_json::json!({}),
                        )
                        .contract(),
                    )
                })
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
            crate::ToolResult::ok(serde_json::json!("ok"))
        }
    }

    #[test]
    fn session_snapshot_serialization_excludes_runtime_only_fields_and_round_trips() {
        let mut state = RuntimeSessionState {
            session_id: "snapshot-test".to_string(),
            policy: SessionPolicy {
                provider_id: "mock".to_string(),
                ..SessionPolicy::new(crate::TurnBudget::Unbounded)
            },
            head_revision: 42,
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        state.set_tool_state_snapshot(Some(crate::ToolState::default()));
        state.set_plugin_snapshot(Some(crate::PluginSessionSnapshot::default()));
        state.set_execution_state_snapshot(Some(vec![1, 2, 3]));
        state.ensure_agent_frame_initialized();

        let value = serde_json::to_value(state.to_snapshot()).expect("serialize snapshot");

        for runtime_key in [
            "head_revision",
            "persisted_node_ids",
            "tool_state_snapshot",
            "plugin_snapshot",
            "execution_state_snapshot",
        ] {
            assert!(
                value.get(runtime_key).is_none(),
                "snapshot unexpectedly exposed {runtime_key}"
            );
        }
        assert!(value.get("agent_frames").is_none());

        let snapshot: SessionSnapshot = serde_json::from_value(value).expect("round-trip snapshot");
        let hydrated = RuntimeSessionState::from_snapshot(snapshot);

        assert_eq!(hydrated.session_id, "snapshot-test");
        assert_eq!(hydrated.policy.recorded_provider_id(), "mock");
        assert_eq!(hydrated.head_revision, 0);
        assert!(hydrated.tool_state_snapshot().is_none());
        assert!(hydrated.plugin_snapshot().is_none());
        assert!(hydrated.execution_state_snapshot().is_none());
        assert!(!hydrated.agent_frames.is_empty());
    }

    #[test]
    fn reconciled_generation_forces_next_plugin_snapshot_export() {
        let names = Arc::new(Mutex::new(vec!["dynamic_one".to_string()]));
        let tools: Arc<dyn crate::ToolProvider> = Arc::new(DynamicSnapshotTools {
            names: Arc::clone(&names),
        });
        let plugins = crate::runtime::tests::helpers::plugin_session_with_tools("root", tools);
        let snapshot = plugins.tool_registry().export_state();
        let persisted_generation = snapshot.generation();
        let mut projected =
            RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
                .to_snapshot();
        projected.tool_state_ref = Some("persisted-tool-state".to_string().into());
        projected.tool_state_generation = Some(persisted_generation);
        let mut state = RuntimeSessionState::from_snapshot(projected);

        names.lock_recover().push("dynamic_two".to_string());
        let report = plugins
            .tool_registry()
            .restore_state(snapshot)
            .expect("live surface restore");
        assert_eq!(report.generation, persisted_generation + 1);

        state.refresh_plugin_snapshots(&plugins);
        let refreshed = state
            .tool_state_snapshot()
            .expect("generation change re-exports the tool snapshot");
        assert_eq!(refreshed.generation(), report.generation);
        assert!(refreshed.contains(&crate::ToolId::from("tool:dynamic_two")));
    }

    #[test]
    fn incomplete_checkpoint_component_projection_is_a_typed_error() {
        let projected =
            RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
                .to_snapshot();
        let state = RuntimeSessionState::from_snapshot(projected);

        let error = state
            .checkpoint_components
            .build_checkpoint(crate::PersistedTurnState::default(), None)
            .expect_err("snapshot projection cannot prove the complete keyed set");

        assert!(matches!(
            error,
            crate::StoreError::IncompleteCheckpointComponentSet
        ));
    }

    #[test]
    fn new_session_rejects_unproven_checkpoint_component_projection() {
        let projected =
            RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
                .to_snapshot();
        let state = RuntimeSessionState::from_snapshot(projected);

        let error = state
            .checkpoint_components
            .complete_for_new_session()
            .expect_err("a public projection cannot prove a complete new-session root");

        assert!(matches!(
            error,
            crate::StoreError::IncompleteCheckpointComponentSet
        ));
    }
}

pub(super) fn apply_persisted_session_config(
    policy: &mut SessionPolicy,
    config: &crate::PersistedSessionConfig,
) {
    policy.model = config.model.clone();
    policy.provider_id = config.provider_id.clone();
}

/// Restore-time headroom shared by every bare next-turn `turn_index + 1`.
///
/// All production increment sites reference this invariant. A durable value is
/// admitted only through [`apply_session_checkpoint`], which reserves enough
/// room for every bounded increment performed before the next commit.
pub(super) const RESTORED_TURN_INDEX_HEADROOM: usize = 16;
const MAX_EXCLUSIVE_RESTORED_TURN_INDEX: usize = usize::MAX - RESTORED_TURN_INDEX_HEADROOM;

fn validate_restored_turn_index(turn_index: usize) -> Result<(), crate::StoreError> {
    if turn_index >= MAX_EXCLUSIVE_RESTORED_TURN_INDEX {
        return Err(crate::StoreError::CheckpointTurnIndexOutOfRange {
            turn_index,
            max_exclusive: MAX_EXCLUSIVE_RESTORED_TURN_INDEX,
        });
    }
    Ok(())
}

/// Admits durable turn usage before the runtime adopts it.
///
/// Restored usage feeds bare aggregations — `TokenUsage::total` in protocol
/// budget policy, `input_total` in context-window policy — and it is the base
/// the next turn's checked merge accumulates onto. Validating both aggregations
/// once here keeps every one of those sites safe by invariant, the same
/// contract [`validate_restored_turn_index`] gives the bare next-turn
/// increments.
fn validate_restored_token_usage(usage: &TokenUsage) -> Result<(), crate::StoreError> {
    let checkpoint_overflow = |overflow: lash_sansio::session_model::TokenUsageOverflow| {
        crate::StoreError::CheckpointTokenUsageOutOfRange {
            counter: overflow.counter(),
        }
    };
    usage.checked_total().map_err(checkpoint_overflow)?;
    usage.checked_input_total().map_err(checkpoint_overflow)?;
    Ok(())
}

pub(crate) fn apply_session_checkpoint(
    state: &mut RuntimeSessionState,
    checkpoint: Option<crate::store::HydratedSessionCheckpoint>,
) -> Result<(), crate::StoreError> {
    let Some(checkpoint) = checkpoint else {
        state.checkpoint_components = RuntimeCheckpointComponents::complete_empty();
        state.plugin_snapshot_revision = None;
        state.ensure_agent_frame_initialized();
        return Ok(());
    };
    // All production next-turn sites rely on RESTORED_TURN_INDEX_HEADROOM, and
    // every usage consumer relies on the restored counters aggregating in
    // range. Validate both durable values once before adopting them.
    validate_restored_turn_index(checkpoint.turn_state.turn_index)?;
    validate_restored_token_usage(&checkpoint.turn_state.token_usage)?;
    state.turn_index = checkpoint.turn_state.turn_index;
    state.token_usage = checkpoint.turn_state.token_usage.clone();
    state.last_prompt_usage = checkpoint.turn_state.last_prompt_usage.clone();
    state.protocol_turn_options = checkpoint.turn_state.protocol_turn_options.clone();
    state.plugin_snapshot_revision = checkpoint.plugin_snapshot_revision;
    state.checkpoint_components = RuntimeCheckpointComponents::from_hydrated(&checkpoint)?;
    state.ensure_agent_frame_initialized();
    Ok(())
}

pub(super) fn apply_session_head(
    state: &mut RuntimeSessionState,
    head: &crate::store::SessionHead,
) {
    state.session_graph = head.graph.clone();
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    state.current_frame_node_id = head.current_frame_node_id.clone();
    state.checkpoint_ref = head.checkpoint_ref.clone();
    state.token_ledger = head.token_ledger.clone();
    state.checkpoint_components = if head.checkpoint_ref.is_some() {
        RuntimeCheckpointComponents::unproven()
    } else {
        RuntimeCheckpointComponents::complete_empty()
    };
    state.plugin_snapshot_revision = None;
    state.ensure_agent_frame_initialized();
    state.head_revision = head.head_revision;
    state.persisted_node_ids = head
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect();
    apply_persisted_session_config(&mut state.policy, &head.config);
}

pub(crate) fn append_session_nodes_to_state_with_clock(
    state: &mut RuntimeSessionState,
    nodes: &[crate::SessionAppendNode],
    draft_namespace: &str,
    clock: &dyn crate::Clock,
) -> Vec<String> {
    let drafts = nodes
        .iter()
        .enumerate()
        .map(|(ordinal, node)| {
            let fallback_digest =
                crate::stable_hash::sha256_hex(format!("{draft_namespace}:{ordinal}").as_bytes());
            session_append_node_draft(node, &format!("m_append_{fallback_digest}"))
        })
        .collect::<Vec<_>>();
    state.ensure_agent_frame_initialized_with_clock(clock);
    state
        .session_graph
        .append_node_drafts_at(draft_namespace, drafts, clock.timestamp_rfc3339())
}

pub(crate) fn boundary_operation(
    session_id: &str,
    boundary_id: &str,
    key: impl Into<String>,
) -> crate::OperationId {
    crate::OperationId::new(
        crate::ExecutionScope::runtime_operation(format!(
            "session:{session_id}:boundary:{boundary_id}"
        )),
        key,
    )
}

pub(crate) fn derive_graph_commit_node_ids(
    state: &mut RuntimeSessionState,
    graph: &mut crate::GraphAppend,
    operation: &crate::OperationId,
) -> Result<Vec<String>, crate::StoreError> {
    let mapping = graph.derive_node_ids(&state.session_id, operation)?;
    apply_graph_commit_node_id_mapping(state, &mapping)?;
    Ok(mapping.into_iter().map(|(_, derived)| derived).collect())
}

pub(crate) fn apply_graph_commit_node_id_mapping(
    state: &mut RuntimeSessionState,
    mapping: &[(String, String)],
) -> Result<(), crate::StoreError> {
    state
        .session_graph
        .remap_node_ids(&state.session_id, mapping);
    if let Some(current) = state.current_frame_node_id.as_mut()
        && let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current)
    {
        *current = derived.clone();
    }
    state.agent_frames = state
        .session_graph
        .try_agent_frame_records(&state.session_id)?;
    Ok(())
}

pub(crate) fn receipt_append_node_ids(
    result: &crate::store::RuntimeCommitResult,
    requested_node_count: usize,
) -> Result<Vec<String>, crate::StoreError> {
    if result.realized_node_timestamps.len() < requested_node_count {
        return Err(crate::StoreError::Backend(format!(
            "append receipt returned {} realized node timestamps for {requested_node_count} requested nodes",
            result.realized_node_timestamps.len()
        )));
    }
    Ok(result.realized_node_timestamps
        [result.realized_node_timestamps.len() - requested_node_count..]
        .iter()
        .map(|realized| realized.node_id.clone())
        .collect())
}

pub(super) fn open_agent_frame_in_state_with_clock(
    state: &mut RuntimeSessionState,
    request: crate::OpenAgentFrameRequest,
    clock: &dyn crate::Clock,
) -> crate::OpenAgentFrameResult {
    state.ensure_agent_frame_initialized_with_clock(clock);
    if request.frame_id.trim().is_empty() {
        return crate::OpenAgentFrameResult {
            frame_node_id: state.current_frame_node_id.clone().unwrap_or_default(),
            opened: false,
            initial_node_ids: Vec::new(),
        };
    }

    let previous = state.current_agent_frame().cloned();
    let mut assignment = previous
        .as_ref()
        .map(|frame| frame.assignment.clone())
        .unwrap_or_else(|| crate::AgentFrameAssignment::from_policy(state.policy.clone()));
    assignment.policy = state.policy.clone();
    let protocol_turn_options = state.protocol_turn_options.clone();
    let frame_node_id = crate::session_graph::frame_node_id(&state.session_id, &request.frame_id);
    let opened = state.session_graph.append_frame_open_with_id_at(
        frame_node_id.clone(),
        request.frame_id.clone(),
        request.reason,
        assignment,
        protocol_turn_options,
        clock.timestamp_rfc3339(),
    );
    if !opened {
        if state.current_frame_node_id.as_deref() == Some(frame_node_id.as_str()) {
            return crate::OpenAgentFrameResult {
                frame_node_id,
                opened: false,
                initial_node_ids: Vec::new(),
            };
        }
        state
            .session_graph
            .set_leaf_node_id(Some(frame_node_id.clone()));
    }
    state.current_frame_node_id = Some(frame_node_id);
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    if let Some((policy, protocol_turn_options)) = state.current_agent_frame().map(|frame| {
        (
            frame.assignment.policy.clone(),
            frame.protocol_turn_options.clone(),
        )
    }) {
        state.policy = policy;
        state.protocol_turn_options = protocol_turn_options;
    }

    let initial_node_ids = append_session_nodes_to_state_with_clock(
        state,
        &request.initial_nodes,
        &request.frame_id,
        clock,
    );
    crate::OpenAgentFrameResult {
        frame_node_id: state.current_frame_node_id.clone().unwrap_or_default(),
        opened: true,
        initial_node_ids,
    }
}

fn session_append_node_draft(
    node: &crate::SessionAppendNode,
    fallback_message_id: &str,
) -> crate::session_graph::SessionNodeDraft {
    match node {
        crate::SessionAppendNode::Message { message } => {
            crate::session_graph::SessionNodeDraft::message(plugin_message_to_message(
                message,
                fallback_message_id,
            ))
        }
        crate::SessionAppendNode::ProtocolEvent { event } => {
            crate::session_graph::SessionNodeDraft::protocol_event(event.clone())
        }
        crate::SessionAppendNode::Plugin { plugin_type, body } => {
            crate::session_graph::SessionNodeDraft::plugin(plugin_type.clone(), body.clone())
        }
    }
}

#[cfg(test)]
mod plugin_snapshot_tests {
    use super::store_plugin_snapshot;
    use crate::{PluginError, PluginSessionSnapshot, RuntimeSessionState};

    fn state() -> RuntimeSessionState {
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    }

    #[test]
    fn ok_capture_overwrites_target() {
        let mut target = state();
        store_plugin_snapshot(&mut target, Ok(PluginSessionSnapshot::default()));
        assert!(
            target.plugin_snapshot().is_some(),
            "a successful capture must be stored"
        );
    }

    #[test]
    fn failed_capture_retains_prior_snapshot() {
        // The regression this guards: a failed snapshot capture used to collapse
        // to `None` via `.ok()`, erasing the last good snapshot so the next cold
        // rebuild would restore an empty plugin surface. A failure must leave the
        // prior snapshot intact.
        let mut target = state();
        target.set_plugin_snapshot(Some(PluginSessionSnapshot::default()));
        store_plugin_snapshot(
            &mut target,
            Err(PluginError::Snapshot("capture failed".to_string())),
        );
        assert!(
            target.plugin_snapshot().is_some(),
            "a failed capture must retain the prior snapshot, not erase it"
        );
    }
}
