/// version_surface = "coexist"
/// version_guard(items(LASH_RLM_EXECUTION_STATE_LEAF_DOMAIN_VERSION, leaf_component_key))
const LASH_RLM_EXECUTION_STATE_LEAF_DOMAIN_VERSION: &str = "lash-rlm-execution-state-leaf/v2";

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::SessionError;
use lash_core::plugin::ExecutionLeafName;
use lashlang::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, CanonicalPathSegment,
    SnapshotDecodeError, Value as FlowValue, validate_canonical_messagepack_structure,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

mod worker_envelope;
pub(crate) use worker_envelope::{RlmWorkerCapture, RlmWorkerEnvelope};

use super::apply_global_defaults;
use super::snapshot::{RLM_SNAPSHOT_VERSION, RlmSnapshotError};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RlmSnapshotRoot {
    version: u32,
    engine: String,
    /// The Lashlang durable header: its format version and heap counters.
    #[serde(with = "serde_bytes")]
    state_header: Vec<u8>,
    /// One Lashlang durable fragment per binding: the binding's value and the
    /// heap objects it carries (`lashlang::DurableParts`).
    globals: BTreeMap<String, PersistedValue>,
    deferred_trigger_resolutions: lash_lashlang_runtime::DeferredTriggerResolutionRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedValue {
    Inline {
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Leaf {
        component: ExecutionLeafName,
    },
}

include!(concat!(env!("OUT_DIR"), "/rlm_snapshot_fields.rs"));

// Serialized field order of the snapshot's dependency-owned nodes.
//
// These types live in `lash-sansio` and `lash-lashlang-runtime`, and the build
// script used to derive each list by serializing an all-fields-set witness.
// That put both crates -- and every first-party crate beneath them -- into
// Buck2's exec configuration, compiled a second time at `opt-level=3` for an
// output no product artifact consumes (FIG-3032). The lists are declared here
// instead, and `generated_snapshot_field_schemas_match_all_fields_set_serialization`
// re-derives every one of them from the same witnesses at test time, so a
// field added, removed or reordered upstream still fails the suite rather than
// silently moving the canonical envelope.

/// `lash_lashlang_runtime::DeferredResolutionLinkKey`.
const DEFERRED_LINK_KEY_FIELDS: &[&str] = &["address"];

/// `lash_lashlang_runtime::DeferredTriggerResolutionRecord`.
const DEFERRED_TRIGGER_RESOLUTION_FIELDS: &[&str] = &["link_key", "resolutions"];

/// `lash_lashlang_runtime::TriggerResolution`.
const TRIGGER_RESOLUTION_FIELDS: &[&str] = &[
    "kind",
    "provider_id",
    "constructor_path",
    "input_type",
    "event_type",
    "route",
    "provider_ids",
];

fn validate_canonical_root(data: &[u8]) -> Result<(), RlmSnapshotError> {
    if matches!(
        data.iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace()),
        Some(b'{' | b'[')
    ) {
        return Err(RlmSnapshotError::FormatMismatch {
            details: "legacy JSON envelope is not a canonical typed root".to_string(),
        });
    }
    validate_canonical_messagepack_structure(
        data,
        "root",
        CANONICAL_MESSAGEPACK_DEPTH_LIMIT,
        root_map_order,
        root_map_required,
    )
    .map_err(|error| match error {
        SnapshotDecodeError::DepthLimitExceeded { limit }
        | SnapshotDecodeError::ValueDepthLimitExceeded { limit } => {
            RlmSnapshotError::EnvelopeDepthLimitExceeded { limit }
        }
        SnapshotDecodeError::NonCanonicalEncoding { location, reason } => {
            RlmSnapshotError::NonCanonicalEnvelope { location, reason }
        }
        SnapshotDecodeError::InvalidEncoding(details) => {
            RlmSnapshotError::FormatMismatch { details }
        }
        error @ (SnapshotDecodeError::VersionMismatch { .. }
        | SnapshotDecodeError::HeaplessSnapshotContainsReference { .. }) => {
            RlmSnapshotError::Lashlang(error)
        }
        // Preserve future decoder failures as typed Lashlang errors; never accept the envelope.
        _ => RlmSnapshotError::Lashlang(error),
    })
}

fn probe_snapshot_version(data: &[u8]) -> Result<u32, RlmSnapshotError> {
    fn take<'a>(data: &'a [u8], offset: &mut usize, len: usize) -> Option<&'a [u8]> {
        let end = offset.checked_add(len)?;
        let value = data.get(*offset..end)?;
        *offset = end;
        Some(value)
    }

    fn read_len(data: &[u8], offset: &mut usize, marker: u8) -> Option<usize> {
        match marker {
            0x80..=0x8f => Some(usize::from(marker & 0x0f)),
            0xde => Some(usize::from(u16::from_be_bytes(
                take(data, offset, 2)?.try_into().ok()?,
            ))),
            0xdf => {
                usize::try_from(u32::from_be_bytes(take(data, offset, 4)?.try_into().ok()?)).ok()
            }
            _ => None,
        }
    }

    fn read_string<'a>(data: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
        let marker = *take(data, offset, 1)?.first()?;
        let len = match marker {
            0xa0..=0xbf => usize::from(marker & 0x1f),
            0xd9 => usize::from(*take(data, offset, 1)?.first()?),
            0xda => usize::from(u16::from_be_bytes(take(data, offset, 2)?.try_into().ok()?)),
            0xdb => {
                usize::try_from(u32::from_be_bytes(take(data, offset, 4)?.try_into().ok()?)).ok()?
            }
            _ => return None,
        };
        take(data, offset, len)
    }

    fn read_u32(data: &[u8], offset: &mut usize) -> Option<u32> {
        let marker = *take(data, offset, 1)?.first()?;
        match marker {
            0x00..=0x7f => Some(u32::from(marker)),
            0xcc => Some(u32::from(*take(data, offset, 1)?.first()?)),
            0xcd => Some(u32::from(u16::from_be_bytes(
                take(data, offset, 2)?.try_into().ok()?,
            ))),
            0xce => Some(u32::from_be_bytes(take(data, offset, 4)?.try_into().ok()?)),
            _ => None,
        }
    }

    let incompatible = || RlmSnapshotError::FormatMismatch {
        details: "snapshot root does not begin with a MessagePack `version` field".to_string(),
    };
    if matches!(
        data.iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace()),
        Some(b'{' | b'[')
    ) {
        return Err(RlmSnapshotError::FormatMismatch {
            details: "legacy JSON envelope is not a canonical typed root".to_string(),
        });
    }
    let mut offset = 0;
    let marker = *take(data, &mut offset, 1)
        .and_then(<[u8]>::first)
        .ok_or_else(incompatible)?;
    if read_len(data, &mut offset, marker).ok_or_else(incompatible)? == 0
        || read_string(data, &mut offset).ok_or_else(incompatible)? != b"version"
    {
        return Err(incompatible());
    }
    read_u32(data, &mut offset).ok_or_else(incompatible)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootNode {
    Root,
    Globals,
    Global,
    DeferredTrigger,
    LinkKey,
    Resolutions,
    Resolution,
    TriggerEventType,
    TriggerTypeExpr,
    TriggerObjectFields,
    TriggerUnionTypes,
    TriggerTypeField,
    TriggerProcess,
    TriggerProcessParams,
    TriggerProcessParam,
    Json,
    Other,
}

