//! Runtime session state and persistence helpers.
//!
//! `RuntimeSessionState` is the runtime-private mutable state shape. Public
//! host/plugin reads use `SessionSnapshot` from the plugin API instead.

use crate::SessionId;
use crate::TurnId;
use crate::facade_support::{SessionGraphFacadeOps, ToolStateFacadeOps};

use crate::session_model::{Message, SessionPolicy, TokenUsage, plugin_message_to_message};
use crate::{PersistedTurnState, SessionSnapshot};

use super::usage::TokenLedgerEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
enum CheckpointComponentCompleteness {
    Complete,
    Unproven,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
enum ExecutionStateBodyResidency {
    /// Bodies are present or their absence has not been authorized in process.
    Resident,
    /// `discard_known_bodies` deliberately released the protocol-owned bytes.
    DiscardedPostCommit,
    /// A store result disagreed with the complete component intent held before adoption.
    CommitResultMismatch,
}

/// What a post-commit body release keeps for a later same-frame restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcceptedExecutionRetention {
    /// A store holds the committed checkpoint: a restore rehydrates from it,
    /// and the released execution bodies are a second resident copy.
    DurableHead,
    /// No store can supply the committed execution: the accepted execution
    /// bodies stay resident until the next commit supersedes them (FIG-2521).
    Resident,
}

mod checkpoint_component;
use checkpoint_component::{
    PendingCheckpointComponentBody, ResidentCheckpointComponent, ResidentCheckpointComponentBody,
};

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
#[derive(Clone, Debug, serde::Serialize)]
pub struct RuntimeCheckpointComponents {
    completeness: CheckpointComponentCompleteness,
    entries: std::collections::BTreeMap<String, ResidentCheckpointComponent>,
    /// Resident-only proof. `RuntimeSessionState` skips this entire component set
    /// during serialization, and store hydration always constructs `Resident`.
    execution_state_body_residency: ExecutionStateBodyResidency,
}

impl Default for RuntimeCheckpointComponents {
    fn default() -> Self {
        Self::unproven()
    }
}

impl RuntimeCheckpointComponents {
    const EXECUTION_STATE_LEAF_PREFIX: &'static str = "execution_state/";

    pub(crate) fn complete_empty() -> Self {
        Self {
            completeness: CheckpointComponentCompleteness::Complete,
            entries: std::collections::BTreeMap::new(),
            execution_state_body_residency: ExecutionStateBodyResidency::Resident,
        }
    }

    pub(crate) fn unproven() -> Self {
        Self {
            completeness: CheckpointComponentCompleteness::Unproven,
            entries: std::collections::BTreeMap::new(),
            execution_state_body_residency: ExecutionStateBodyResidency::Resident,
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
    pub fn complete_for_new_session(&self) -> Result<(), crate::StoreError> {
        match self.completeness {
            CheckpointComponentCompleteness::Complete => Ok(()),
            CheckpointComponentCompleteness::Unproven => {
                Err(crate::StoreError::IncompleteCheckpointComponentSet)
            }
        }
    }

    fn descriptor(
        blob_ref: crate::store::BlobRef,
        fleet_format: crate::store::FleetFormat,
    ) -> crate::CheckpointComponentDescriptor {
        crate::CheckpointComponentDescriptor {
            blob_ref,
            encoding_version: fleet_format.writer_version(crate::surface_format!(
                crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION
            )),
        }
    }

    fn from_snapshot(snapshot: &SessionSnapshot, fleet_format: crate::store::FleetFormat) -> Self {
        let mut result = Self::unproven();
        if let Some(blob_ref) = snapshot.tool_state_ref.clone() {
            result.entries.insert(
                crate::store::TOOL_STATE_CHECKPOINT_COMPONENT.to_string(),
                ResidentCheckpointComponent::Unchanged {
                    descriptor: Self::descriptor(blob_ref, fleet_format),
                    body: ResidentCheckpointComponentBody::ToolState {
                        snapshot: None,
                        generation: snapshot.tool_state_generation,
                    },
                },
            );
        }
        if let Some(blob_ref) = snapshot.plugin_state_ref.clone() {
            result.entries.insert(
                crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT.to_string(),
                ResidentCheckpointComponent::Unchanged {
                    descriptor: Self::descriptor(blob_ref, fleet_format),
                    body: ResidentCheckpointComponentBody::PluginState {
                        snapshot: None,
                        generations: snapshot.plugin_state_generations.clone(),
                    },
                },
            );
        }
        if let Some(blob_ref) = snapshot.execution_state_ref.clone() {
            result.entries.insert(
                crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                ResidentCheckpointComponent::Unchanged {
                    descriptor: Self::descriptor(blob_ref, fleet_format),
                    body: ResidentCheckpointComponentBody::ExecutionState(None),
                },
            );
        }
        result
    }

    #[expect(
        clippy::expect_used,
        reason = "the decode above reported this component present"
    )]
    fn from_hydrated(
        checkpoint: &crate::store::HydratedSessionCheckpoint,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<Self, crate::StoreError> {
        let manifest = checkpoint.manifest(fleet_format)?;
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
                        .decode_component_for_fleet::<crate::ToolState>(key, fleet_format)?
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
                crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT => {
                    let snapshot = checkpoint
                        .decode_component_for_fleet::<crate::PluginState>(key, fleet_format)?
                        .expect("present plugin-state component");
                    ResidentCheckpointComponentBody::PluginState {
                        generations: plugin_generations(&snapshot),
                        snapshot: Some(snapshot),
                    }
                }
                crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT => {
                    ResidentCheckpointComponentBody::ExecutionState(
                        checkpoint.checked_component_body_for_fleet(key, fleet_format)?,
                    )
                }
                _ => ResidentCheckpointComponentBody::Opaque(
                    checkpoint.checked_component_body_for_fleet(key, fleet_format)?,
                ),
            };
            entries.insert(
                key.clone(),
                ResidentCheckpointComponent::Unchanged { descriptor, body },
            );
        }
        Ok(Self {
            completeness: CheckpointComponentCompleteness::Complete,
            entries,
            execution_state_body_residency: ExecutionStateBodyResidency::Resident,
        })
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn complete_refs_for_testing(
        refs: impl IntoIterator<Item = (String, crate::store::BlobRef)>,
    ) -> Self {
        let entries = refs
            .into_iter()
            .map(|(key, blob_ref)| {
                (
                    key,
                    ResidentCheckpointComponent::Unchanged {
                        descriptor: Self::descriptor(
                            blob_ref,
                            crate::store::FleetFormat::current(),
                        ),
                        body: ResidentCheckpointComponentBody::Opaque(None),
                    },
                )
            })
            .collect();
        Self {
            completeness: CheckpointComponentCompleteness::Complete,
            entries,
            execution_state_body_residency: ExecutionStateBodyResidency::Resident,
        }
    }

