use std::collections::{BTreeMap, BTreeSet};

use lash_core::SessionError;
use lashlang::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, ExecutionScratch, SnapshotDecodeError,
    State as FlowState, Value as FlowValue, validate_canonical_messagepack_structure,
};
use serde::{Deserialize, Serialize};

use crate::projection::{prune_protected_bindings, prune_reserved_projected_bindings};

use super::apply_global_defaults;
use super::files::{clear_dir, collect_files, restore_files};
use super::snapshot::{RLM_SNAPSHOT_VERSION, RlmSnapshotError, restore_runtime, snapshot_runtime};

#[derive(Serialize, Deserialize)]
pub(super) struct RlmSnapshotEnvelope {
    version: u32,
    engine: String,
    #[serde(with = "serde_bytes")]
    pub(super) vars: Vec<u8>,
    files: BTreeMap<String, String>,
    deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
}

const ENVELOPE_FIELDS: &[&str] = &["version", "engine", "vars", "files", "deferred_resolutions"];
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

fn validate_canonical_envelope(data: &[u8]) -> Result<(), RlmSnapshotError> {
    if matches!(
        data.iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace()),
        Some(b'{' | b'[')
    ) {
        return Err(RlmSnapshotError::FormatMismatch {
            details: "legacy JSON envelope is not canonical typed MessagePack".to_string(),
        });
    }
    // ToolDefinition flattens ToolManifest and ToolContract. Its field order is
    // therefore the sole accepted non-fixed-point ordering exception until
    // FIG-1210 removes that public wire-shape constraint. Dynamic maps and JSON
    // objects below it remain strictly sorted and unique.
    validate_canonical_messagepack_structure(
        data,
        "envelope",
        CANONICAL_MESSAGEPACK_DEPTH_LIMIT,
        envelope_map_order,
        envelope_map_required,
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
    })
}