impl RootNode {
    fn child(self, segment: &CanonicalPathSegment) -> Self {
        use RootNode::*;
        match (self, segment.key()) {
            (Root, Some("globals")) => Globals,
            (Root, Some("deferred_trigger_resolutions")) => DeferredTrigger,
            (Globals, Some(_)) => Global,
            (DeferredTrigger, Some("link_key")) => LinkKey,
            (DeferredTrigger, Some("resolutions")) => Resolutions,
            (Resolutions, Some(_)) => Resolution,
            (Resolution, Some("input_type")) => TriggerTypeExpr,
            (Resolution, Some("event_type")) => TriggerEventType,
            (Resolution, Some("route")) => Json,
            (TriggerEventType, Some("ty")) => TriggerTypeExpr,
            (TriggerTypeExpr, Some("Object")) => TriggerObjectFields,
            (TriggerTypeExpr, Some("List" | "TriggerHandle")) => TriggerTypeExpr,
            (TriggerTypeExpr, Some("Union")) => TriggerUnionTypes,
            (TriggerTypeExpr, Some("Process")) => TriggerProcess,
            (TriggerObjectFields, None) => TriggerTypeField,
            (TriggerUnionTypes, None) => TriggerTypeExpr,
            (TriggerTypeField, Some("ty")) => TriggerTypeExpr,
            (TriggerProcess, Some("params")) => TriggerProcessParams,
            (TriggerProcess, Some("output")) => TriggerTypeExpr,
            (TriggerProcessParams, None) => TriggerProcessParam,
            (TriggerProcessParam, Some("ty")) => TriggerTypeExpr,
            (Json, _) => Json,
            _ => Other,
        }
    }
}

fn root_node(path: &[CanonicalPathSegment]) -> RootNode {
    path.iter().fold(RootNode::Root, RootNode::child)
}

fn root_map_order(path: &[CanonicalPathSegment]) -> CanonicalMapOrder {
    use RootNode::*;
    match root_node(path) {
        Root => CanonicalMapOrder::Declared(ROOT_FIELDS),
        Globals | Resolutions | Json => CanonicalMapOrder::Sorted,
        Global => CanonicalMapOrder::Declared(PERSISTED_VALUE_FIELDS),
        DeferredTrigger => CanonicalMapOrder::Declared(DEFERRED_TRIGGER_RESOLUTION_FIELDS),
        LinkKey => CanonicalMapOrder::Declared(DEFERRED_LINK_KEY_FIELDS),
        Resolution => CanonicalMapOrder::Declared(TRIGGER_RESOLUTION_FIELDS),
        TriggerEventType => CanonicalMapOrder::Declared(&["name", "ty"]),
        TriggerTypeExpr => CanonicalMapOrder::Sorted,
        TriggerTypeField => CanonicalMapOrder::Declared(&["name", "ty", "optional"]),
        TriggerObjectFields | TriggerUnionTypes => CanonicalMapOrder::Sorted,
        TriggerProcess => CanonicalMapOrder::Declared(&["kind", "params", "output"]),
        TriggerProcessParam => CanonicalMapOrder::Declared(&["name", "ty"]),
        TriggerProcessParams => CanonicalMapOrder::Sorted,
        Other => CanonicalMapOrder::Unordered,
    }
}

