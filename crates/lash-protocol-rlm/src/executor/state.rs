use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::SessionError;
use lashlang::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, CanonicalPathSegment, DurableBaseline,
    DurableFragment, ExecutionScratch, SnapshotDecodeError, State as FlowState, Value as FlowValue,
    validate_canonical_messagepack_structure,
};
use serde::{Deserialize, Serialize};

use crate::projection::{prune_protected_bindings, prune_reserved_projected_bindings};

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
    deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
    deferred_trigger_resolutions: lash_lashlang_runtime::DeferredTriggerResolutionRecord,
    /// Attempt bound this execution stamps onto the children its code starts,
    /// pinned the first time a cell needs one. `None` means no cell has started
    /// a child yet, so nothing is recorded to preserve.
    child_max_attempts: Option<std::num::NonZeroU32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedValue {
    Inline {
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Leaf {
        component: String,
    },
}

include!(concat!(env!("OUT_DIR"), "/rlm_snapshot_fields.rs"));

// Serialized field order of the snapshot's dependency-owned nodes.
//
// These types live in `lash-sansio` and `lash-lashlang-runtime`, and the build
// script used to derive each list by serializing an all-fields-set witness.
// That put both crates -- and every first-party crate beneath them -- into
// Bazel's exec configuration, compiled a second time at `opt-level=3` for an
// output no product artifact consumes (FIG-3032). The lists are declared here
// instead, and `generated_snapshot_field_schemas_match_all_fields_set_serialization`
// re-derives every one of them from the same witnesses at test time, so a
// field added, removed or reordered upstream still fails the suite rather than
// silently moving the canonical envelope.

/// `lash_lashlang_runtime::DeferredResolutionRecord`.
const DEFERRED_RESOLUTION_FIELDS: &[&str] = &["link_key", "resolutions"];

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

/// `lash_lashlang_runtime::Resolution`.
const RESOLUTION_FIELDS: &[&str] = &["kind", "definition", "source_id", "execution_binding"];

/// `lash_sansio::ToolDefinition`.
const TOOL_DEFINITION_FIELDS: &[&str] = &["manifest", "contract"];

/// `lash_sansio::ToolManifest`.
const TOOL_MANIFEST_FIELDS: &[&str] = &[
    "inline",
    "id",
    "name",
    "description",
    "compact_contract",
    "activation",
    "bindings",
    "argument_projection",
    "retry_policy",
];

/// `lash_sansio::ToolContract`. The skipped `identity` and `compact_cache`
/// fields never serialize, so they do not appear here.
const TOOL_CONTRACT_FIELDS: &[&str] = &[
    "input_schema",
    "output_schema",
    "output_contract",
    "examples",
];

/// `lash_sansio::SchemaContract`.
const SCHEMA_CONTRACT_FIELDS: &[&str] = &["canonical", "projection"];

/// `lash_sansio::SchemaProjectionPolicy`.
const SCHEMA_PROJECTION_FIELDS: &[&str] = &["mode", "overrides"];

/// `lash_sansio::SchemaProjectionOverride`.
const SCHEMA_OVERRIDE_FIELDS: &[&str] = &["dialect", "schema"];

/// `lash_sansio::CompactToolContract`.
const COMPACT_CONTRACT_FIELDS: &[&str] = &[
    "name",
    "signature",
    "returns",
    "parameters",
    "return_fields",
    "description",
    "examples",
];

/// `lash_sansio::ToolRetryPolicy`.
const RETRY_POLICY_FIELDS: &[&str] = &["type", "max_attempts", "base_delay_ms", "max_delay_ms"];

/// `lash_sansio::ToolOutputContract`.
const OUTPUT_CONTRACT_FIELDS: &[&str] = &["kind", "input_field", "default_schema"];

/// `lash_sansio::ToolArgumentProjectionPolicy`.
const ARGUMENT_PROJECTION_FIELDS: &[&str] = &["kind", "field"];

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
    Deferred,
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
    Definition,
    Manifest,
    Contract,
    SchemaContract,
    Projection,
    Overrides,
    Override,
    CompactContract,
    RetryPolicy,
    OutputContract,
    ArgumentProjection,
    Json,
    Other,
}

