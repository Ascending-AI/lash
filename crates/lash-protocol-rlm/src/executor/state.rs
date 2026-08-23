use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::SessionError;
use lashlang::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, ExecutionScratch, SnapshotDecodeError,
    State as FlowState, Value as FlowValue, validate_canonical_messagepack_structure,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::projection::{prune_protected_bindings, prune_reserved_projected_bindings};

use super::apply_global_defaults;
use super::files::{ScratchFileError, ScratchFileStamp, collect_files, restore_files};
use super::snapshot::{RLM_SNAPSHOT_VERSION, RlmSnapshotError};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RlmSnapshotRoot {
    version: u32,
    engine: String,
    globals: BTreeMap<String, PersistedValue>,
    files: BTreeMap<String, PersistedValue>,
    deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
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

const ROOT_FIELDS: &[&str] = &[
    "version",
    "engine",
    "globals",
    "files",
    "deferred_resolutions",
];
const PERSISTED_VALUE_FIELDS: &[&str] = &["kind", "body", "component"];
const DEFERRED_RESOLUTION_FIELDS: &[&str] = &["link_key", "resolutions"];
const DEFERRED_LINK_KEY_FIELDS: &[&str] = &[
    "session_id",
    "turn_id",
    "turn_index",
    "protocol_iteration",
    "effect_id",
    "replay_key",
];
const RESOLUTION_FIELDS: &[&str] = &["kind", "definition", "source_id", "execution_binding"];
const TOOL_DEFINITION_FIELDS: &[&str] = &[
    "id",
    "name",
    "description",
    "compact_contract",
    "activation",
    "bindings",
    "argument_projection",
    "retry_policy",
    "input_schema",
    "output_schema",
    "output_contract",
    "examples",
];
const SCHEMA_CONTRACT_FIELDS: &[&str] = &["canonical", "projection"];
const SCHEMA_PROJECTION_FIELDS: &[&str] = &["mode", "overrides"];
const SCHEMA_OVERRIDE_FIELDS: &[&str] = &["dialect", "schema"];
const COMPACT_CONTRACT_FIELDS: &[&str] = &[
    "name",
    "signature",
    "returns",
    "parameters",
    "return_fields",
    "description",
    "examples",
];
const RETRY_POLICY_FIELDS: &[&str] = &["type", "max_attempts", "base_delay_ms", "max_delay_ms"];
const OUTPUT_CONTRACT_FIELDS: &[&str] = &["kind", "input_field", "default_schema"];
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
    // ToolDefinition flattens ToolManifest and ToolContract. Its field order is
    // therefore the sole accepted non-fixed-point ordering exception until
    // FIG-1210 removes that public wire-shape constraint. Dynamic maps and JSON
    // objects below it remain strictly sorted and unique.
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
        error @ SnapshotDecodeError::VersionMismatch { .. } => RlmSnapshotError::Lashlang(error),
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

fn root_map_order(location: &str) -> CanonicalMapOrder {
    match location {
        "root" => CanonicalMapOrder::Declared(ROOT_FIELDS),
        "root.globals" | "root.files" | "root.deferred_resolutions.resolutions" => {
            CanonicalMapOrder::Sorted
        }
        "root.deferred_resolutions" => CanonicalMapOrder::Declared(DEFERRED_RESOLUTION_FIELDS),
        "root.deferred_resolutions.link_key" => {
            CanonicalMapOrder::Declared(DEFERRED_LINK_KEY_FIELDS)
        }
        _ if is_persisted_value_location(location) => {
            CanonicalMapOrder::Declared(PERSISTED_VALUE_FIELDS)
        }
        _ if is_resolution_location(location) => CanonicalMapOrder::Declared(RESOLUTION_FIELDS),
        _ if is_root_json_location(location) => CanonicalMapOrder::Sorted,
        _ if location.ends_with(".definition") => CanonicalMapOrder::Fields(TOOL_DEFINITION_FIELDS),
        _ if location.ends_with(".input_schema") || location.ends_with(".output_schema") => {
            CanonicalMapOrder::Fields(SCHEMA_CONTRACT_FIELDS)
        }
        _ if location.ends_with(".projection") => {
            CanonicalMapOrder::Fields(SCHEMA_PROJECTION_FIELDS)
        }
        _ if is_schema_override_location(location) => {
            CanonicalMapOrder::Fields(SCHEMA_OVERRIDE_FIELDS)
        }
        _ if location.ends_with(".compact_contract") => {
            CanonicalMapOrder::Fields(COMPACT_CONTRACT_FIELDS)
        }
        _ if location.ends_with(".retry_policy") => CanonicalMapOrder::Fields(RETRY_POLICY_FIELDS),
        _ if location.ends_with(".output_contract") => {
            CanonicalMapOrder::Fields(OUTPUT_CONTRACT_FIELDS)
        }
        _ if location.ends_with(".argument_projection") => {
            CanonicalMapOrder::Fields(ARGUMENT_PROJECTION_FIELDS)
        }
        // FIG-1210: serde flatten prevents a fixed declaration-order rule for
        // the ToolDefinition map and its fixed-field descendants.
        _ => CanonicalMapOrder::Unordered,
    }
}

fn root_map_required(location: &str) -> bool {
    matches!(
        location,
        "root"
            | "root.globals"
            | "root.files"
            | "root.deferred_resolutions"
            | "root.deferred_resolutions.link_key"
            | "root.deferred_resolutions.resolutions"
    ) || is_resolution_location(location)
        || is_persisted_value_location(location)
        || location.ends_with(".definition")
        || location.ends_with(".input_schema")
        || location.ends_with(".output_schema")
        || location.ends_with(".projection")
        || is_schema_override_location(location)
        || location.ends_with(".compact_contract")
        || location.ends_with(".retry_policy")
        || location.ends_with(".output_contract")
        || location.ends_with(".argument_projection")
}

fn is_resolution_location(location: &str) -> bool {
    location
        .strip_prefix("root.deferred_resolutions.resolutions.")
        .is_some_and(|suffix| !suffix.contains('.'))
        || (location.starts_with("root.deferred_resolutions.resolutions[")
            && !location["root.deferred_resolutions.resolutions".len()..].contains("]."))
}

fn is_global_location(location: &str) -> bool {
    location
        .strip_prefix("root.globals.")
        .is_some_and(|suffix| !suffix.contains('.'))
        || (location.starts_with("root.globals[") && location.ends_with(']'))
}

fn is_file_location(location: &str) -> bool {
    location
        .strip_prefix("root.files.")
        .is_some_and(|suffix| !suffix.contains('.'))
        || (location.starts_with("root.files[") && location.ends_with(']'))
}

fn is_persisted_value_location(location: &str) -> bool {
    is_global_location(location) || is_file_location(location)
}

fn is_schema_override_location(location: &str) -> bool {
    location.contains(".projection.overrides[") && location.ends_with(']')
}

fn is_root_json_location(location: &str) -> bool {
    let json_field = [
        ".execution_binding",
        ".canonical",
        ".default_schema",
        ".parameters",
        ".return_fields",
        ".schema",
    ]
    .into_iter()
    .any(|field| {
        location.find(field).is_some_and(|index| {
            location[index + field.len()..].is_empty()
                || matches!(
                    location.as_bytes().get(index + field.len()),
                    Some(b'.' | b'[')
                )
        })
    });
    json_field || location.ends_with(".bindings") || location.contains(".bindings[")
}

fn snapshot_runtime_value(value: &FlowValue) -> Result<Vec<u8>, lashlang::ContinuationError> {
    let snapshot =
        lashlang::Snapshot::new([("value".to_string(), value.clone())].into_iter().collect());
    snapshot.to_canonical_bytes()
}

fn restore_runtime_value(data: &[u8]) -> Result<FlowValue, RlmSnapshotError> {
    let snapshot = lashlang::Snapshot::from_canonical_bytes(data)?;
    if snapshot.globals().len() != 1 || snapshot.globals().get("value").is_none() {
        return Err(RlmSnapshotError::FormatMismatch {
            details: "value body must contain exactly the canonical `value` binding".to_string(),
        });
    }
    Ok(snapshot
        .into_globals()
        .remove("value")
        .expect("the canonical value binding was checked"))
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
    changed_leaves: &mut BTreeMap<String, Vec<u8>>,
) -> PersistedValue {
    if body_prefers_leaf(body.len()) {
        let component = leaf_component_key(&body);
        if !prior_leaf_keys.contains(&component) {
            changed_leaves.insert(component.clone(), body);
        }
        PersistedValue::Leaf { component }
    } else {
        PersistedValue::Inline { body }
    }
}

fn leaf_component_key(body: &[u8]) -> String {
    format!("execution_state/sha256/{:x}", Sha256::digest(body))
}

#[cfg(test)]
pub(super) fn measure_snapshot(
    snapshot: &lash_core::plugin::ExecutionStateSnapshot,
) -> lash_core::testing::RuntimeCommitBudgetMeasurement {
    let state = lash_core::RuntimeSessionState {
        session_id: "fig-1257-snapshot-budget".to_string(),
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
                    .strip_prefix("execution_state/sha256/")
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
        .map(Vec::as_slice)
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
    root_leaf_keys_from_sections(&root.globals, &root.files)
}

fn root_leaf_keys_from_sections(
    globals: &BTreeMap<String, PersistedValue>,
    files: &BTreeMap<String, PersistedValue>,
) -> BTreeSet<String> {
    globals
        .values()
        .filter_map(|global| match global {
            PersistedValue::Inline { .. } => None,
            PersistedValue::Leaf { component } => Some(component.clone()),
        })
        .chain(files.values().filter_map(|file| match file {
            PersistedValue::Inline { .. } => None,
            PersistedValue::Leaf { component } => Some(component.clone()),
        }))
        .collect()
}

/// Which state a capture is relative to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// A checkpoint delta: re-encode assigned bindings and changed files only,
    /// and reference every other leaf the receiver already holds.
    Incremental,
    /// Every binding and file, with every leaf body present. Relative to
    /// nothing, so it neither reads nor advances capture bookkeeping.
    Complete,
}

/// A capture that has been built but not yet installed as the pending capture.
struct PreparedCapture {
    snapshot: lash_core::plugin::ExecutionStateSnapshot,
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_files: BTreeMap<String, PersistedValue>,
    persisted_file_stamps: BTreeMap<String, ScratchFileStamp>,
    leaf_keys: BTreeSet<String>,
    #[cfg(test)]
    encoded_globals: usize,
}

struct CaptureRollback {
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_files: BTreeMap<String, PersistedValue>,
    persisted_file_stamps: BTreeMap<String, ScratchFileStamp>,
    persisted_leaf_keys: BTreeSet<String>,
    dirty_globals: BTreeSet<String>,
    dirty_files: BTreeSet<String>,
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
    /// Live scratch directory for this execution state.
    ///
    /// Change detection for its files is a metadata stamp (length, nanosecond
    /// mtime, and on Unix device/inode/ctime), which a same-length rewrite
    /// inside one filesystem timestamp tick could defeat. Any writer inside this
    /// process must therefore mark the path in `dirty_files` as
    /// [`Self::write_scratch_file_for_testing`] does, rather than rely on the stamp. Today
    /// no production code writes here — code effects write through Lashlang
    /// values, and `restore_files` writes only into a fresh directory during
    /// restore — so the only writer is that test helper. A first production
    /// writer must be added with its marking, not without it.
    scratch_dir: tempfile::TempDir,
    persisted_globals: BTreeMap<String, PersistedValue>,
    persisted_files: BTreeMap<String, PersistedValue>,
    persisted_file_stamps: BTreeMap<String, ScratchFileStamp>,
    persisted_leaf_keys: BTreeSet<String>,
    dirty_globals: BTreeSet<String>,
    dirty_files: BTreeSet<String>,
    root_dirty: bool,
    capture_rollback: Option<CaptureRollback>,
    pending_snapshot: Option<lash_core::plugin::ExecutionStateSnapshot>,
    #[cfg(test)]
    encoded_globals_in_last_snapshot: usize,
}

impl RlmExecutionState {
    #[cfg(test)]
    pub fn new() -> Result<Self, SessionError> {
        Self::for_engine("lashlang")
    }

    pub(crate) fn for_engine(engine_id: impl Into<Arc<str>>) -> Result<Self, SessionError> {
        Ok(Self {
            engine_id: engine_id.into(),
            rlm: FlowState::new(),
            scratch: ExecutionScratch::new(),
            linked_programs: lashlang::LinkedProgramCache::new(),
            stored_lashlang_modules: BTreeSet::new(),
            deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord::default(),
            scratch_dir: tempfile::TempDir::new()?,
            persisted_globals: BTreeMap::new(),
            persisted_files: BTreeMap::new(),
            persisted_file_stamps: BTreeMap::new(),
            persisted_leaf_keys: BTreeSet::new(),
            dirty_globals: BTreeSet::new(),
            dirty_files: BTreeSet::new(),
            root_dirty: true,
            capture_rollback: None,
            pending_snapshot: None,
            #[cfg(test)]
            encoded_globals_in_last_snapshot: 0,
        })
    }

    pub fn execution_state_dirty(&self) -> bool {
        self.pending_snapshot.is_some()
            || self.root_dirty
            || !self.dirty_globals.is_empty()
            || !self.dirty_files.is_empty()
    }

    pub(super) fn mark_execution_started(&mut self) {
        self.dirty_globals
            .append(&mut self.scratch.take_assigned_globals());
        if self.persisted_globals.is_empty() && self.root_dirty {
            self.dirty_globals
                .extend(self.rlm.globals().iter().map(|(name, _)| name.to_string()));
        }
        self.root_dirty = true;
    }

    /// Write a scratch file and mark it changed. The marking, not the metadata
    /// stamp, is what makes a same-length rewrite safe; see [`Self::scratch_dir`].
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    fn write_scratch_file_for_testing(
        &mut self,
        path: &str,
        body: &[u8],
    ) -> Result<(), SessionError> {
        restore_files(
            self.scratch_dir.path(),
            &[(path.to_string(), body.to_vec())].into_iter().collect(),
        )
        .map_err(|error| SessionError::Protocol(error.to_string()))?;
        self.dirty_files.insert(path.to_string());
        self.root_dirty = true;
        Ok(())
    }

    /// Encode the canonical RLM root and only the leaf bodies whose logical
    /// values were assigned since the previous capture.
    pub fn snapshot_execution_state(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.absorb_pending_assignments();
        if !self.root_dirty
            && self.dirty_globals.is_empty()
            && self.dirty_files.is_empty()
            && let Some(snapshot) = &self.pending_snapshot
        {
            return Ok(snapshot.clone());
        }
        let prepared = self.build_capture(CaptureMode::Incremental)?;
        Ok(self.install_capture(prepared))
    }

    /// Answer whether the capture `snapshot_execution_state` would take right
    /// now can succeed, without staging it.
    ///
    /// The whole fallible part of a capture is building it — canonical encoding
    /// of every assigned binding and reading every changed scratch file — so
    /// this proves capturability by building the same capture and dropping it.
    /// It advances no capture bookkeeping: the dirty set only absorbs the
    /// assignments the last execution recorded, which is what keeps them from
    /// being lost, and everything else stays exactly as the eventual capture
    /// will find it.
    pub fn probe_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.absorb_pending_assignments();
        if !self.root_dirty
            && self.dirty_globals.is_empty()
            && self.dirty_files.is_empty()
            && self.pending_snapshot.is_some()
        {
            return Ok(());
        }
        self.build_capture(CaptureMode::Incremental).map(|_| ())
    }

    /// The complete live execution state, with every leaf body present and no
    /// capture bookkeeping touched. Explicit administrative snapshot uses this;
    /// it is relative to nothing, so it never depends on which leaf bodies are
    /// still resident in the runtime's checkpoint state.
    pub fn hydrated_execution_state(
        &self,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        let prepared = self.build_capture(CaptureMode::Complete)?;
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

    /// Fold the assignments the last execution recorded into the dirty set. On
    /// a session's first capture nothing is persisted yet, so every live global
    /// is dirty.
    fn absorb_pending_assignments(&mut self) {
        self.dirty_globals
            .append(&mut self.scratch.take_assigned_globals());
        if self.persisted_globals.is_empty() && self.root_dirty {
            self.dirty_globals
                .extend(self.rlm.globals().iter().map(|(name, _)| name.to_string()));
        }
    }

    #[cfg(feature = "testing")]
    pub(super) fn absorb_pending_assignments_for_perf(&mut self) {
        self.absorb_pending_assignments();
    }

    /// Build a capture. Reads state; mutates none of it.
    fn build_capture(&self, mode: CaptureMode) -> Result<PreparedCapture, SessionError> {
        let complete = mode == CaptureMode::Complete;
        let current_globals = self.rlm.globals();
        let all_global_names = || {
            current_globals
                .iter()
                .map(|(name, _)| name.to_string())
                .collect::<BTreeSet<_>>()
        };
        let dirty_globals = if complete {
            std::borrow::Cow::Owned(all_global_names())
        } else {
            std::borrow::Cow::Borrowed(&self.dirty_globals)
        };
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
        let mut next_globals = if complete {
            BTreeMap::new()
        } else {
            self.persisted_globals.clone()
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
        for name in dirty_globals.iter() {
            let Some(value) = current_globals.get(name) else {
                next_globals.remove(name);
                continue;
            };
            let body = snapshot_runtime_value(value).map_err(|error| {
                SessionError::Protocol(format!(
                    "failed to snapshot RLM global `{name}` as canonical state: {error}"
                ))
            })?;
            #[cfg(test)]
            {
                encoded_globals += 1;
            }
            let persisted = persist_value_body(body, &prior_leaf_keys, &mut changed_leaves);
            next_globals.insert(name.clone(), persisted);
        }

        let mut reusable_file_stamps = if complete {
            BTreeMap::new()
        } else {
            self.persisted_file_stamps.clone()
        };
        for path in &self.dirty_files {
            reusable_file_stamps.remove(path);
        }
        let collected_files = collect_files(self.scratch_dir.path(), &reusable_file_stamps)
            .map_err(|error| {
                SessionError::Protocol(format!("failed to collect RLM scratch files: {error}"))
            })?;
        let mut persisted_files = BTreeMap::new();
        let mut persisted_file_stamps = BTreeMap::new();
        for (path, collected) in collected_files {
            let persisted = if let Some(body) = collected.changed_body {
                persist_value_body(body, &prior_leaf_keys, &mut changed_leaves)
            } else {
                self.persisted_files.get(&path).cloned().ok_or_else(|| {
                    SessionError::Protocol(format!(
                        "unchanged RLM scratch file `{path}` has no persisted value"
                    ))
                })?
            };
            persisted_file_stamps.insert(path.clone(), collected.stamp);
            persisted_files.insert(path, persisted);
        }

        let root = RlmSnapshotRoot {
            version: RLM_SNAPSHOT_VERSION,
            engine: self.engine_id.to_string(),
            globals: next_globals.clone(),
            files: persisted_files.clone(),
            deferred_resolutions: self.deferred_resolutions.clone(),
        };
        let encoded = rmp_serde::to_vec_named(&root).map_err(|error| {
            SessionError::Protocol(format!("failed to encode RLM snapshot root: {error}"))
        })?;
        validate_canonical_root(&encoded).map_err(|error| {
            SessionError::Protocol(format!("failed to encode canonical RLM root: {error}"))
        })?;

        let leaf_keys = root_leaf_keys(&root);
        let mut snapshot = lash_core::plugin::ExecutionStateSnapshot::from_root(Some(encoded));
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
            persisted_files,
            persisted_file_stamps,
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
            persisted_files,
            persisted_file_stamps,
            leaf_keys,
            #[cfg(test)]
            encoded_globals,
        } = prepared;
        if self.capture_rollback.is_none() {
            self.capture_rollback = Some(CaptureRollback {
                persisted_globals: std::mem::replace(
                    &mut self.persisted_globals,
                    persisted_globals,
                ),
                persisted_files: std::mem::replace(&mut self.persisted_files, persisted_files),
                persisted_file_stamps: std::mem::replace(
                    &mut self.persisted_file_stamps,
                    persisted_file_stamps,
                ),
                persisted_leaf_keys: std::mem::replace(&mut self.persisted_leaf_keys, leaf_keys),
                dirty_globals: std::mem::take(&mut self.dirty_globals),
                dirty_files: std::mem::take(&mut self.dirty_files),
            });
        } else {
            self.persisted_globals = persisted_globals;
            self.persisted_files = persisted_files;
            self.persisted_file_stamps = persisted_file_stamps;
            self.persisted_leaf_keys = leaf_keys;
            self.dirty_globals.clear();
            self.dirty_files.clear();
        }
        self.root_dirty = false;
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
        let prior_global_names = rollback
            .persisted_globals
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let prior_file_names = rollback.persisted_files.keys().cloned().collect::<Vec<_>>();
        self.persisted_globals = rollback.persisted_globals;
        self.persisted_files = rollback.persisted_files;
        self.persisted_file_stamps = rollback.persisted_file_stamps;
        self.persisted_leaf_keys = rollback.persisted_leaf_keys;
        self.dirty_globals = rollback.dirty_globals;
        self.dirty_files = rollback.dirty_files;
        self.dirty_globals.extend(prior_global_names);
        self.dirty_globals
            .extend(self.rlm.globals().iter().map(|(name, _)| name.to_string()));
        self.dirty_files.extend(prior_file_names);
        self.root_dirty = true;
        self.pending_snapshot = None;
    }

    #[cfg(test)]
    pub(super) fn encoded_globals_in_last_snapshot(&self) -> usize {
        self.encoded_globals_in_last_snapshot
    }

    pub fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), RlmSnapshotError> {
        let found_version = probe_snapshot_version(&state.root)?;
        if found_version != RLM_SNAPSHOT_VERSION {
            return Err(RlmSnapshotError::VersionMismatch {
                expected: RLM_SNAPSHOT_VERSION,
                found: found_version,
            });
        }
        validate_canonical_root(&state.root)?;
        let parsed: RlmSnapshotRoot = rmp_serde::from_slice(&state.root).map_err(|error| {
            RlmSnapshotError::FormatMismatch {
                details: error.to_string(),
            }
        })?;

        if parsed.version != RLM_SNAPSHOT_VERSION {
            return Err(RlmSnapshotError::VersionMismatch {
                expected: RLM_SNAPSHOT_VERSION,
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

        let mut globals = lashlang::Record::new();
        for (name, persisted) in &parsed.globals {
            let body = match persisted {
                PersistedValue::Inline { body } => body.as_slice(),
                PersistedValue::Leaf { component } => resolve_leaf(state, name, component)?,
            };
            globals.insert(name.clone(), restore_runtime_value(body)?);
        }
        let mut next_rlm = FlowState::from_snapshot(lashlang::Snapshot::new(globals));
        prune_reserved_projected_bindings(&mut next_rlm);

        let mut files = BTreeMap::new();
        for (path, persisted) in &parsed.files {
            let body = match persisted {
                PersistedValue::Inline { body } => body.as_slice(),
                PersistedValue::Leaf { component } => resolve_leaf(state, path, component)?,
            };
            files.insert(path.clone(), body.to_vec());
        }
        let next_scratch_dir = tempfile::TempDir::new().map_err(ScratchFileError::CreateScratch)?;
        restore_files(next_scratch_dir.path(), &files)?;
        let next_file_stamps = collect_files(next_scratch_dir.path(), &BTreeMap::new())?
            .into_iter()
            .map(|(path, collected)| (path, collected.stamp))
            .collect();
        let next_live_names = next_rlm
            .globals()
            .iter()
            .map(|(name, _)| name.to_string())
            .collect::<BTreeSet<_>>();
        let pruned_reserved = parsed.globals.len() != next_live_names.len();
        let mut next_globals = parsed.globals;
        next_globals.retain(|name, _| next_live_names.contains(name));
        self.rlm = next_rlm;
        self.scratch_dir = next_scratch_dir;
        self.deferred_resolutions = parsed.deferred_resolutions;
        self.persisted_globals = next_globals;
        self.persisted_files = parsed.files;
        self.persisted_file_stamps = next_file_stamps;
        self.persisted_leaf_keys =
            root_leaf_keys_from_sections(&self.persisted_globals, &self.persisted_files);
        self.root_dirty = pruned_reserved;
        self.dirty_globals.clear();
        self.dirty_files.clear();
        self.capture_rollback = None;
        self.pending_snapshot = None;
        Ok(())
    }

    pub fn prune_protected_globals(&mut self, protected_names: &BTreeSet<String>) {
        let before = self
            .rlm
            .globals()
            .iter()
            .map(|(name, _)| name.to_string())
            .collect::<BTreeSet<_>>();
        prune_protected_bindings(&mut self.rlm, protected_names);
        for removed in before.difference(
            &self
                .rlm
                .globals()
                .iter()
                .map(|(name, _)| name.to_string())
                .collect(),
        ) {
            self.dirty_globals.insert(removed.clone());
        }
    }

    pub(super) fn mark_globals_removed<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        for name in names {
            if self.rlm.globals().get(name).is_some() {
                self.dirty_globals.insert(name.to_string());
                self.root_dirty = true;
            }
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
        if inserted.is_empty() {
            return Ok(());
        }
        self.dirty_globals.extend(inserted);
        self.root_dirty = true;
        Ok(())
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

#[cfg(feature = "testing")]
pub(crate) fn capture_scratch_files_for_testing(
    files: Vec<(String, Vec<u8>)>,
) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
    // Production construction names its dialect; this testing-feature helper
    // captures files for the Lashlang engine, which is what it did before the
    // dialect became explicit.
    let mut state = RlmExecutionState::for_engine("lashlang")?;
    for (path, body) in files {
        state.write_scratch_file_for_testing(&path, &body)?;
    }
    state.hydrated_execution_state()
}

#[cfg(test)]
mod tests;