fn root_map_required(path: &[CanonicalPathSegment]) -> bool {
    !matches!(
        root_node(path),
        RootNode::Json
            | RootNode::Other
            | RootNode::TriggerTypeExpr
            | RootNode::TriggerObjectFields
            | RootNode::TriggerUnionTypes
            | RootNode::TriggerProcessParams
    )
}

fn body_prefers_leaf(encoded_len: usize) -> bool {
    // One source of truth, owned by the crate both sides of the checkpoint
    // contract depend on. Deliberately not a store blob-compression profile:
    // that answers whether bytes should be compressed, not whether a value is
    // worth its own component, and reading it here would make snapshot shape
    // depend on the configured backend.
    encoded_len >= lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES
}

fn persist_value_body(
    body: Vec<u8>,
    prior_leaf_keys: &BTreeSet<ExecutionLeafName>,
    changed_leaves: &mut BTreeMap<ExecutionLeafName, Arc<[u8]>>,
) -> PersistedValue {
    if body_prefers_leaf(body.len()) {
        let component = leaf_component_key(&body);
        if !prior_leaf_keys.contains(&component) {
            changed_leaves.insert(component.clone(), body.into());
        }
        PersistedValue::Leaf { component }
    } else {
        PersistedValue::Inline { body }
    }
}

fn leaf_component_key(body: &[u8]) -> ExecutionLeafName {
    ExecutionLeafName::new(format!(
        "blake3/{}",
        lash_sansio::core_support::blake3_domain_hash_hex(
            LASH_RLM_EXECUTION_STATE_LEAF_DOMAIN_VERSION,
            body,
        )
    ))
}

#[cfg(test)]
pub(super) fn measure_snapshot(
    snapshot: &lash_core::plugin::ExecutionStateCapture,
) -> lash_core::testing::RuntimeCommitBudgetMeasurement {
    let state = lash_core::RuntimeSessionState {
        session_id: lash_sansio::SessionId::from("fig-1257-snapshot-budget"),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    let mut commit = lash_core::RuntimeCommit::persisted_state_for_test(&state);
    commit.checkpoint.components.insert(
        "execution_state".to_string(),
        lash_core::HydratedCheckpointComponent::changed(
            snapshot.root().expect("snapshot root").clone(),
        ),
    );
    for (key, component) in snapshot.leaves() {
        let component = match component {
            lash_core::plugin::LeafChange::Changed(body) => {
                lash_core::HydratedCheckpointComponent::changed(body.clone())
            }
            lash_core::plugin::LeafChange::Unchanged => {
                let hash = key
                    .name()
                    .strip_prefix("blake3/")
                    .expect("RLM leaf component key");
                lash_core::HydratedCheckpointComponent::unchanged(
                    &lash_core::CheckpointComponentDescriptor {
                        blob_ref: lash_core::BlobRef(hash.to_string()),
                        encoding_version: lash_core::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
                    },
                )
            }
        };
        commit
            .checkpoint
            .components
            .insert(key.to_string(), component);
    }
    lash_core::testing::measure_runtime_commit_budget(&commit)
        .expect("measure RLM runtime commit budget")
}

fn resolve_leaf<'a>(
    state: &'a lash_core::plugin::HydratedExecutionState,
    logical_key: &str,
    component: &ExecutionLeafName,
) -> Result<&'a [u8], RlmSnapshotError> {
    let body = state
        .components
        .get(component)
        .map(Arc::as_ref)
        .ok_or_else(|| RlmSnapshotError::MissingLeaf {
            logical_key: logical_key.to_string(),
            component: component.clone(),
        })?;
    let actual_component = leaf_component_key(body);
    if &actual_component != component {
        return Err(RlmSnapshotError::LeafHashMismatch {
            logical_key: logical_key.to_string(),
            component: component.clone(),
            actual_component,
        });
    }
    Ok(body)
}

fn root_leaf_keys(root: &RlmSnapshotRoot) -> BTreeSet<ExecutionLeafName> {
    leaf_keys_for_values(&root.globals)
}

fn leaf_keys_for_values(values: &BTreeMap<String, PersistedValue>) -> BTreeSet<ExecutionLeafName> {
    values
        .values()
        .filter_map(|global| match global {
            PersistedValue::Inline { .. } => None,
            PersistedValue::Leaf { component } => Some(component.clone()),
        })
        .collect()
}