impl RootNode {
    fn child(self, segment: &CanonicalPathSegment) -> Self {
        use RootNode::*;
        match (self, segment.key()) {
            (Root, Some("globals")) => Globals,
            (Root, Some("deferred_resolutions")) => Deferred,
            (Root, Some("deferred_trigger_resolutions")) => DeferredTrigger,
            (Globals, Some(_)) => Global,
            (Deferred, Some("link_key")) => LinkKey,
            (Deferred, Some("resolutions")) => Resolutions,
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
            (Resolution, Some("definition")) => Definition,
            (Resolution, Some("execution_binding")) => Json,
            (Definition, Some("manifest")) => Manifest,
            (Definition, Some("contract")) => Contract,
            (Manifest, Some("bindings")) => Json,
            (Manifest, Some("compact_contract")) => CompactContract,
            (Manifest, Some("retry_policy")) => RetryPolicy,
            (Manifest, Some("argument_projection")) => ArgumentProjection,
            (Contract, Some("input_schema" | "output_schema")) => SchemaContract,
            (Contract, Some("output_contract")) => OutputContract,
            (SchemaContract, Some("canonical")) => Json,
            (SchemaContract, Some("projection")) => Projection,
            (Projection, Some("overrides")) => Overrides,
            (Overrides, None) => Override,
            (Override, Some("schema")) => Json,
            (CompactContract, Some("parameters" | "return_fields")) => Json,
            (OutputContract, Some("default_schema")) => Json,
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
        Deferred => CanonicalMapOrder::Declared(DEFERRED_RESOLUTION_FIELDS),
        DeferredTrigger => CanonicalMapOrder::Declared(DEFERRED_TRIGGER_RESOLUTION_FIELDS),
        LinkKey => CanonicalMapOrder::Declared(DEFERRED_LINK_KEY_FIELDS),
        Resolution => {
            let path_is_trigger = path.first().and_then(CanonicalPathSegment::key)
                == Some("deferred_trigger_resolutions");
            if path_is_trigger {
                CanonicalMapOrder::Declared(TRIGGER_RESOLUTION_FIELDS)
            } else {
                CanonicalMapOrder::Declared(RESOLUTION_FIELDS)
            }
        }
        TriggerEventType => CanonicalMapOrder::Declared(&["name", "ty"]),
        TriggerTypeExpr => CanonicalMapOrder::Sorted,
        TriggerTypeField => CanonicalMapOrder::Declared(&["name", "ty", "optional"]),
        TriggerObjectFields | TriggerUnionTypes => CanonicalMapOrder::Sorted,
        TriggerProcess => CanonicalMapOrder::Declared(&["kind", "params", "output"]),
        TriggerProcessParam => CanonicalMapOrder::Declared(&["name", "ty"]),
        TriggerProcessParams => CanonicalMapOrder::Sorted,
        Definition => CanonicalMapOrder::Declared(TOOL_DEFINITION_FIELDS),
        Manifest => CanonicalMapOrder::Declared(TOOL_MANIFEST_FIELDS),
        Contract => CanonicalMapOrder::Declared(TOOL_CONTRACT_FIELDS),
        SchemaContract => CanonicalMapOrder::Declared(SCHEMA_CONTRACT_FIELDS),
        Projection => CanonicalMapOrder::Declared(SCHEMA_PROJECTION_FIELDS),
        Override => CanonicalMapOrder::Declared(SCHEMA_OVERRIDE_FIELDS),
        CompactContract => CanonicalMapOrder::Declared(COMPACT_CONTRACT_FIELDS),
        RetryPolicy => CanonicalMapOrder::Declared(RETRY_POLICY_FIELDS),
        OutputContract => CanonicalMapOrder::Declared(OUTPUT_CONTRACT_FIELDS),
        ArgumentProjection => CanonicalMapOrder::Declared(ARGUMENT_PROJECTION_FIELDS),
        Overrides | Other => CanonicalMapOrder::Unordered,
    }
}

fn root_map_required(path: &[CanonicalPathSegment]) -> bool {
    !matches!(
        root_node(path),
        RootNode::Overrides
            | RootNode::Json
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
    prior_leaf_keys: &BTreeSet<String>,
    changed_leaves: &mut BTreeMap<String, Arc<[u8]>>,
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

fn leaf_component_key(body: &[u8]) -> String {
    format!(
        "execution_state/blake3/{}",
        lash_sansio::core_support::blake3_domain_hash_hex("lash-rlm-execution-state-leaf/v2", body,)
    )
}

#[cfg(test)]
pub(super) fn measure_snapshot(
    snapshot: &lash_core::plugin::ExecutionStateSnapshot,
) -> lash_core::testing::RuntimeCommitBudgetMeasurement {
    let state = lash_core::RuntimeSessionState {
        session_id: lash_sansio::SessionId::from("fig-1257-snapshot-budget"),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let mut commit = lash_core::RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        "execution_state".to_string(),
        lash_core::HydratedCheckpointComponent::changed(
            snapshot.root.clone().expect("snapshot root"),
        ),
    );
    for (key, component) in &snapshot.components {
        let component = match component {
            lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => {
                lash_core::HydratedCheckpointComponent::changed(body.clone())
            }
            lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                let hash = key
                    .strip_prefix("execution_state/blake3/")
                    .expect("RLM leaf component key");
                lash_core::HydratedCheckpointComponent::unchanged(
                    &lash_core::CheckpointComponentDescriptor {
                        blob_ref: lash_core::BlobRef(hash.to_string()),
                        encoding_version: lash_core::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
                    },
                )
            }
        };
        commit.checkpoint.components.insert(key.clone(), component);
    }
    lash_core::testing::measure_runtime_commit_budget(&commit)
        .expect("measure RLM runtime commit budget")
}

fn resolve_leaf<'a>(
    state: &'a lash_core::plugin::HydratedExecutionState,
    logical_key: &str,
    component: &str,
) -> Result<&'a [u8], RlmSnapshotError> {
    let body = state
        .components
        .get(component)
        .map(Arc::as_ref)
        .ok_or_else(|| RlmSnapshotError::MissingLeaf {
            logical_key: logical_key.to_string(),
            component: component.to_string(),
        })?;
    let actual_component = leaf_component_key(body);
    if actual_component != component {
        return Err(RlmSnapshotError::LeafHashMismatch {
            logical_key: logical_key.to_string(),
            component: component.to_string(),
            actual_component,
        });
    }
    Ok(body)
}