fn envelope_map_order(location: &str) -> CanonicalMapOrder {
    if is_envelope_json_location(location) {
        return CanonicalMapOrder::Sorted;
    }
    match location {
        "envelope" => CanonicalMapOrder::Declared(ENVELOPE_FIELDS),
        "envelope.files" | "envelope.deferred_resolutions.resolutions" => CanonicalMapOrder::Sorted,
        "envelope.deferred_resolutions" => CanonicalMapOrder::Declared(DEFERRED_RESOLUTION_FIELDS),
        "envelope.deferred_resolutions.link_key" => {
            CanonicalMapOrder::Declared(DEFERRED_LINK_KEY_FIELDS)
        }
        _ if is_resolution_location(location) => CanonicalMapOrder::Declared(RESOLUTION_FIELDS),
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

fn envelope_map_required(location: &str) -> bool {
    matches!(
        location,
        "envelope"
            | "envelope.files"
            | "envelope.deferred_resolutions"
            | "envelope.deferred_resolutions.link_key"
            | "envelope.deferred_resolutions.resolutions"
    ) || is_resolution_location(location)
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
    location.starts_with("envelope.deferred_resolutions.resolutions[")
        && !location["envelope.deferred_resolutions.resolutions".len()..].contains("].")
}

fn is_schema_override_location(location: &str) -> bool {
    location.contains(".projection.overrides[") && location.ends_with(']')
}

fn is_envelope_json_location(location: &str) -> bool {
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

pub struct RlmExecutionState {
    pub(super) rlm: FlowState,
    pub(super) scratch: ExecutionScratch,
    pub(super) linked_programs: lashlang::LinkedProgramCache,
    pub(super) stored_lashlang_modules: BTreeSet<lashlang::ModuleRef>,
    /// Active-link record of deferred tool resolutions, keyed by Lashlang
    /// call-path. Snapshotted/restored with the rest of the execution state so
    /// a re-driven or recovered link replays the recorded grants and
    /// `NotAvailable` results without leaking them into a later code effect.
    pub(super) deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
    pub(super) scratch_dir: tempfile::TempDir,
    pub(super) dirty: bool,
}

impl RlmExecutionState {
    pub fn new() -> Result<Self, SessionError> {
        Ok(Self {
            rlm: FlowState::new(),
            scratch: ExecutionScratch::new(),
            linked_programs: lashlang::LinkedProgramCache::new(),
            stored_lashlang_modules: BTreeSet::new(),
            deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord::default(),
            scratch_dir: tempfile::TempDir::new()?,
            dirty: true,
        })
    }

    pub fn execution_state_dirty(&self) -> bool {
        self.dirty
    }

    /// Encode the canonical RLM persistence envelope.
    ///
    /// Every byte sequence emitted here round-trips identically. Any accepted
    /// foreign wire is also a fixed point except for field order within the
    /// flattened `ToolDefinition` subtree; FIG-1210 tracks removing that sole
    /// public wire-shape exception.
    pub fn snapshot_execution_state(&mut self) -> Result<Option<Vec<u8>>, SessionError> {
        let vars = snapshot_runtime(&self.rlm).map_err(|error| {
            SessionError::Protocol(format!("failed to snapshot RLM canonical state: {error}"))
        })?;
        let files = collect_files(self.scratch_dir.path()).unwrap_or_default();
        let combined = RlmSnapshotEnvelope {
            version: RLM_SNAPSHOT_VERSION,
            engine: "lashlang".to_string(),
            vars,
            files,
            deferred_resolutions: self.deferred_resolutions.clone(),
        };
        let encoded = rmp_serde::to_vec_named(&combined).map_err(|error| {
            SessionError::Protocol(format!("failed to encode RLM snapshot envelope: {error}"))
        })?;
        validate_canonical_envelope(&encoded).map_err(|error| {
            SessionError::Protocol(format!("failed to encode canonical RLM envelope: {error}"))
        })?;
        self.dirty = false;
        Ok(Some(encoded))
    }

    pub fn restore_execution_state(&mut self, data: &[u8]) -> Result<(), RlmSnapshotError> {
        validate_canonical_envelope(data)?;
        let parsed: RlmSnapshotEnvelope =
            rmp_serde::from_slice(data).map_err(|error| RlmSnapshotError::FormatMismatch {
                details: error.to_string(),
            })?;

        if parsed.version != RLM_SNAPSHOT_VERSION {
            return Err(RlmSnapshotError::VersionMismatch {
                expected: RLM_SNAPSHOT_VERSION,
                found: parsed.version,
            });
        }
        if parsed.engine != "lashlang" {
            return Err(RlmSnapshotError::EngineMismatch {
                found: parsed.engine,
            });
        }

        self.rlm = restore_runtime(&parsed.vars)?;
        prune_reserved_projected_bindings(&mut self.rlm);

        clear_dir(self.scratch_dir.path());
        let _ = restore_files(self.scratch_dir.path(), &parsed.files);
        self.deferred_resolutions = parsed.deferred_resolutions;
        self.dirty = true;
        Ok(())
    }

    pub fn prune_protected_globals(&mut self, protected_names: &BTreeSet<String>) {
        prune_protected_bindings(&mut self.rlm, protected_names);
    }

    pub fn patch_globals(
        &mut self,
        patch: &lash_rlm_types::RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        if patch.is_empty() {
            return Ok(());
        }
        apply_global_defaults(&mut self.rlm, patch, protected_names)
            .map_err(SessionError::Protocol)?;
        self.dirty = true;
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

#[cfg(test)]
mod bound_variable_value_tests {
    use super::*;
    use lashlang::{
        ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest, ProjectedReadResponse,
        ProjectedValue, Record as FlowRecord, Value as FlowValue,
    };
    use serde_json::json;

    fn push_string(bytes: &mut Vec<u8>, value: &str) {
        assert!(value.len() < 32);
        bytes.push(0xa0 | value.len() as u8);
        bytes.extend_from_slice(value.as_bytes());
    }

    fn push_map(bytes: &mut Vec<u8>, length: usize) {
        assert!(length < 16);
        bytes.push(0x80 | length as u8);
    }

    fn push_binary(bytes: &mut Vec<u8>, value: &[u8]) {
        assert!(u8::try_from(value.len()).is_ok());
        bytes.extend_from_slice(&[0xc4, value.len() as u8]);
        bytes.extend_from_slice(value);
    }

    fn hand_crafted_envelope(file_keys: &[(&str, &str)], resolution_keys: &[&str]) -> Vec<u8> {
        let vars = lashlang::Snapshot::default()
            .to_canonical_bytes()
            .expect("canonical vars");
        let mut bytes = Vec::new();
        push_map(&mut bytes, 5);
        push_string(&mut bytes, "version");
        bytes.push(RLM_SNAPSHOT_VERSION as u8);
        push_string(&mut bytes, "engine");
        push_string(&mut bytes, "lashlang");
        push_string(&mut bytes, "vars");
        push_binary(&mut bytes, &vars);
        push_string(&mut bytes, "files");
        push_map(&mut bytes, file_keys.len());
        for (key, value) in file_keys {
            push_string(&mut bytes, key);
            push_string(&mut bytes, value);
        }
        push_string(&mut bytes, "deferred_resolutions");
        push_map(&mut bytes, 1);
        push_string(&mut bytes, "resolutions");
        push_map(&mut bytes, resolution_keys.len());
        for key in resolution_keys {
            push_string(&mut bytes, key);
            push_map(&mut bytes, 1);
            push_string(&mut bytes, "kind");
            push_string(&mut bytes, "not_available");
        }
        bytes
    }

    #[test]
    fn old_json_snapshot_is_typed_format_rejection_with_cutover_remedy() {
        let old_snapshot = serde_json::to_vec(&json!({
            "version": 5,
            "engine": "lashlang",
            "vars": "{\"globals\":{}}",
            "files": {},
            "deferred_resolutions": {"resolutions": {}}
        }))
        .expect("old JSON snapshot");
        let mut state = RlmExecutionState::new().expect("state");

        let error = state
            .restore_execution_state(&old_snapshot)
            .expect_err("old JSON must not have a compatibility decoder");

        assert!(matches!(&error, RlmSnapshotError::FormatMismatch { .. }));
        let message = error.to_string();
        assert!(message.contains("drain in-flight sessions on the old build"));
        assert!(message.contains("recreate development/test stores"));
    }

    #[test]
    fn old_snapshot_version_is_typed_rejection_with_cutover_remedy() {
        let mut source = RlmExecutionState::new().expect("source state");
        let bytes = source
            .snapshot_execution_state()
            .expect("snapshot")
            .expect("snapshot bytes");
        let mut envelope: RlmSnapshotEnvelope =
            rmp_serde::from_slice(&bytes).expect("decode current envelope");
        envelope.version = RLM_SNAPSHOT_VERSION - 1;
        let old_version = rmp_serde::to_vec_named(&envelope).expect("old-version envelope");
        let mut target = RlmExecutionState::new().expect("target state");

        let error = target
            .restore_execution_state(&old_version)
            .expect_err("old version must be rejected before Lashlang decode");

        assert!(matches!(
            &error,
            RlmSnapshotError::VersionMismatch {
                expected: RLM_SNAPSHOT_VERSION,
                found
            } if *found == RLM_SNAPSHOT_VERSION - 1
        ));
        let message = error.to_string();
        assert!(message.contains("drain in-flight sessions on the old build"));
        assert!(message.contains("recreate development/test stores"));
    }

    #[test]
    fn execution_envelope_is_deterministic_for_scratch_file_insertion_order() {
        fn state_with_files(files: &[(&str, &str)]) -> RlmExecutionState {
            let state = RlmExecutionState::new().expect("state");
            for (path, contents) in files {
                let path = state.scratch_dir.path().join(path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).expect("create scratch parent");
                }
                std::fs::write(path, contents).expect("write scratch file");
            }
            state
        }

        let mut left = state_with_files(&[
            ("z-last.txt", "same-z"),
            ("nested/middle.txt", "same-middle"),
            ("a-first.txt", "same-a"),
        ]);
        let mut right = state_with_files(&[
            ("a-first.txt", "same-a"),
            ("nested/middle.txt", "same-middle"),
            ("z-last.txt", "same-z"),
        ]);

        let left = left
            .snapshot_execution_state()
            .expect("left snapshot")
            .expect("left bytes");
        let right = right
            .snapshot_execution_state()
            .expect("right snapshot")
            .expect("right bytes");

        assert_eq!(left, right);
    }

    #[test]
    fn canonical_envelope_rejects_unsorted_and_duplicate_files_with_typed_keys() {
        for (keys, offending) in [
            (&[("z.txt", "z"), ("a.txt", "a")][..], "a.txt"),
            (&[("a.txt", "first"), ("a.txt", "second")][..], "a.txt"),
        ] {
            let error = validate_canonical_envelope(&hand_crafted_envelope(keys, &[]))
                .expect_err("non-canonical files map must fail before serde");
            assert!(matches!(
                error,
                RlmSnapshotError::NonCanonicalEnvelope { location, reason }
                    if location == "envelope.files" && reason.contains(offending)
            ));
        }
    }

    #[test]
    fn canonical_envelope_rejects_unsorted_and_duplicate_nested_resolution_keys() {
        for keys in [&["z.tool", "a.tool"][..], &["a.tool", "a.tool"][..]] {
            let error = validate_canonical_envelope(&hand_crafted_envelope(&[], keys))
                .expect_err("non-canonical resolutions map must fail before serde");
            assert!(matches!(
                error,
                RlmSnapshotError::NonCanonicalEnvelope { location, reason }
                    if location == "envelope.deferred_resolutions.resolutions"
                        && reason.contains("a.tool")
            ));
        }
    }

    #[test]
    fn canonical_envelope_rejects_a_depth_bomb_before_serde() {
        let vars = lashlang::Snapshot::default()
            .to_canonical_bytes()
            .expect("canonical vars");
        let mut bytes = Vec::new();
        push_map(&mut bytes, 5);
        push_string(&mut bytes, "version");
        bytes.push(RLM_SNAPSHOT_VERSION as u8);
        push_string(&mut bytes, "engine");
        push_string(&mut bytes, "lashlang");
        push_string(&mut bytes, "vars");
        push_binary(&mut bytes, &vars);
        push_string(&mut bytes, "files");
        push_map(&mut bytes, 1);
        push_string(&mut bytes, "bomb");
        bytes.extend(std::iter::repeat_n(
            0x91,
            CANONICAL_MESSAGEPACK_DEPTH_LIMIT + 1,
        ));
        bytes.push(0xc0);
        push_string(&mut bytes, "deferred_resolutions");
        push_map(&mut bytes, 1);
        push_string(&mut bytes, "resolutions");
        push_map(&mut bytes, 0);

        assert!(matches!(
            validate_canonical_envelope(&bytes),
            Err(RlmSnapshotError::EnvelopeDepthLimitExceeded { limit })
                if limit == CANONICAL_MESSAGEPACK_DEPTH_LIMIT
        ));
    }

    #[test]
    fn canonical_envelope_rejects_non_minimal_container_width_before_serde() {
        let canonical = hand_crafted_envelope(&[], &[]);
        assert_eq!(
            canonical[0], 0x85,
            "fixture root must be a five-field fixmap"
        );
        let mut non_minimal = vec![0xde, 0x00, 0x05];
        non_minimal.extend_from_slice(&canonical[1..]);

        assert!(matches!(
            validate_canonical_envelope(&non_minimal),
            Err(RlmSnapshotError::NonCanonicalEnvelope { location, reason })
                if location == "envelope" && reason.contains("map length is not minimally encoded")
        ));
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn includes_globals_excludes_history_and_named() {
        let mut state = RlmExecutionState::new().unwrap();
        let mut set_default = serde_json::Map::new();
        set_default.insert("inventory".to_string(), json!(["lantern"]));
        set_default.insert("secret".to_string(), json!(1));
        state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody { set_default },
                &BTreeSet::new(),
            )
            .unwrap();

        let exclude: BTreeSet<String> = ["secret".to_string()].into_iter().collect();
        let vars = state.bound_variable_values(&exclude);
        assert!(vars.iter().any(|(name, _)| name == "inventory"), "{vars:?}");
        assert!(
            !vars.iter().any(|(name, _)| name == "secret"),
            "excluded name leaked: {vars:?}"
        );
        assert!(
            !vars.iter().any(|(name, _)| name == "history"),
            "history leaked: {vars:?}"
        );
    }

    #[test]
    fn excludes_direct_projected_globals() {
        let mut state = RlmExecutionState::new().unwrap();
        let mut snapshot = state.rlm.snapshot();
        snapshot.globals.insert(
            "projected".to_string(),
            FlowValue::Projected(ProjectedValue::scalar(
                "projected",
                FlowValue::String("host".into()),
            )),
        );
        snapshot
            .globals
            .insert("plain".to_string(), FlowValue::String("local".into()));
        state.rlm = FlowState::from_snapshot(snapshot);

        let vars = state.bound_variable_values(&BTreeSet::new());

        assert!(vars.iter().any(
            |(name, value)| name == "plain" && value == &FlowValue::String("local".into())
        ));
        assert!(
            !vars.iter().any(|(name, _)| name == "projected"),
            "{vars:?}"
        );
    }

    #[test]
    fn excludes_top_level_globals_containing_nested_projected_values() {
        let mut state = RlmExecutionState::new().unwrap();
        let mut record = FlowRecord::new();
        record.insert(
            "body".to_string(),
            FlowValue::Projected(ProjectedValue::scalar(
                "body",
                FlowValue::String("host".into()),
            )),
        );
        record.insert("title".to_string(), FlowValue::String("local".into()));
        let mut snapshot = state.rlm.snapshot();
        snapshot
            .globals
            .insert("doc".to_string(), FlowValue::Record(Arc::new(record)));
        snapshot.globals.insert(
            "plain".to_string(),
            FlowValue::List(vec![FlowValue::Number(1.0)].into()),
        );
        state.rlm = FlowState::from_snapshot(snapshot);

        let vars = state.bound_variable_values(&BTreeSet::new());

        assert!(vars.iter().any(|(name, _)| name == "plain"));
        assert!(!vars.iter().any(|(name, _)| name == "doc"), "{vars:?}");
    }

    #[derive(Default)]
    struct CountingProjectedValue {
        materialize_count: AtomicUsize,
        render_count: AtomicUsize,
    }

    impl ProjectedHostDescriptor for CountingProjectedValue {
        fn type_name(&self) -> &str {
            "string"
        }

        fn read_one(
            &self,
            request: ProjectedReadRequest,
        ) -> ProjectedFuture<'_, ProjectedReadResponse> {
            Box::pin(async move {
                match request {
                    ProjectedReadRequest::Render => {
                        self.render_count.fetch_add(1, Ordering::SeqCst);
                        ProjectedReadResponse::Text("rendered".to_string())
                    }
                    ProjectedReadRequest::Materialize => {
                        self.materialize_count.fetch_add(1, Ordering::SeqCst);
                        ProjectedReadResponse::Value(FlowValue::String("materialized".into()))
                    }
                    _ => ProjectedReadResponse::Missing,
                }
            })
        }
    }

    #[test]
    fn excludes_custom_projected_globals_without_rendering_or_materializing() {
        let projected = Arc::new(CountingProjectedValue::default());
        let mut state = RlmExecutionState::new().unwrap();
        let mut snapshot = state.rlm.snapshot();
        snapshot.globals.insert(
            "projected".to_string(),
            FlowValue::Projected(ProjectedValue::custom("projected", projected.clone())),
        );
        state.rlm = FlowState::from_snapshot(snapshot);

        let vars = state.bound_variable_values(&BTreeSet::new());

        assert!(vars.is_empty(), "{vars:?}");
        assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
        assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
    }
}