/// The guest half of a parsed root, with every leaf body resolved: what the
/// parent hands the worker on restore.
pub(super) fn worker_bound_envelope(
    state: &lash_core::plugin::HydratedExecutionState,
    root: &RlmSnapshotRoot,
) -> Result<RlmWorkerEnvelope, RlmSnapshotError> {
    let mut globals = BTreeMap::new();
    for (name, persisted) in &root.globals {
        let body = match persisted {
            PersistedValue::Inline { body } => body.as_slice(),
            PersistedValue::Leaf { component } => resolve_leaf(state, name, component)?,
        };
        globals.insert(name.clone(), ByteBuf::from(body.to_vec()));
    }
    Ok(RlmWorkerEnvelope {
        state_header: ByteBuf::from(root.state_header.clone()),
        globals,
    })
}

/// Which state a capture is relative to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// A checkpoint delta: re-encode the bindings whose fragments changed and
    /// reference every other leaf the receiver already holds.
    Incremental,
    /// Every binding, with every leaf body present. Relative to nothing, so it
    /// neither reads nor advances capture bookkeeping.
    Complete,
}

/// A capture that has been built but not yet installed as the pending capture.
struct PreparedCapture {
    snapshot: lash_core::plugin::ExecutionStateCapture,
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: BTreeMap<String, String>,
    leaf_keys: BTreeSet<ExecutionLeafName>,
    #[cfg(test)]
    encoded_globals: usize,
}

#[derive(Clone)]
struct CaptureRollback {
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: BTreeMap<String, String>,
    persisted_leaf_keys: BTreeSet<ExecutionLeafName>,
}

/// The live state and capture bookkeeping that one foreground cell may mutate.
///
/// Restoring only the serialized state would make a prior successful cell look
/// clean after a later cell is cancelled, allowing the prior cell to disappear
/// from the next cold snapshot.
pub(super) struct RlmExecutionCheckpoint {
    vm_state: lash_vm_client::RemoteState,
    deferred_trigger_resolutions: lash_lashlang_runtime::DeferredTriggerResolutionRecord,
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: BTreeMap<String, String>,
    persisted_leaf_keys: BTreeSet<ExecutionLeafName>,
    capture_dirty: bool,
    capture_rollback: Option<CaptureRollback>,
    pending_snapshot: Option<lash_core::plugin::ExecutionStateCapture>,
    #[cfg(test)]
    encoded_globals_in_last_snapshot: usize,
}

/// One RLM session's execution state, split by authority (ADR 0123).
///
/// `vm` is the worker's: the guest heap, roots and scratch, and the cells
/// compiled against them. Everything else is the parent's: the deferred
/// resolutions and their grants, the frame's module edges, and the capture
/// bookkeeping the durable root is assembled from. Guest state crosses to and
/// from the worker only as a [`RlmWorkerEnvelope`] or [`RlmWorkerCapture`].
pub struct RlmExecutionState {
    engine_id: Arc<str>,
    pub(super) vm: lash_vm_client::RemoteVm,
    /// The modules the current frame holds an edge of (ADR 0113 §3.1). A
    /// cache for one frame: a module first bound in a new frame acquires
    /// that frame's edge, and a cold restore starts it empty and re-acquires.
    frame_held_modules: Option<(lash_core::FrameEnvironmentId, BTreeSet<lashlang::ModuleRef>)>,
    /// A transient projection of the active link's journaled tool outcomes.
    /// A cold restore clears it; re-execution reads the journaled effect.
    pub(super) deferred_link: Option<lash_lashlang_runtime::DeferredLink>,
    /// Trigger-definition outcomes remain separate from tool grants so a
    /// mixed link cannot execute one provider family through the other.
    pub(super) deferred_trigger_resolutions: lash_lashlang_runtime::DeferredTriggerResolutionRecord,
    /// The body each binding's fragment was last captured as, and the
    /// baseline those bodies stand for. The two move together: a capture
    /// installs both, and a rollback or checkpoint restore rewinds both.
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: BTreeMap<String, String>,
    persisted_leaf_keys: BTreeSet<ExecutionLeafName>,
    /// Whether anything may have changed since the last installed capture: a
    /// cell ran, a patch or prune committed, the root's own records moved.
    /// Which fragments changed is the durable diff's question, not this flag's.
    capture_dirty: bool,
    capture_rollback: Option<CaptureRollback>,
    pending_snapshot: Option<lash_core::plugin::ExecutionStateCapture>,
    active_execution_checkpoint: Option<RlmExecutionCheckpoint>,
    execution_response_returned: bool,
    #[cfg(test)]
    encoded_globals_in_last_snapshot: usize,
}