fn root_leaf_keys(root: &RlmSnapshotRoot) -> BTreeSet<String> {
    leaf_keys_for_values(&root.globals)
}

fn leaf_keys_for_values(values: &BTreeMap<String, PersistedValue>) -> BTreeSet<String> {
    values
        .values()
        .filter_map(|global| match global {
            PersistedValue::Inline { .. } => None,
            PersistedValue::Leaf { component } => Some(component.clone()),
        })
        .collect()
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
    snapshot: lash_core::plugin::ExecutionStateSnapshot,
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: DurableBaseline,
    leaf_keys: BTreeSet<String>,
    #[cfg(test)]
    encoded_globals: usize,
}

#[derive(Clone)]
struct CaptureRollback {
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: DurableBaseline,
    persisted_leaf_keys: BTreeSet<String>,
}

/// The live state and capture bookkeeping that one foreground cell may mutate.
///
/// Restoring only the serialized state would make a prior successful cell look
/// clean after a later cell is cancelled, allowing the prior cell to disappear
/// from the next cold snapshot.
pub(super) struct RlmExecutionCheckpoint {
    rlm: FlowState,
    deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
    deferred_trigger_resolutions: lash_lashlang_runtime::DeferredTriggerResolutionRecord,
    child_max_attempts: Option<std::num::NonZeroU32>,
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: DurableBaseline,
    persisted_leaf_keys: BTreeSet<String>,
    capture_dirty: bool,
    capture_rollback: Option<CaptureRollback>,
    pending_snapshot: Option<lash_core::plugin::ExecutionStateSnapshot>,
    #[cfg(test)]
    encoded_globals_in_last_snapshot: usize,
}