    pub fn build_checkpoint(
        &self,
        turn_state: crate::PersistedTurnState,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<crate::store::HydratedSessionCheckpoint, crate::StoreError> {
        if self.completeness != CheckpointComponentCompleteness::Complete {
            return Err(crate::StoreError::IncompleteCheckpointComponentSet);
        }
        let mut components = std::collections::BTreeMap::new();
        for (key, component) in &self.entries {
            let pending = match component {
                ResidentCheckpointComponent::Changed { body, .. } => {
                    let body = match body {
                        PendingCheckpointComponentBody::ToolState { snapshot, .. } => {
                            crate::store::encode_checkpoint_component(key, snapshot)
                                .map(std::sync::Arc::from)?
                        }
                        PendingCheckpointComponentBody::PluginState { snapshot, .. } => {
                            crate::store::encode_checkpoint_component(key, snapshot)
                                .map(std::sync::Arc::from)?
                        }
                        PendingCheckpointComponentBody::ExecutionState(bytes)
                        | PendingCheckpointComponentBody::Opaque(bytes) => {
                            std::sync::Arc::clone(bytes)
                        }
                    };
                    crate::HydratedCheckpointComponent::changed_for_fleet(body, fleet_format)
                }
                ResidentCheckpointComponent::Unchanged { descriptor, .. } => {
                    crate::HydratedCheckpointComponent::unchanged(descriptor)
                }
            };
            components.insert(key.clone(), pending);
        }
        Ok(crate::store::HydratedSessionCheckpoint {
            turn_state,
            components,
        })
    }

    fn component(&self, key: &str) -> Option<&ResidentCheckpointComponent> {
        self.entries.get(key)
    }

    fn component_ref(&self, key: &str) -> Option<&crate::store::BlobRef> {
        self.component(key)
            .and_then(|component| component.descriptor())
            .map(|descriptor| &descriptor.blob_ref)
    }

    fn tool_state_snapshot(&self) -> Option<&crate::ToolState> {
        self.component(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
            .and_then(ResidentCheckpointComponent::tool_state_snapshot)
    }

    fn tool_state_generation(&self) -> Option<u64> {
        self.component(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
            .and_then(ResidentCheckpointComponent::tool_state_generation)
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
            .and_then(|entry| entry.descriptor().cloned());
        self.entries.insert(
            key,
            ResidentCheckpointComponent::Changed {
                descriptor,
                body: PendingCheckpointComponentBody::ToolState {
                    snapshot,
                    generation,
                },
            },
        );
    }

    fn plugin_state(&self) -> Option<&crate::PluginState> {
        self.component(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
            .and_then(ResidentCheckpointComponent::plugin_state_snapshot)
    }

    fn plugin_generations(&self) -> Option<&std::collections::BTreeMap<String, u64>> {
        self.component(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
            .and_then(ResidentCheckpointComponent::plugin_generations)
    }

    fn set_plugin_state(&mut self, snapshot: Option<crate::PluginState>) {
        let key = crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT.to_string();
        let Some(snapshot) = snapshot else {
            self.entries.remove(&key);
            return;
        };
        let descriptor = self
            .entries
            .get(&key)
            .and_then(|entry| entry.descriptor().cloned());
        self.entries.insert(
            key,
            ResidentCheckpointComponent::Changed {
                descriptor,
                body: PendingCheckpointComponentBody::PluginState {
                    generations: plugin_generations(&snapshot),
                    snapshot,
                },
            },
        );
    }

    fn execution_state_snapshot(&self) -> Option<std::sync::Arc<[u8]>> {
        self.component(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .and_then(ResidentCheckpointComponent::execution_state_body)
    }

    fn set_execution_state_snapshot(&mut self, snapshot: Option<std::sync::Arc<[u8]>>) {
        self.execution_state_body_residency = ExecutionStateBodyResidency::Resident;
        self.entries
            .retain(|key, _| !key.starts_with(Self::EXECUTION_STATE_LEAF_PREFIX));
        self.set_execution_state_root(snapshot);
    }

    fn set_execution_state_root(&mut self, snapshot: Option<std::sync::Arc<[u8]>>) {
        let key = crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string();
        let Some(snapshot) = snapshot else {
            self.entries.remove(&key);
            return;
        };
        let descriptor = self
            .entries
            .get(&key)
            .and_then(|entry| entry.descriptor().cloned());
        self.entries.insert(
            key,
            ResidentCheckpointComponent::Changed {
                descriptor,
                body: PendingCheckpointComponentBody::ExecutionState(snapshot),
            },
        );
    }

    fn set_execution_state_components(
        &mut self,
        snapshot: crate::plugin::ExecutionStateSnapshot,
    ) -> Result<(), crate::StoreError> {
        if snapshot.root.is_none() && !snapshot.components.is_empty() {
            return Err(crate::StoreError::Backend(
                "an absent execution-state root cannot retain leaf components".to_string(),
            ));
        }
        self.execution_state_body_residency = ExecutionStateBodyResidency::Resident;
        let mut replacement_entries = std::collections::BTreeMap::new();
        for (key, component) in &snapshot.components {
            if !key.starts_with(Self::EXECUTION_STATE_LEAF_PREFIX) {
                return Err(crate::StoreError::Backend(format!(
                    "execution-state leaf component `{key}` is outside the `{}` namespace",
                    Self::EXECUTION_STATE_LEAF_PREFIX
                )));
            }
            let replacement = match component {
                crate::plugin::ExecutionStateComponentSnapshot::Changed(body) => {
                    ResidentCheckpointComponent::Changed {
                        descriptor: self
                            .entries
                            .get(key)
                            .and_then(|entry| entry.descriptor().cloned()),
                        body: PendingCheckpointComponentBody::Opaque(std::sync::Arc::clone(body)),
                    }
                }
                crate::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                    let Some(existing) = self.entries.get(key) else {
                        return Err(crate::StoreError::Backend(format!(
                            "execution-state leaf component `{key}` was marked unchanged without resident state"
                        )));
                    };
                    // A durable ref or a body still pending its first commit
                    // both let the next commit reference the leaf as
                    // unchanged; anything else has nothing to reference.
                    match existing {
                        ResidentCheckpointComponent::Unchanged { .. }
                        | ResidentCheckpointComponent::Changed {
                            descriptor: Some(_),
                            ..
                        }
                        | ResidentCheckpointComponent::Changed {
                            body: PendingCheckpointComponentBody::Opaque(_),
                            ..
                        } => existing.clone(),
                        _ => {
                            return Err(crate::StoreError::Backend(format!(
                                "execution-state leaf component `{key}` was marked unchanged without a durable ref or pending body"
                            )));
                        }
                    }
                }
            };
            replacement_entries.insert(key.clone(), replacement);
        }

        self.entries
            .retain(|key, _| !key.starts_with(Self::EXECUTION_STATE_LEAF_PREFIX));
        self.entries.extend(replacement_entries);
        self.set_execution_state_root(snapshot.root);
        Ok(())
    }

    /// Whether the next commit can reference the execution-state leaf `key`
    /// as unchanged: the resident set holds its durable ref or its pending body.
    fn holds_execution_state_leaf(&self, key: &str) -> bool {
        self.entries.get(key).is_some_and(|entry| {
            matches!(
                entry,
                ResidentCheckpointComponent::Unchanged { .. }
                    | ResidentCheckpointComponent::Changed {
                        descriptor: Some(_),
                        ..
                    }
                    | ResidentCheckpointComponent::Changed {
                        body: PendingCheckpointComponentBody::Opaque(_),
                        ..
                    }
            )
        })
    }

    fn execution_state_hydration(
        &self,
    ) -> Result<Option<crate::plugin::HydratedExecutionState>, crate::StoreError> {
        if self.execution_state_body_residency == ExecutionStateBodyResidency::CommitResultMismatch
        {
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind: "RuntimeCommitReceipt",
                message: "committed checkpoint components differ from resident commit intent"
                    .to_string(),
            });
        }
        let Some(root) = self.execution_state_snapshot() else {
            if self.execution_state_body_residency
                == ExecutionStateBodyResidency::DiscardedPostCommit
            {
                // A released root is never "no execution" (FIG-2521): the
                // durable head must be hydrated instead. Only a set with no
                // root entry at all never held execution.
                if self
                    .entries
                    .contains_key(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
                {
                    return Err(crate::StoreError::ExecutionStateBodiesReleased);
                }
                return Ok(None);
            }
            let has_leaves = self
                .entries
                .keys()
                .any(|key| key.starts_with(Self::EXECUTION_STATE_LEAF_PREFIX));
            if has_leaves {
                return Err(crate::StoreError::StoredDataCorrupt {
                    record_kind: "RuntimeCheckpointComponents",
                    message: "execution-state leaves exist without a root component".to_string(),
                });
            }
            return Ok(None);
        };
        let mut components = std::collections::BTreeMap::new();
        for (key, component) in &self.entries {
            if !key.starts_with(Self::EXECUTION_STATE_LEAF_PREFIX) {
                continue;
            }
            let Some(body) = component.opaque_body() else {
                return Err(crate::StoreError::StoredDataCorrupt {
                    record_kind: "RuntimeCheckpointComponents",
                    message: format!("execution-state leaf component `{key}` was not hydrated"),
                });
            };
            components.insert(key.clone(), body);
        }
        Ok(Some(crate::plugin::HydratedExecutionState {
            root,
            components,
        }))
    }

    fn manifest_matches_resident_commit_intent(
        &self,
        manifest: &crate::store::SessionCheckpoint,
    ) -> bool {
        // This compares the store result with the complete intent already held
        // in process. It performs no store lookup and does not infer permission
        // from the bodiless descriptor shape produced after adoption.
        self.build_checkpoint(
            crate::PersistedTurnState::default(),
            crate::store::FleetFormat::current(),
        )
        .and_then(|checkpoint| checkpoint.manifest(crate::store::FleetFormat::current()))
        .is_ok_and(|resident| resident.components == manifest.components)
    }

    fn discard_known_bodies(
        &mut self,
        committed_components_match: bool,
        retention: AcceptedExecutionRetention,
    ) {
        // With no store to rehydrate from, the resident execution bodies are
        // the accepted execution itself (FIG-2521): they stay in place, the
        // residency proof is untouched, and only the tool and plugin snapshots
        // — re-exported from the live plugins — are released. A set without a
        // root holds no execution to keep and is released like any other.
        if retention == AcceptedExecutionRetention::Resident
            && self.execution_state_snapshot().is_some()
        {
            self.entries
                .retain(|_, component| component.release_typed_snapshot());
            return;
        }
        // This is the sole writer of the privileged `DiscardedPostCommit`
        // state. Staging may only reset the proof to `Resident`.
        self.execution_state_body_residency = match (
            self.execution_state_body_residency,
            committed_components_match,
        ) {
            (ExecutionStateBodyResidency::CommitResultMismatch, _) => {
                ExecutionStateBodyResidency::CommitResultMismatch
            }
            (_, true) => ExecutionStateBodyResidency::DiscardedPostCommit,
            (_, false) => ExecutionStateBodyResidency::CommitResultMismatch,
        };
        self.entries.retain(|_, component| component.release_body());
    }

    fn adopt_manifest(&mut self, manifest: &crate::store::SessionCheckpoint) {
        self.entries
            .retain(|key, _| manifest.components.contains_key(key));
        for (key, descriptor) in &manifest.components {
            if let Some(component) = self.entries.get_mut(key) {
                component.adopt_descriptor(descriptor.clone());
            } else {
                self.entries.insert(
                    key.clone(),
                    ResidentCheckpointComponent::Unchanged {
                        descriptor: descriptor.clone(),
                        body: ResidentCheckpointComponentBody::Opaque(None),
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
/// Durable authority inputs required to reconstruct a session on another worker.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RuntimeSessionAuthority {
    pub tool_access: crate::SessionToolAccess,
    #[serde(default)]
    pub subagent: Option<crate::SubagentSessionContext>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeSessionState {
    pub session_id: SessionId,
    pub policy: SessionPolicy,
    /// Derived cache of FrameOpen nodes; never serialized or persisted.
    #[serde(skip)]
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    /// The follow-on the durable head owes (ADR 0101 §3). A head fact: it is
    /// adopted with the head and written only through a head commit, never
    /// serialized with the resident state.
    #[serde(skip)]
    pub pending_follow_on: Option<Box<crate::store::PendingFollowOn>>,
    #[serde(default)]
    pub session_graph: crate::SessionGraph,
    #[serde(default)]
    pub turn_index: usize,
    #[serde(default)]
    pub token_usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_usage: Option<TokenUsage>,
    #[serde(default)]
    pub protocol_turn_options: crate::ProtocolTurnOptions,
    /// Durable authority used to rebuild the session's Tool Catalog policy.
    #[serde(flatten)]
    pub authority: Box<RuntimeSessionAuthority>,
    #[serde(skip, default)]
    pub checkpoint_components: RuntimeCheckpointComponents,
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
    /// The resident mirror of the head config's `config_revision` (ADR 0101
    /// §12): what a submitter reads to fill `ApplyConfigPatch::base_config_revision`,
    /// and what the apply-time compare-and-set checks. `0` at creation, `+1`
    /// per applied config patch, and restored head-authoritatively with the
    /// rest of the config by [`adopt_durable_head`]. Skipped on serialize —
    /// the durable copy lives in the session head's `PersistedSessionConfig`.
    #[serde(skip)]
    pub config_revision: u64,
    /// Node ids known to exist durably. This is deliberately independent of
    /// the resident graph: partial residency omits durable off-path nodes,
    /// while host-side edits can add resident nodes before they commit.
    #[serde(skip)]
    pub persisted_node_ids: std::collections::HashSet<crate::NodeId>,
    /// Runtime-only marker set by a `PreservePersisted` open (FIG-3353): the
    /// loaded tool-state snapshot is durable truth and is never restamped from
    /// the live registry. Skipped on serialize — it is a per-open claim, not
    /// durable session content.
    #[serde(skip)]
    pub preserve_tool_state_snapshot: bool,
}

impl RuntimeSessionState {
    pub fn new(policy: SessionPolicy) -> Self {
        Self {
            session_id: SessionId::from("root"),
            policy,
            agent_frames: Vec::new(),
            current_frame_node_id: None,
            pending_follow_on: None,
            session_graph: crate::SessionGraph::default(),
            turn_index: 0,
            token_usage: TokenUsage::default(),
            last_prompt_usage: None,
            protocol_turn_options: crate::ProtocolTurnOptions::default(),
            authority: Box::default(),
            checkpoint_components: RuntimeCheckpointComponents::complete_empty(),
            token_ledger: Vec::new(),
            checkpoint_ref: None,
            head_revision: 0,
            config_revision: 0,
            persisted_node_ids: std::collections::HashSet::new(),
            preserve_tool_state_snapshot: false,
        }
    }

    /// Builds a `RuntimeSessionState` from snapshot data for protocol and process-engine
    /// implementors while materializing or restoring protocol session state.
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Self {
        Self::from_snapshot_for_fleet(snapshot, crate::store::FleetFormat::current())
    }

    /// The fleet-aware restore: descriptors reconstructed for components the
    /// snapshot names only by ref stamp the fleet's writer version for the
    /// component-encoding surface (FIG-3796).
    pub fn from_snapshot_for_fleet(
        snapshot: SessionSnapshot,
        fleet_format: crate::store::FleetFormat,
    ) -> Self {
        // Authority deliberately defaults here and must be restored by adopt_durable_head;
        // consuming a snapshot without the subsequent head adoption would widen authority.
        let checkpoint_components =
            RuntimeCheckpointComponents::from_snapshot(&snapshot, fleet_format);
        let agent_frames = snapshot
            .session_graph
            .agent_frame_records(&snapshot.session_id);
        let mut state = Self {
            session_id: snapshot.session_id,
            policy: snapshot.policy,
            agent_frames,
            current_frame_node_id: snapshot.current_frame_node_id,
            pending_follow_on: None,
            session_graph: snapshot.session_graph,
            turn_index: snapshot.turn_index,
            token_usage: snapshot.token_usage,
            last_prompt_usage: snapshot.last_prompt_usage,
            protocol_turn_options: snapshot.protocol_turn_options,
            authority: Box::default(),
            checkpoint_components,
            token_ledger: snapshot.token_ledger,
            checkpoint_ref: snapshot.checkpoint_ref,
            head_revision: 0,
            config_revision: 0,
            persisted_node_ids: std::collections::HashSet::new(),
            preserve_tool_state_snapshot: false,
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
            plugin_state_ref: self.plugin_state_ref().cloned(),
            plugin_state_generations: self
                .checkpoint_components
                .plugin_generations()
                .cloned()
                .unwrap_or_default(),
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
        self.token_ledger = snapshot.token_ledger.clone();
        self.checkpoint_ref = snapshot.checkpoint_ref.clone();
    }

    /// Folds durable token-ledger entries into a per-source report for protocol and administration
    /// embedders without mutating the ledger.
    pub fn usage_report(&self) -> super::usage::SessionUsageReport {
        super::usage::SessionUsageReport::from_entries(&self.token_ledger)
    }

    pub fn read_model(
        &self,
    ) -> Result<crate::session_graph::SessionReadModel, crate::SessionGraphScopeError> {
        self.session_graph
            .read_model(self.current_frame_node_id.as_ref())
    }

    /// Replaces the current frame's readable message tail for protocol implementors restoring
    /// state; transient messages are excluded and the frame projection is refreshed.
    pub fn replace_active_read_state(
        &mut self,
        messages: &[Message],
    ) -> Result<(), crate::SessionGraphScopeError> {
        self.ensure_agent_frame_initialized();
        self.session_graph
            .rewrite_active_read_tail(self.current_frame_node_id.as_ref(), messages)?;
        self.refresh_current_frame_projection();
        Ok(())
    }

    pub fn append_active_read_delta(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_active_read_delta(messages);
        self.refresh_current_frame_projection();
    }

    pub fn append_active_conversation_messages(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_active_read_delta(messages);
        self.refresh_current_frame_projection();
    }

    pub fn append_active_conversation_messages_with_clock(
        &mut self,
        messages: &[Message],
        clock: &dyn crate::Clock,
    ) {
        self.ensure_agent_frame_initialized_with_clock(clock);
        self.session_graph
            .append_active_conversation_messages_at(messages, clock.timestamp_rfc3339());
        self.refresh_current_frame_projection();
    }

    pub fn read_view(&self) -> Result<crate::SessionReadView, crate::SessionGraphScopeError> {
        crate::SessionReadView::from_persisted_state(self)
    }

    pub fn session_graph(&self) -> &crate::SessionGraph {
        &self.session_graph
    }

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
    pub fn plugin_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
    }

    /// Typed resident view of the well-known plugin-snapshot component.
    pub fn plugin_state(&self) -> Option<&crate::PluginState> {
        self.checkpoint_components.plugin_state()
    }

    /// Replace or explicitly delete the well-known plugin-snapshot component.
    pub fn set_plugin_state(&mut self, snapshot: Option<crate::PluginState>) {
        self.checkpoint_components.set_plugin_state(snapshot);
    }

    pub fn plugin_state_is_dirty(&self) -> bool {
        self.checkpoint_components
            .component(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
            .is_some_and(|component| {
                matches!(component, ResidentCheckpointComponent::Changed { .. })
            })
    }

    /// Durable reference for the well-known execution-state component.
    pub fn execution_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
    }

    /// Advances resident state to a store's committed head revision and realized timestamps, adopts
    /// durable artifact references, and clears transient snapshots so protocol and store
    /// implementors cannot reuse stale bytes.
    ///
    /// # Panics
    /// Panics in every build profile if the receipt does not advance resident
    /// state. Replay callers refresh state instead of adopting an old receipt.
    pub fn apply_persisted_commit_result(&mut self, result: crate::store::RuntimeCommitReceipt) {
        assert!(
            result.head_revision > self.head_revision,
            "adopted head revision must advance"
        );
        self.head_revision = result.head_revision;
        self.checkpoint_ref = Some(result.checkpoint_ref);
        self.session_graph
            .apply_realized_node_timestamps(&result.realized_node_timestamps);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
        let committed_components_match = self
            .checkpoint_components
            .manifest_matches_resident_commit_intent(&result.manifest);
        self.checkpoint_components.adopt_manifest(&result.manifest);
        self.checkpoint_components.discard_known_bodies(
            committed_components_match,
            AcceptedExecutionRetention::DurableHead,
        );
    }

    pub fn pending_graph_commit(&self) -> crate::GraphAppend {
        let nodes = self
            .session_graph
            .nodes
            .iter()
            .filter(|node| !self.persisted_node_ids.contains(&node.node_id))
            .map(|node| node.as_ref().clone())
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            crate::GraphAppend::PreserveHead
        } else {
            crate::GraphAppend::Extend { nodes }
        }
    }

    pub fn mark_node_ids_persisted<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = crate::NodeId>,
    {
        self.persisted_node_ids.extend(node_ids);
    }

    /// Clears in-memory tool, plugin, and execution-state snapshots for protocol implementors after
    /// their durable references have become authoritative.
    pub fn discard_runtime_snapshots(&mut self) {
        self.checkpoint_components
            .discard_known_bodies(true, AcceptedExecutionRetention::DurableHead);
    }

    /// [`Self::discard_runtime_snapshots`] for a session no store backs: the
    /// accepted execution bodies stay resident, replaced at the next commit,
    /// so a same-frame restore rebuilds from them instead of from nothing
    /// (FIG-2521).
    pub fn discard_runtime_snapshots_retaining_accepted_execution(&mut self) {
        self.checkpoint_components
            .discard_known_bodies(true, AcceptedExecutionRetention::Resident);
    }

    /// Updates execution state snapshot state for protocol and process-engine implementors while
    /// materializing or restoring protocol session state.
    pub fn set_execution_state_snapshot(
        &mut self,
        execution_state_snapshot: Option<std::sync::Arc<[u8]>>,
    ) {
        // A materialized frame-switch outcome passes `None` here to clear the checkpoint. Clear
        // the durable ref with the resident body: every store interprets an absent body with a
        // present ref as an unchanged component, which would otherwise restore the old frame.
        self.checkpoint_components
            .set_execution_state_snapshot(execution_state_snapshot);
    }

    /// Replaces the complete protocol-owned execution-state root and leaf set.
    ///
    /// Runtime-owned staging: the turn boundary and the explicit administrative
    /// restore path are the only callers, so this is not integrator surface
    /// (ADR 0051's "neither" class — it only mutates state the runtime owns).
    /// Downstream tests reach the same staging through
    /// `lash_core::testing::stage_execution_state_components`.
    pub fn set_execution_state_components(
        &mut self,
        snapshot: crate::plugin::ExecutionStateSnapshot,
    ) -> Result<(), crate::StoreError> {
        self.checkpoint_components
            .set_execution_state_components(snapshot)
    }

    /// Stages a complete execution capture the protocol session was just
    /// restored to, over the resident set this state already holds (FIG-2521).
    ///
    /// The executor treats every restored leaf as its persisted baseline, so
    /// the next commit references them as unchanged. A leaf the resident set
    /// holds — durably, or as a body still waiting for its first commit —
    /// keeps exactly that bookkeeping; only a leaf the set never held is
    /// staged with its body. The root is staged as changed: a capture may
    /// carry appended seed globals the durable root does not.
    pub fn stage_restored_execution_state(
        &mut self,
        restored: crate::plugin::HydratedExecutionState,
    ) -> Result<(), crate::StoreError> {
        let mut snapshot = crate::plugin::ExecutionStateSnapshot::from_root(Some(restored.root));
        for (key, body) in restored.components {
            if self.checkpoint_components.holds_execution_state_leaf(&key) {
                snapshot.unchanged_component(key);
            } else {
                snapshot.changed_component(key, body);
            }
        }
        self.set_execution_state_components(snapshot)
    }

    /// Exposes execution state snapshot to protocol and process-engine implementors while
    /// materializing or restoring protocol session state. Returns `None` when no execution state
    /// snapshot is present.
    pub fn execution_state_snapshot(&self) -> Option<std::sync::Arc<[u8]>> {
        self.checkpoint_components.execution_state_snapshot()
    }

    /// Returns the fully hydrated protocol-owned execution-state root and leaves.
    pub fn execution_state_hydration(
        &self,
    ) -> Result<Option<crate::plugin::HydratedExecutionState>, crate::StoreError> {
        self.checkpoint_components.execution_state_hydration()
    }

    /// Refreshes exported plugin state while respecting the session handle's
    /// namespace permissions. Plugin-facing handles expose no namespaces.
    pub fn refresh_plugin_states(&mut self, plugins: &dyn SessionPluginStateSource) {
        self.refresh_plugin_states_with(plugins, |source| source.export_plugin_state());
    }

    pub fn capture_plugin_states(&mut self, plugins: &dyn SessionPluginStateSource) {
        self.refresh_plugin_states_with(plugins, |source| source.capture_plugin_state());
    }

    fn refresh_plugin_states_with(
        &mut self,
        plugins: &dyn SessionPluginStateSource,
        capture: fn(&dyn SessionPluginStateSource) -> crate::PluginState,
    ) {
        // A `PreservePersisted` open (FIG-3353) never reconciled its registry,
        // so refreshing tool state here would overwrite the durable surface
        // with whatever the sources happen to advertise. The loaded snapshot
        // rides the next commit forward untouched.
        if !self.preserve_tool_state_snapshot {
            let generation = plugins.tool_state_generation();
            if self.tool_state_ref().is_none() || self.tool_state_generation() != Some(generation) {
                let snapshot = plugins.export_tool_state();
                self.set_tool_state_snapshot(Some(snapshot));
            }
        }

        let generations = plugins.plugin_state_generations();
        let captured = self.checkpoint_components.plugin_generations();
        if !generations.is_empty()
            && (self.plugin_state_ref().is_none() || captured != Some(&generations))
        {
            self.set_plugin_state(Some(capture(plugins)));
        }
    }
}

impl RuntimeSessionState {
    pub fn refresh_current_frame_projection(&mut self) {
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(|frame_node_id| {
                crate::FrameNodeId::new(frame_node_id)
                    .expect("a graph node identity selected as a frame is non-empty")
            });
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    pub fn current_agent_frame(&self) -> Option<&crate::AgentFrameRecord> {
        self.agent_frames.iter().find(|frame| {
            Some(frame.frame_node_id.as_str()) == self.current_frame_node_id.as_deref()
        })
    }

    /// This is a raw field read with no layering and is the single source of truth for live
    /// policy.
    pub fn effective_policy(&self) -> &SessionPolicy {
        &self.policy
    }

    /// This is a raw field read with no layering and is the single source of truth for live
    /// protocol turn options.
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
    #[expect(
        clippy::expect_used,
        reason = "a frame node identity and the initial frame material are non-empty"
    )]
    pub fn ensure_agent_frame_initialized_with_clock(&mut self, clock: &dyn crate::Clock) {
        if let Some(frame_node_id) = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
        {
            self.current_frame_node_id = Some(
                crate::FrameNodeId::new(frame_node_id)
                    .expect("a graph node identity selected as a frame is non-empty"),
            );
            self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
            return;
        }
        if self.session_graph.leaf_node_id.is_some() {
            self.current_frame_node_id = None;
            self.agent_frames.clear();
            return;
        }
        let assignment = crate::AgentFrameAssignment::from_policy(self.policy.clone());
        let frame_key = crate::FrameKey::from_caller_material("initial-frame")
            .expect("the initial frame material is non-empty");
        let frame_node_id =
            crate::session_graph::frame_node_id(&self.session_id, frame_key.as_str());
        self.session_graph.append_frame_open_with_id_at(
            frame_node_id.clone(),
            frame_key,
            crate::AgentFrameReason::initial(),
            assignment,
            self.protocol_turn_options.clone(),
            clock.timestamp_rfc3339(),
        );
        self.current_frame_node_id = Some(frame_node_id);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    /// Open a still-unpersisted initial frame under the session's settled
    /// protocol turn options.
    ///
    /// A fresh session opens its initial frame when its state is built, before
    /// protocol materialization settles the options. That frame is not durable
    /// until the first commit carries it, while the settled options are
    /// published at materialization. A reopen of that durable head therefore
    /// opens the frame under the settled options, and the first commit must
    /// carry the same frame on every execution (FIG-3684). A persisted frame
    /// is an immutable historical snapshot and is never rewritten.
    #[expect(
        clippy::expect_used,
        reason = "the initial frame material is a non-empty literal"
    )]
    pub fn open_unpersisted_initial_frame_under_settled_protocol_options(&mut self) {
        let frame_key = crate::FrameKey::from_caller_material("initial-frame")
            .expect("the initial frame material is non-empty");
        let frame_node_id = crate::NodeId::new(
            crate::session_graph::frame_node_id(&self.session_id, frame_key.as_str()).into_inner(),
        );
        if self.persisted_node_ids.contains(&frame_node_id) {
            return;
        }
        let settled = &self.protocol_turn_options;
        let Some(position) = self.session_graph.nodes.iter().position(|node| {
            node.node_id == frame_node_id
                && matches!(
                    &node.payload,
                    crate::SessionNodePayload::FrameOpen { protocol_turn_options, .. }
                        if protocol_turn_options != settled
                )
        }) else {
            return;
        };
        let settled = settled.clone();
        let record = std::sync::Arc::make_mut(&mut self.session_graph.data_mut().nodes[position]);
        if let crate::SessionNodePayload::FrameOpen {
            protocol_turn_options,
            ..
        } = &mut record.payload
        {
            *protocol_turn_options = settled;
        }
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    #[expect(
        clippy::expect_used,
        reason = "the initial frame material is a non-empty literal"
    )]
    pub fn reset_initial_agent_frame_with_clock(
        &mut self,
        assignment: crate::AgentFrameAssignment,
        protocol_turn_options: crate::ProtocolTurnOptions,
        clock: &dyn crate::Clock,
    ) {
        self.policy = assignment.policy.clone();
        self.protocol_turn_options = protocol_turn_options.clone();
        let frame_key = crate::FrameKey::from_caller_material("initial-frame")
            .expect("the initial frame material is non-empty");
        let frame_node_id =
            crate::session_graph::frame_node_id(&self.session_id, frame_key.as_str());
        self.session_graph.append_frame_open_with_id_at(
            frame_node_id.clone(),
            frame_key,
            crate::AgentFrameReason::initial(),
            assignment,
            protocol_turn_options,
            clock.timestamp_rfc3339(),
        );
        self.current_frame_node_id = Some(frame_node_id);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }
}

pub mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`RuntimeSessionState`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait RuntimeSessionStateFacadeOps {
        fn turn_state(&self) -> PersistedTurnState;

        // APIT is intentionally non-dyn-compatible; this trait has one static-dispatch impl.
        fn turn_scope(&self, turn_id: impl Into<TurnId>) -> crate::ExecutionScope;

        // APIT is intentionally non-dyn-compatible; this trait has one static-dispatch impl.
        fn queue_drain_scope(&self, drain_id: impl Into<String>) -> crate::ExecutionScope;

        fn process_execution_env_spec(
            &self,
            fallback_policy: &SessionPolicy,
        ) -> crate::ProcessExecutionEnvSpec;
    }

    impl RuntimeSessionStateFacadeOps for RuntimeSessionState {
        fn turn_state(&self) -> PersistedTurnState {
            PersistedTurnState {
                turn_index: self.turn_index,
                token_usage: self.token_usage.clone(),
                last_prompt_usage: self.last_prompt_usage.clone(),
                protocol_turn_options: self.protocol_turn_options.clone(),
            }
        }

        fn turn_scope(&self, turn_id: impl Into<TurnId>) -> crate::ExecutionScope {
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
mod tests;

/// Adopt a durable session config onto resident state: the one config→state
/// mapping, shared by head adoption and by a root's recorded turn config
/// (FIG-3600 S6, D3 §2.1). The durable config wins for every fact it
/// carries; the turn budget stays live-owned (FIG-1875), and a `None` prompt
/// or protocol-turn-options value (a head written before the field existed)
/// keeps the resident one.
pub fn adopt_session_config(
    state: &mut RuntimeSessionState,
    config: &crate::PersistedSessionConfig,
) {
    state.authority.tool_access = config.tool_access.clone();
    state.authority.subagent = config.subagent.clone();
    apply_persisted_session_config(state, config);
    if let Some(options) = config.protocol_turn_options.as_ref() {
        state.protocol_turn_options = options.clone();
    }
}

/// Adopt the durable head config's carried fields onto resident state: the
/// policy-homed values plus the config compare-and-set revision, which moves
/// only with the config itself (ADR 0101 §12).
pub(super) fn apply_persisted_session_config(
    state: &mut RuntimeSessionState,
    config: &crate::PersistedSessionConfig,
) {
    state.policy.model = config.model.clone();
    state.policy.provider_id = config.provider_id.clone();
    if let Some(prompt) = config.prompt.as_ref() {
        state.policy.prompt = prompt.clone();
    }
    state.policy.generation = config.generation.clone();
    state.config_revision = config.config_revision;
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
    fleet_format: crate::store::FleetFormat,
) -> Result<(), crate::StoreError> {
    let Some(checkpoint) = checkpoint else {
        state.checkpoint_components = RuntimeCheckpointComponents::complete_empty();
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
    state.checkpoint_components =
        RuntimeCheckpointComponents::from_hydrated(&checkpoint, fleet_format)?;
    state.ensure_agent_frame_initialized();
    Ok(())
}

/// The runtime-lease facts that stay live-owned across a durable-head
/// adoption (FIG-1875).
///
/// Everything the head carries is adopted head-authoritatively; only these
/// process-local lease facts are installed by the caller. The provider
/// resolver is also live-owned, but it lives outside `RuntimeSessionState`
/// and is never touched by adoption.
pub struct LiveOwnedSessionFacts {
    pub(crate) session_id: Option<SessionId>,
    pub(crate) turn_budget: crate::TurnBudget,
}

impl LiveOwnedSessionFacts {
    /// Capture the live-owned facts of the policy about to be overwritten.
    pub fn of(policy: &SessionPolicy) -> Self {
        Self {
            session_id: policy.session_id.clone(),
            turn_budget: policy.turn_budget,
        }
    }
}

/// Adopt a durable session head (and its checkpoint) as one total operation.
///
/// This is the single home of the head→state mapping (FIG-1875, ruled
/// head-authoritative): on any adoption the durable head wins for every fact
/// it carries — graph, frames, config, protocol turn options, checkpoint
/// progress, authority, and ledger. No resident copy of a durable fact is
/// preserved; the only survivors are the caller-supplied
/// [`LiveOwnedSessionFacts`] plus whatever the target state carries for facts
/// the head does not represent (for example the live-policy flags
/// `autonomous` and `no_progress_budget`).
pub fn adopt_durable_head(
    state: &mut RuntimeSessionState,
    head: &crate::store::SessionHead,
    checkpoint: Option<crate::store::HydratedSessionCheckpoint>,
    live_owned: LiveOwnedSessionFacts,
    fleet_format: crate::store::FleetFormat,
) -> Result<(), crate::StoreError> {
    state.session_id = head.session_id.clone();
    state.session_graph = head.graph.clone();
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    state.current_frame_node_id = head.current_frame_node_id.clone();
    state.pending_follow_on = head.pending_follow_on.clone().map(Box::new);
    state.checkpoint_ref = head.checkpoint_ref.clone();
    state.token_ledger = head.token_ledger.clone();
    state.checkpoint_components = if head.checkpoint_ref.is_some() {
        RuntimeCheckpointComponents::unproven()
    } else {
        RuntimeCheckpointComponents::complete_empty()
    };
    state.head_revision = head.head_revision;
    state.persisted_node_ids = head
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect();
    adopt_session_config(state, &head.config);
    state.policy.session_id = live_owned.session_id;
    state.policy.turn_budget = live_owned.turn_budget;
    // The config adopted the commanded head value before the checkpoint
    // restore (so a checkpointless graph's initial frame captures it); adopt
    // it again after (the head row is authoritative over the checkpoint's
    // turn-state copy; `None` is a pre-v6-content head, which keeps the
    // checkpoint fallback). FIG-2479.
    apply_session_checkpoint(state, checkpoint, fleet_format)?;
    if let Some(options) = head.config.protocol_turn_options.as_ref() {
        state.protocol_turn_options = options.clone();
    }
    Ok(())
}

pub fn append_session_nodes_to_state_with_clock(
    state: &mut RuntimeSessionState,
    nodes: &[crate::SessionAppendNode],
    draft_namespace: &str,
    clock: &dyn crate::Clock,
) -> Vec<crate::NodeId> {
    let drafts = session_append_node_drafts(nodes, draft_namespace);
    state.ensure_agent_frame_initialized_with_clock(clock);
    state
        .session_graph
        .append_node_drafts_at(draft_namespace, drafts, clock.timestamp_rfc3339())
}

/// Names a boundary; a stable name alone does not make a rebuilt request replay-safe.
///
/// FIG-869 audit (production callers; SQLite witnesses in
/// `lash-sqlite-store/tests/boundary_retry.rs`):
///
/// | Operation | Caller contract | Retry after head advance | Decision |
/// | --- | --- | --- | --- |
/// | append-session-nodes (runtime and graph service) | Stable host request ID | Semantic receipt returns original result | Keep append identity |
/// | append-session-nodes (turn draft) | Deduplicate within one physical turn draft | Local identity returns recorded outcome; enclosing turn owns persistence | No independent boundary receipt; outside non-append adoption |
/// | preview (initial park) | Local hash input, never submitted | No store operation to replay | No speculative receipt |
/// | initial-park | Persist dirty state on consuming park | Exact commit replays; changed content gets a different operation | Keep content-addressed identity; no rebuilt-request promise |
/// | record-config | Persist materialized protocol configuration | Semantic-boundary receipt replays same-request rebuilds; a differing canonical encoding is refused (FIG-2480) | Adopted: `SemanticBoundary` identity with a typed operation tag |
/// | record-seeded-config (reopen seed) | Persist the reconciled host seed at facade reopen | Exact seed retry replays; a changed seed or advanced head gets a different operation | Keep content-addressed identity (initial-park pattern); no rebuilt-request promise |
/// | create-session | Create a new child; registered IDs are rejected | Semantic-boundary receipt replays same-request rebuilds; a differing canonical encoding is refused (FIG-2480) | Adopted: `SemanticBoundary` identity with a typed operation tag |
/// | usage-ledger | Flush staged child usage after its turn | Semantic-boundary receipt replays same-request rebuilds; a differing canonical encoding is refused (FIG-2480) | Adopted: `SemanticBoundary` identity with a typed operation tag |
///
/// Plain-commit writes still require the original canonical commit for replay,
/// excluding the optimistic head revision. Operations adopting the FIG-2480
/// `SemanticBoundary` identity replay a rebuilt same-request retry from receipt
/// evidence instead; a non-retry with a differing canonical encoding is refused,
/// never silently deduplicated. Do not infer host retry safety from a stable
/// scope alone: only the identity a commit carries decides.
///
/// Frame open and extension apply are no longer callers of this seam. Remaining
/// references are test helpers and witnesses, not additional production operations.
pub fn boundary_operation(
    session_id: &SessionId,
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

pub fn derive_graph_commit_node_ids(
    state: &mut RuntimeSessionState,
    graph: &mut crate::GraphAppend,
    operation: &crate::OperationId,
) -> Result<Vec<crate::NodeId>, crate::StoreError> {
    let mapping = graph.derive_node_ids(&state.session_id, operation)?;
    apply_graph_commit_node_id_mapping(state, &mapping)?;
    Ok(mapping.into_iter().map(|(_, derived)| derived).collect())
}

pub fn apply_graph_commit_node_id_mapping(
    state: &mut RuntimeSessionState,
    mapping: &[(crate::NodeId, crate::NodeId)],
) -> Result<(), crate::StoreError> {
    state
        .session_graph
        .remap_node_ids(&state.session_id, mapping);
    if let Some(current) = state.current_frame_node_id.as_mut()
        && let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current.as_str())
    {
        *current = crate::FrameNodeId::new(derived.as_str())
            .expect("derived graph node identities are non-empty");
    }
    state.agent_frames = state
        .session_graph
        .try_agent_frame_records(&state.session_id)?;
    Ok(())
}

pub fn receipt_append_node_ids(
    result: &crate::store::RuntimeCommitReceipt,
    requested_node_count: usize,
) -> Result<Vec<crate::NodeId>, crate::StoreError> {
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

pub fn resolve_append_node_ids(
    result: &crate::store::RuntimeCommitReceipt,
    locally_derived_node_ids: Vec<crate::NodeId>,
) -> Result<Vec<crate::NodeId>, crate::StoreError> {
    if result.receipt_replayed {
        receipt_append_node_ids(result, locally_derived_node_ids.len())
    } else {
        Ok(locally_derived_node_ids)
    }
}

pub fn open_agent_frame_in_state_with_clock(
    state: &mut RuntimeSessionState,
    request: crate::OpenAgentFrameRequest,
    clock: &dyn crate::Clock,
) -> Result<crate::OpenAgentFrameResult, crate::RuntimeError> {
    state.ensure_agent_frame_initialized_with_clock(clock);
    let previous = state.current_agent_frame().cloned();
    let mut assignment = previous
        .as_ref()
        .map(|frame| frame.assignment.clone())
        .unwrap_or_else(|| crate::AgentFrameAssignment::from_policy(state.policy.clone()));
    assignment.policy = state.policy.clone();
    let protocol_turn_options = state.protocol_turn_options.clone();
    let frame_node_id =
        crate::session_graph::frame_node_id(&state.session_id, request.frame_key.as_str());
    let opened = state.session_graph.append_frame_open_with_id_at(
        frame_node_id.clone(),
        request.frame_key.clone(),
        request.reason,
        assignment,
        protocol_turn_options,
        clock.timestamp_rfc3339(),
    );
    if !opened {
        if state.current_frame_node_id.as_deref() == Some(frame_node_id.as_str()) {
            return Ok(crate::OpenAgentFrameResult {
                frame_node_id: frame_node_id.into_inner(),
                opened: false,
                initial_node_ids: Vec::new(),
            });
        }
        return Err(crate::RuntimeError::new(
            crate::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported,
            "switching to a persisted historical frame requires a commanded config patch, which is not supported",
        ));
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
        request.frame_key.as_str(),
        clock,
    );
    Ok(crate::OpenAgentFrameResult {
        frame_node_id: state
            .current_frame_node_id
            .clone()
            .map(crate::FrameNodeId::into_inner)
            .unwrap_or_default(),
        opened: true,
        initial_node_ids,
    })
}

/// Builds the node drafts an append request materializes, with fallback
/// message ids derived from the append's draft namespace so the same request
/// yields the same drafts wherever it is folded.
pub fn session_append_node_drafts(
    nodes: &[crate::SessionAppendNode],
    draft_namespace: &str,
) -> Vec<crate::session_graph::SessionNodeDraft> {
    nodes
        .iter()
        .enumerate()
        .map(|(ordinal, node)| {
            let fallback_digest = crate::stable_hash::blake3_hex(
                "lash-session-append-draft-fallback/v2",
                format!("{draft_namespace}:{ordinal}").as_bytes(),
            );
            session_append_node_draft(node, &format!("m_append_{fallback_digest}"))
        })
        .collect()
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

fn plugin_generations(state: &crate::PluginState) -> std::collections::BTreeMap<String, u64> {
    state
        .plugins
        .iter()
        .map(|(id, namespace)| (id.clone(), namespace.generation))
        .collect()
}

/// The plugin-side facts durable session state refreshes itself from.
///
/// `lash-core`'s `PluginSession` is the sole implementor; the trait exists so
/// the durable state struct does not need the plugin host to describe itself.
pub trait SessionPluginStateSource {
    /// Current tool-registry generation.
    fn tool_state_generation(&self) -> u64;

    /// Snapshot of the tool registry at the current generation.
    fn export_tool_state(&self) -> crate::ToolState;

    /// Per-plugin state generations, keyed by plugin id.
    fn plugin_state_generations(&self) -> std::collections::BTreeMap<String, u64>;

    /// Namespace-filtered export, as a plugin-facing handle sees it.
    fn export_plugin_state(&self) -> crate::PluginState;

    /// Unfiltered capture, as the runtime commits it.
    fn capture_plugin_state(&self) -> crate::PluginState;
}

impl<T> SessionPluginStateSource for std::sync::Arc<T>
where
    T: SessionPluginStateSource + ?Sized,
{
    fn tool_state_generation(&self) -> u64 {
        T::tool_state_generation(self)
    }

    fn export_tool_state(&self) -> crate::ToolState {
        T::export_tool_state(self)
    }

    fn plugin_state_generations(&self) -> std::collections::BTreeMap<String, u64> {
        T::plugin_state_generations(self)
    }

    fn export_plugin_state(&self) -> crate::PluginState {
        T::export_plugin_state(self)
    }

    fn capture_plugin_state(&self) -> crate::PluginState {
        T::capture_plugin_state(self)
    }
}