impl RlmExecutionState {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::for_engine("typescript")
    }

    #[cfg(test)]
    pub(crate) fn for_engine(engine_id: impl Into<Arc<str>>) -> Self {
        Self::for_engine_with_workers(engine_id, lash_vm_client::service::Service::default())
    }
    pub(crate) fn for_engine_with_workers(
        engine_id: impl Into<Arc<str>>,
        workers: lash_vm_client::service::Service,
    ) -> Self {
        Self {
            engine_id: engine_id.into(),
            vm: lash_vm_client::RemoteVm::pristine(workers),
            frame_held_modules: None,
            deferred_link: None,
            deferred_trigger_resolutions:
                lash_lashlang_runtime::DeferredTriggerResolutionRecord::default(),
            persisted_globals: BTreeMap::new(),
            persisted_baseline: BTreeMap::default(),
            persisted_leaf_keys: BTreeSet::new(),
            capture_dirty: true,
            capture_rollback: None,
            pending_snapshot: None,
            active_execution_checkpoint: None,
            execution_response_returned: false,
            #[cfg(test)]
            encoded_globals_in_last_snapshot: 0,
        }
    }

    /// Whether `frame` is known to hold an edge of `module_ref`.
    pub(super) fn frame_holds(
        &self,
        frame: &lash_core::FrameEnvironmentId,
        module_ref: &lashlang::ModuleRef,
    ) -> bool {
        self.frame_held_modules
            .as_ref()
            .is_some_and(|(held_frame, modules)| {
                held_frame == frame && modules.contains(module_ref)
            })
    }

    /// The modules the current frame is known to hold.
    #[cfg(test)]
    pub(super) fn frame_held_module_refs(&self) -> impl Iterator<Item = &lashlang::ModuleRef> {
        self.frame_held_modules
            .iter()
            .flat_map(|(_, modules)| modules.iter())
    }

    /// Record that `frame` holds an edge of `module_ref`, forgetting what an
    /// earlier frame held.
    pub(super) fn record_frame_hold(
        &mut self,
        frame: &lash_core::FrameEnvironmentId,
        module_ref: lashlang::ModuleRef,
    ) {
        match &mut self.frame_held_modules {
            Some((held_frame, modules)) if held_frame == frame => {
                modules.insert(module_ref);
            }
            held => *held = Some((frame.clone(), BTreeSet::from([module_ref]))),
        }
    }

    pub fn execution_state_dirty(&self) -> bool {
        self.pending_snapshot.is_some() || self.capture_dirty
    }

    pub(super) fn mark_execution_started(&mut self) {
        debug_assert!(
            !self.execution_response_returned,
            "a returned code execution must be settled before another cell starts"
        );
        self.accept_code_execution();
        self.capture_dirty = true;
    }

    pub(super) fn execution_checkpoint(&self) -> RlmExecutionCheckpoint {
        RlmExecutionCheckpoint {
            vm_state: self.vm.state().clone(),
            deferred_trigger_resolutions: self.deferred_trigger_resolutions.clone(),
            persisted_globals: self.persisted_globals.clone(),
            persisted_baseline: self.persisted_baseline.clone(),
            persisted_leaf_keys: self.persisted_leaf_keys.clone(),
            capture_dirty: self.capture_dirty,
            capture_rollback: self.capture_rollback.clone(),
            pending_snapshot: self.pending_snapshot.clone(),
            #[cfg(test)]
            encoded_globals_in_last_snapshot: self.encoded_globals_in_last_snapshot,
        }
    }

    fn restore_execution_checkpoint(&mut self, checkpoint: RlmExecutionCheckpoint) {
        self.vm.replace_state(checkpoint.vm_state);
        self.deferred_link = None;
        self.deferred_trigger_resolutions = checkpoint.deferred_trigger_resolutions;
        self.persisted_globals = checkpoint.persisted_globals;
        self.persisted_baseline = checkpoint.persisted_baseline;
        self.persisted_leaf_keys = checkpoint.persisted_leaf_keys;
        self.capture_dirty = checkpoint.capture_dirty;
        self.capture_rollback = checkpoint.capture_rollback;
        self.pending_snapshot = checkpoint.pending_snapshot;
        #[cfg(test)]
        {
            self.encoded_globals_in_last_snapshot = checkpoint.encoded_globals_in_last_snapshot;
        }
        self.active_execution_checkpoint = None;
        self.execution_response_returned = false;
    }

    pub(super) fn begin_code_execution(&mut self, checkpoint: RlmExecutionCheckpoint) {
        debug_assert!(self.active_execution_checkpoint.is_none());
        self.active_execution_checkpoint = Some(checkpoint);
        self.execution_response_returned = false;
    }

    pub(crate) fn prepare_runtime_code_execution(&mut self) -> Result<(), &'static str> {
        if self.active_execution_checkpoint.is_none() {
            return Ok(());
        }
        if self.execution_response_returned {
            return Err("the previous code execution response has not been settled");
        }
        self.rollback_code_execution();
        Ok(())
    }

    pub(crate) fn mark_code_execution_response_returned(&mut self) {
        self.execution_response_returned = true;
    }

    pub(crate) fn accept_code_execution(&mut self) {
        self.active_execution_checkpoint = None;
        self.execution_response_returned = false;
    }

    pub(crate) fn rollback_code_execution(&mut self) {
        let Some(checkpoint) = self.active_execution_checkpoint.take() else {
            self.execution_response_returned = false;
            return;
        };
        self.restore_execution_checkpoint(checkpoint);
    }

    /// Cancellation rolls the execution back; the cell's snapshot is its
    /// execution's, not the session's.
    pub(crate) fn terminate_code_execution(&mut self) {
        self.rollback_code_execution();
    }

    /// Encode the canonical RLM root and only the leaf bodies whose logical
    /// values were assigned since the previous capture.
    ///
    /// `fleet_format` is the `F` the bound session's store recorded: the root
    /// and every durable part stamp `F`'s writer versions (FIG-3796), never
    /// the bare build constants.
    pub async fn snapshot_execution_state(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, SessionError> {
        if !self.capture_dirty
            && let Some(snapshot) = &self.pending_snapshot
        {
            return Ok(snapshot.clone());
        }
        let prepared = self
            .build_capture(CaptureMode::Incremental, fleet_format)
            .await?;
        Ok(self.install_capture(prepared))
    }

    /// Answer whether the capture `snapshot_execution_state` would take right
    /// now can succeed, without staging it.
    ///
    /// The whole fallible part of a capture is building it — canonical encoding
    /// of every changed fragment — so this proves capturability by building the
    /// same capture and dropping it. It advances no capture bookkeeping.
    pub async fn probe_execution_state_capture(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        if !self.capture_dirty && self.pending_snapshot.is_some() {
            return Ok(());
        }
        self.build_capture(CaptureMode::Incremental, fleet_format)
            .await
            .map(|_| ())
    }

    /// The complete live execution state, with every leaf body present and no
    /// capture bookkeeping touched. Explicit administrative snapshot uses this;
    /// it is relative to nothing, so it never depends on which leaf bodies are
    /// still resident in the runtime's checkpoint state.
    pub async fn hydrated_execution_state(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        let prepared = self
            .build_capture(CaptureMode::Complete, fleet_format)
            .await?;
        let lash_core::plugin::ExecutionStateCapture::Replace { root, leaves } = prepared.snapshot
        else {
            return Err(SessionError::Protocol(
                "RLM root was not encoded".to_string(),
            ));
        };
        let mut components = BTreeMap::new();
        for (key, component) in leaves {
            match component {
                lash_core::plugin::LeafChange::Changed(body) => {
                    components.insert(key, body);
                }
                lash_core::plugin::LeafChange::Unchanged => {
                    return Err(SessionError::Protocol(format!(
                        "complete RLM execution state referenced leaf `{key}` without its body"
                    )));
                }
            }
        }
        Ok(lash_core::plugin::HydratedExecutionState { root, components })
    }

    async fn build_capture(
        &self,
        mode: CaptureMode,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<PreparedCapture, SessionError> {
        let complete = mode == CaptureMode::Complete;
        let complete_baseline = BTreeMap::new();
        // The worker captures its guest state; the parent reads the capture
        // back structurally and assembles the root from it and its own
        // authority.
        let parts = self
            .vm
            .state()
            .capture(
                if complete {
                    &complete_baseline
                } else {
                    &self.persisted_baseline
                },
                fleet_format,
            )
            .await
            .map_err(worker_session_error)?;
        let baseline = parts.baseline;
        let mut capture = RlmWorkerCapture {
            state_header: ByteBuf::from(parts.header),
            changed: BTreeMap::new(),
            unchanged: BTreeSet::new(),
        };
        for (name, fragment) in parts.fragments {
            match fragment {
                lashlang::DurableFragment::Changed(body) => {
                    capture.changed.insert(name, ByteBuf::from(body));
                }
                lashlang::DurableFragment::Unchanged => {
                    capture.unchanged.insert(name);
                }
            }
        }
        let capture = capture.encode();
        let capture = RlmWorkerCapture::accept(&capture)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        // Leaves the receiver of this capture already holds. A staged but
        // uncommitted capture supersedes the durable set, so a leaf it evicted
        // must be resent even though it is still durable.
        let prior_leaf_keys = if complete {
            BTreeSet::new()
        } else {
            self.pending_snapshot
                .as_ref()
                .map(|snapshot| snapshot.leaves().keys().cloned().collect())
                .unwrap_or_else(|| self.persisted_leaf_keys.clone())
        };
        let mut changed_leaves = if complete {
            BTreeMap::new()
        } else {
            self.pending_snapshot
                .as_ref()
                .into_iter()
                .flat_map(|snapshot| snapshot.leaves())
                .filter_map(|(key, component)| match component {
                    lash_core::plugin::LeafChange::Changed(body) => {
                        Some((key.clone(), body.clone()))
                    }
                    lash_core::plugin::LeafChange::Unchanged => None,
                })
                .collect::<BTreeMap<_, _>>()
        };
        #[cfg(test)]
        let mut encoded_globals = 0;
        let mut next_globals = BTreeMap::new();
        for (name, body) in capture.changed {
            #[cfg(test)]
            {
                encoded_globals += 1;
            }
            let persisted =
                persist_value_body(body.into_vec(), &prior_leaf_keys, &mut changed_leaves);
            next_globals.insert(name, persisted);
        }
        for name in capture.unchanged {
            let persisted = self.persisted_globals.get(&name).cloned().ok_or_else(|| {
                SessionError::Protocol(format!(
                    "RLM global `{name}` is unchanged since a capture that recorded no body for it"
                ))
            })?;
            next_globals.insert(name, persisted);
        }

        let root = RlmSnapshotRoot {
            version: fleet_format.writer_version(lash_core::surface_format!(RLM_SNAPSHOT_VERSION)),
            engine: self.engine_id.to_string(),
            state_header: capture.state_header.into_vec(),
            globals: next_globals.clone(),
            deferred_trigger_resolutions: self.deferred_trigger_resolutions.clone(),
        };
        let encoded = rmp_serde::to_vec_named(&root).map_err(|error| {
            SessionError::Protocol(format!("failed to encode RLM snapshot root: {error}"))
        })?;
        validate_canonical_root(&encoded).map_err(|error| {
            SessionError::Protocol(format!("failed to encode canonical RLM root: {error}"))
        })?;

        let leaf_keys = root_leaf_keys(&root);
        let leaves = leaf_keys
            .iter()
            .map(|key| {
                let change = match changed_leaves.remove(key) {
                    Some(body) => lash_core::plugin::LeafChange::Changed(body),
                    None => lash_core::plugin::LeafChange::Unchanged,
                };
                (key.clone(), change)
            })
            .collect();
        let snapshot = lash_core::plugin::ExecutionStateCapture::Replace {
            root: encoded.into(),
            leaves,
        };
        Ok(PreparedCapture {
            snapshot,
            persisted_globals: next_globals,
            persisted_baseline: baseline,
            leaf_keys,
            #[cfg(test)]
            encoded_globals,
        })
    }

    /// Make a built capture the pending capture: the caches it computed become
    /// authoritative, and the first capture since the last settlement records
    /// the rollback baseline an aborted commit restores.
    fn install_capture(
        &mut self,
        prepared: PreparedCapture,
    ) -> lash_core::plugin::ExecutionStateCapture {
        let PreparedCapture {
            snapshot,
            persisted_globals,
            persisted_baseline,
            leaf_keys,
            #[cfg(test)]
            encoded_globals,
        } = prepared;
        let rollback = CaptureRollback {
            persisted_globals: std::mem::replace(&mut self.persisted_globals, persisted_globals),
            persisted_baseline: std::mem::replace(&mut self.persisted_baseline, persisted_baseline),
            persisted_leaf_keys: std::mem::replace(&mut self.persisted_leaf_keys, leaf_keys),
        };
        if self.capture_rollback.is_none() {
            self.capture_rollback = Some(rollback);
        }
        self.capture_dirty = false;
        #[cfg(test)]
        {
            self.encoded_globals_in_last_snapshot = encoded_globals;
        }
        self.pending_snapshot = Some(snapshot.clone());
        snapshot
    }

    pub(crate) fn acknowledge_execution_state_capture(&mut self) {
        self.capture_rollback = None;
        self.pending_snapshot = None;
    }

    pub(crate) fn abort_execution_state_capture(&mut self) {
        let Some(rollback) = self.capture_rollback.take() else {
            return;
        };
        // The durable set is the rollback's: its bodies and the baseline they
        // stand for, so the next capture diffs against what the store holds.
        self.persisted_globals = rollback.persisted_globals;
        self.persisted_baseline = rollback.persisted_baseline;
        self.persisted_leaf_keys = rollback.persisted_leaf_keys;
        self.capture_dirty = true;
        self.pending_snapshot = None;
    }

    /// Restores a persisted execution-state snapshot.
    ///
    /// `fleet_format` is the `F` the bound store recorded: the read admits
    /// every version of the snapshot surface's read window (FIG-3796,
    /// FIG-3802) — the newest, and each older version a `Lift::Decoder` row
    /// registers, which this canonical binary root reads natively. The root's
    /// bytes and its leaves' identities are read as stored.
    pub async fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), RlmSnapshotError> {
        let window = fleet_format.read_window(lash_core::surface_format!(RLM_SNAPSHOT_VERSION));
        let found_version = probe_snapshot_version(&state.root)?;
        if !window.admits(found_version) {
            return Err(RlmSnapshotError::VersionMismatch {
                expected: window.newest(),
                found: found_version,
            });
        }
        validate_canonical_root(&state.root)?;
        let parsed: RlmSnapshotRoot = rmp_serde::from_slice(&state.root).map_err(|error| {
            RlmSnapshotError::FormatMismatch {
                details: error.to_string(),
            }
        })?;

        if !window.admits(parsed.version) {
            return Err(RlmSnapshotError::VersionMismatch {
                expected: window.newest(),
                found: parsed.version,
            });
        }
        if parsed.engine != self.engine_id.as_ref() {
            return Err(RlmSnapshotError::EngineMismatch {
                expected: self.engine_id.to_string(),
                found: parsed.engine,
            });
        }

        let expected_leaf_keys = root_leaf_keys(&parsed);
        let supplied_leaf_keys = state.components.keys().cloned().collect::<BTreeSet<_>>();
        if expected_leaf_keys != supplied_leaf_keys {
            return Err(RlmSnapshotError::LeafSetMismatch {
                missing: expected_leaf_keys
                    .difference(&supplied_leaf_keys)
                    .cloned()
                    .collect(),
                unexpected: supplied_leaf_keys
                    .difference(&expected_leaf_keys)
                    .cloned()
                    .collect(),
            });
        }

        let envelope = worker_bound_envelope(state, &parsed)?;
        let mut restored = lash_vm_client::RemoteState::pristine(self.vm.state().service().clone());
        let baseline = restored
            .restore(
                envelope.state_header.into_vec(),
                envelope
                    .globals
                    .into_iter()
                    .map(|(name, bytes)| (name, bytes.into_vec()))
                    .collect(),
                fleet_format,
            )
            .await
            .map_err(|error| match error {
                lash_vm_client::RemoteRestoreError::Snapshot(error) => {
                    RlmSnapshotError::Lashlang(error)
                }
                lash_vm_client::RemoteRestoreError::Worker(error) => {
                    RlmSnapshotError::WorkerUnavailable(error)
                }
            })?;
        restored
            .remove_names(BTreeSet::from(["history".to_string()]))
            .await
            .map_err(RlmSnapshotError::WorkerUnavailable)?;

        let next_live_names = restored
            .binding_names()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        self.vm.replace_state(restored);
        let pruned_reserved = parsed.globals.len() != next_live_names.len();
        self.deferred_link = None;
        self.deferred_trigger_resolutions = parsed.deferred_trigger_resolutions;
        self.persisted_leaf_keys = expected_leaf_keys;
        self.persisted_globals = parsed.globals;
        self.persisted_baseline = baseline;
        self.capture_dirty = pruned_reserved;
        self.capture_rollback = None;
        self.pending_snapshot = None;
        self.active_execution_checkpoint = None;
        self.execution_response_returned = false;
        Ok(())
    }

    pub async fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        let before = self.vm.state().binding_names().count();
        let names = protected_names
            .iter()
            .cloned()
            .chain(std::iter::once("history".to_string()))
            .collect();
        self.vm
            .state_mut()
            .remove_names(names)
            .await
            .map_err(worker_session_error)?;
        if self.vm.state().binding_names().count() != before {
            self.capture_dirty = true;
        }
        Ok(())
    }

    pub async fn patch_globals(
        &mut self,
        patch: &lash_rlm_types::RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        if patch.is_empty() {
            return Ok(());
        }
        // The state commits the whole batch or none of it, so the dirty
        // bookkeeping is recorded from what the commit reports rather than
        // reconstructed afterwards. A rejected patch leaves both untouched.
        let inserted = apply_global_defaults(self.vm.state_mut(), patch, protected_names).await?;
        if !inserted.is_empty() {
            self.capture_dirty = true;
        }
        Ok(())
    }

    /// Every binding the session holds, read from the runtime roots that own
    /// them rather than the host view, which omits a binding with no host
    /// shape (ADR 0076).
    #[cfg(test)]
    pub(crate) fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.vm.state().binding_names()
    }

    /// The globals a cell boundary dropped for holding a function, which a
    /// later cell's reference is refused by name for.
    #[cfg(test)]
    pub(crate) fn expired_functions(&self) -> &BTreeSet<String> {
        self.vm.state().expired_functions()
    }

    /// How many globals the last capture encoded afresh.
    #[cfg(test)]
    pub(super) fn encoded_globals_in_last_snapshot(&self) -> usize {
        self.encoded_globals_in_last_snapshot
    }

    /// The bindings the "Bound Variables" section shows by summary: the ones
    /// with no host view (ADR 0076), each with its bounded runtime summary,
    /// under the same exclusions as [`Self::bound_variable_values`].
    pub(crate) fn opaque_bound_variables(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Vec<(String, String)> {
        self.vm
            .state()
            .opaque_bindings()
            .into_iter()
            .filter(|(name, _)| name != "history" && !exclude.contains(name))
            .collect()
    }

    /// The live top-level variable namespace as JSON for the "Bound Variables"
    /// prompt section: the model's own scratch variables plus any seeded
    /// computed globals, which are the same kind of value and render the same
    /// way.
    ///
    /// Excludes the reserved `history` binding, the supplied `exclude` names
    /// (read-only values, which get their own type-only section), and any
    /// value that contains read-only projected data. Those are never
    /// materialized for a value preview here.
    pub(crate) fn bound_variable_values(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Vec<(String, FlowValue)> {
        let mut out = Vec::new();
        for (name, value) in self.vm.state().globals().iter() {
            if name == "history" || exclude.contains(name) || value.contains_projected() {
                continue;
            }
            out.push((name.to_string(), value.clone()));
        }
        out
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod guarded_surface_tests;

#[cfg(test)]
mod authority_split_tests;

fn worker_session_error(error: lash_vm_client::PoolError) -> SessionError {
    SessionError::Plugin(lash_core::PluginError::Runtime(error.into_runtime_error()))
}