pub struct RlmExecutionState {
    engine_id: Arc<str>,
    pub(super) rlm: FlowState,
    pub(super) scratch: ExecutionScratch,
    pub(super) linked_programs: lashlang::LinkedProgramCache,
    pub(super) stored_lashlang_modules: BTreeSet<lashlang::ModuleRef>,
    /// Active-link record of deferred tool resolutions, keyed by Lashlang
    /// call-path. Snapshotted/restored with the rest of the execution state so
    /// a re-driven or recovered link replays the recorded grants and
    /// `NotAvailable` results without leaking them into a later code effect.
    pub(super) deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
    /// Trigger-definition outcomes remain separate from tool grants so a
    /// mixed link cannot execute one provider family through the other.
    pub(super) deferred_trigger_resolutions: lash_lashlang_runtime::DeferredTriggerResolutionRecord,
    /// Attempt bound stamped onto children this execution's code starts. Pinned
    /// from the host config by the first cell that actually starts a child and
    /// then replayed from the durable snapshot, so a later cell keeps the
    /// recorded value. It is an optimisation, not the source of truth: an
    /// already-registered child re-registers with the bound on its registry
    /// row, which is what keeps a redrive's fingerprint stable.
    child_max_attempts: Option<std::num::NonZeroU32>,
    /// The body each binding's fragment was last captured as, and the
    /// baseline those bodies stand for. The two move together: a capture
    /// installs both, and a rollback or checkpoint restore rewinds both.
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_baseline: DurableBaseline,
    persisted_leaf_keys: BTreeSet<String>,
    /// Whether anything may have changed since the last installed capture: a
    /// cell ran, a patch or prune committed, the root's own records moved.
    /// Which fragments changed is the durable diff's question, not this flag's.
    capture_dirty: bool,
    capture_rollback: Option<CaptureRollback>,
    pending_snapshot: Option<lash_core::plugin::ExecutionStateSnapshot>,
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

    pub(crate) fn for_engine(engine_id: impl Into<Arc<str>>) -> Self {
        Self {
            engine_id: engine_id.into(),
            rlm: FlowState::new(),
            scratch: ExecutionScratch::new(),
            linked_programs: lashlang::LinkedProgramCache::new(),
            stored_lashlang_modules: BTreeSet::new(),
            deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord::default(),
            deferred_trigger_resolutions:
                lash_lashlang_runtime::DeferredTriggerResolutionRecord::default(),
            child_max_attempts: None,
            persisted_globals: BTreeMap::new(),
            persisted_baseline: DurableBaseline::default(),
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

    /// The attempt bound an earlier cell of this execution pinned, if any.
    pub(super) fn child_max_attempts(&self) -> Option<std::num::NonZeroU32> {
        self.child_max_attempts
    }

    /// A cell that started no child pins nothing and leaves the snapshot root
    /// clean; the first cell that does start one dirties the root exactly once
    /// so the value rides the durable snapshot for later cells. The pin is
    /// write-once: an already-pinned execution keeps its value even if a cell
    /// reports a different one, so a host default that moved mid-execution
    /// cannot rewrite the bound a sibling child already registered with.
    pub(super) fn adopt_child_max_attempts(&mut self, pinned: Option<std::num::NonZeroU32>) {
        if self.child_max_attempts.is_some() {
            return;
        }
        if let Some(pinned) = pinned {
            self.child_max_attempts = Some(pinned);
            self.capture_dirty = true;
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
            rlm: self.rlm.clone(),
            deferred_resolutions: self.deferred_resolutions.clone(),
            deferred_trigger_resolutions: self.deferred_trigger_resolutions.clone(),
            child_max_attempts: self.child_max_attempts,
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
        self.rlm = checkpoint.rlm;
        self.deferred_resolutions = checkpoint.deferred_resolutions;
        self.deferred_trigger_resolutions = checkpoint.deferred_trigger_resolutions;
        self.child_max_attempts = checkpoint.child_max_attempts;
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
        self.cancel_code_execution();
        Ok(())
    }

    pub(crate) fn mark_code_execution_response_returned(&mut self) {
        self.execution_response_returned = true;
    }

    pub(crate) fn accept_code_execution(&mut self) {
        self.active_execution_checkpoint = None;
        self.execution_response_returned = false;
    }

    pub(crate) fn cancel_code_execution(&mut self) {
        let Some(checkpoint) = self.active_execution_checkpoint.take() else {
            self.execution_response_returned = false;
            return;
        };
        self.restore_execution_checkpoint(checkpoint);
        self.scratch = ExecutionScratch::new();
    }

    /// Encode the canonical RLM root and only the leaf bodies whose logical
    /// values were assigned since the previous capture.
    ///
    /// `fleet_format` is the `F` the bound session's store recorded: the root
    /// and every durable part stamp `F`'s writer versions (FIG-3796), never
    /// the bare build constants.
    pub fn snapshot_execution_state(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        if !self.capture_dirty
            && let Some(snapshot) = &self.pending_snapshot
        {
            return Ok(snapshot.clone());
        }
        let prepared = self.build_capture(CaptureMode::Incremental, fleet_format)?;
        Ok(self.install_capture(prepared))
    }

    /// Answer whether the capture `snapshot_execution_state` would take right
    /// now can succeed, without staging it.
    ///
    /// The whole fallible part of a capture is building it — canonical encoding
    /// of every changed fragment — so this proves capturability by building the
    /// same capture and dropping it. It advances no capture bookkeeping.
    pub fn probe_execution_state_capture(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        if !self.capture_dirty && self.pending_snapshot.is_some() {
            return Ok(());
        }
        self.build_capture(CaptureMode::Incremental, fleet_format)
            .map(|_| ())
    }

    /// The complete live execution state, with every leaf body present and no
    /// capture bookkeeping touched. Explicit administrative snapshot uses this;
    /// it is relative to nothing, so it never depends on which leaf bodies are
    /// still resident in the runtime's checkpoint state.
    pub fn hydrated_execution_state(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        let prepared = self.build_capture(CaptureMode::Complete, fleet_format)?;
        let mut components = BTreeMap::new();
        for (key, component) in prepared.snapshot.components {
            match component {
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => {
                    components.insert(key, body);
                }
                lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                    return Err(SessionError::Protocol(format!(
                        "complete RLM execution state referenced leaf `{key}` without its body"
                    )));
                }
            }
        }
        Ok(lash_core::plugin::HydratedExecutionState {
            root: prepared
                .snapshot
                .root
                .ok_or_else(|| SessionError::Protocol("RLM root was not encoded".to_string()))?,
            components,
        })
    }

    fn build_capture(
        &self,
        mode: CaptureMode,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<PreparedCapture, SessionError> {
        let complete = mode == CaptureMode::Complete;
        let complete_baseline = DurableBaseline::default();
        let parts = self
            .rlm
            .durable_parts(
                if complete {
                    &complete_baseline
                } else {
                    &self.persisted_baseline
                },
                fleet_format,
            )
            .map_err(|error| {
                SessionError::Protocol(format!(
                    "failed to snapshot RLM execution state as canonical state: {error}"
                ))
            })?;
        // Leaves the receiver of this capture already holds. A staged but
        // uncommitted capture supersedes the durable set, so a leaf it evicted
        // must be resent even though it is still durable.
        let prior_leaf_keys = if complete {
            BTreeSet::new()
        } else {
            self.pending_snapshot
                .as_ref()
                .map(|snapshot| snapshot.components.keys().cloned().collect())
                .unwrap_or_else(|| self.persisted_leaf_keys.clone())
        };
        let mut changed_leaves = if complete {
            BTreeMap::new()
        } else {
            self.pending_snapshot
                .as_ref()
                .into_iter()
                .flat_map(|snapshot| &snapshot.components)
                .filter_map(|(key, component)| match component {
                    lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => {
                        Some((key.clone(), body.clone()))
                    }
                    lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => None,
                })
                .collect::<BTreeMap<_, _>>()
        };
        #[cfg(test)]
        let mut encoded_globals = 0;
        let mut next_globals = BTreeMap::new();
        for (name, fragment) in parts.fragments {
            let persisted = match fragment {
                DurableFragment::Changed(body) => {
                    #[cfg(test)]
                    {
                        encoded_globals += 1;
                    }
                    persist_value_body(body, &prior_leaf_keys, &mut changed_leaves)
                }
                DurableFragment::Unchanged => {
                    self.persisted_globals.get(&name).cloned().ok_or_else(|| {
                        SessionError::Protocol(format!(
                            "RLM global `{name}` is unchanged since a capture that recorded no body for it"
                        ))
                    })?
                }
            };
            next_globals.insert(name, persisted);
        }

        let root = RlmSnapshotRoot {
            version: fleet_format.writer_version(lash_core::surface_format!(RLM_SNAPSHOT_VERSION)),
            engine: self.engine_id.to_string(),
            state_header: parts.header,
            globals: next_globals.clone(),
            deferred_resolutions: self.deferred_resolutions.clone(),
            deferred_trigger_resolutions: self.deferred_trigger_resolutions.clone(),
            child_max_attempts: self.child_max_attempts,
        };
        let encoded = rmp_serde::to_vec_named(&root).map_err(|error| {
            SessionError::Protocol(format!("failed to encode RLM snapshot root: {error}"))
        })?;
        validate_canonical_root(&encoded).map_err(|error| {
            SessionError::Protocol(format!("failed to encode canonical RLM root: {error}"))
        })?;

        let leaf_keys = root_leaf_keys(&root);
        let mut snapshot =
            lash_core::plugin::ExecutionStateSnapshot::from_root(Some(encoded.into()));
        for key in &leaf_keys {
            if let Some(body) = changed_leaves.remove(key) {
                snapshot.changed_component(key.clone(), body);
            } else {
                snapshot.unchanged_component(key.clone());
            }
        }
        Ok(PreparedCapture {
            snapshot,
            persisted_globals: next_globals,
            persisted_baseline: parts.baseline,
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
    ) -> lash_core::plugin::ExecutionStateSnapshot {
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

    #[cfg(test)]
    pub(super) fn encoded_globals_in_last_snapshot(&self) -> usize {
        self.encoded_globals_in_last_snapshot
    }

    /// Restores a persisted execution-state snapshot.
    ///
    /// `fleet_format` is the `F` the bound store recorded: the read admits the
    /// pair `{fleet's writer version, this build's newest}` — ADR 0106 §2's
    /// `[N-1, N]` window (FIG-3796). A payload at the fleet's older recorded
    /// version would climb to the newest through a surface-owned lift step
    /// before it decodes; this canonical binary root has no lift step yet, so
    /// an admitted older version is refused closed rather than decoded on
    /// shape alone.
    pub fn restore_execution_state(
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

        if parsed.version != window.newest() {
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

        let mut fragments = Vec::with_capacity(parsed.globals.len());
        for (name, persisted) in &parsed.globals {
            let body = match persisted {
                PersistedValue::Inline { body } => body.as_slice(),
                PersistedValue::Leaf { component } => resolve_leaf(state, name, component)?,
            };
            fragments.push((name.as_str(), body));
        }
        let (mut next_rlm, baseline) =
            FlowState::from_durable_parts(&parsed.state_header, fragments, fleet_format)?;
        prune_reserved_projected_bindings(&mut next_rlm);

        let next_live_names = next_rlm
            .binding_names()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let pruned_reserved = parsed.globals.len() != next_live_names.len();
        self.rlm = next_rlm;
        self.deferred_resolutions = parsed.deferred_resolutions;
        self.deferred_trigger_resolutions = parsed.deferred_trigger_resolutions;
        self.child_max_attempts = parsed.child_max_attempts;
        self.persisted_globals = parsed.globals;
        self.persisted_baseline = baseline;
        self.persisted_leaf_keys = leaf_keys_for_values(&self.persisted_globals);
        self.capture_dirty = pruned_reserved;
        self.capture_rollback = None;
        self.pending_snapshot = None;
        self.active_execution_checkpoint = None;
        self.execution_response_returned = false;
        Ok(())
    }

    pub fn prune_protected_globals(&mut self, protected_names: &BTreeSet<String>) {
        let before = self.rlm.binding_names().count();
        prune_protected_bindings(&mut self.rlm, protected_names);
        if self.rlm.binding_names().count() != before {
            self.capture_dirty = true;
        }
    }

    pub fn patch_globals(
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
        let inserted = apply_global_defaults(&mut self.rlm, patch, protected_names)
            .map_err(SessionError::Protocol)?;
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
        self.rlm.binding_names()
    }

    /// The bindings the "Bound Variables" section shows by summary: the ones
    /// with no host view (ADR 0076), each with its bounded runtime summary,
    /// under the same exclusions as [`Self::bound_variable_values`].
    pub(crate) fn opaque_bound_variables(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Vec<(String, String)> {
        self.rlm
            .opaque_bindings()
            .into_iter()
            .filter(|(name, _)| name != "history" && !exclude.contains(name))
            .collect()
    }

    /// The globals a cell boundary dropped for holding a function, which a
    /// later cell's reference is refused by name for.
    #[cfg(test)]
    pub(crate) fn expired_functions(&self) -> &std::collections::BTreeSet<String> {
        self.rlm.expired_functions()
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
        for (name, value) in self.rlm.globals().iter() {
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
