//! Tests for the RLM execution-state snapshot root, its keyed leaves, and
//! restore.

use super::*;
use crate::dialect::{RlmDialectServices, SessionDialect};
use lashlang::{
    DurableBaseline, DurableFragment, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, Record as FlowRecord, State as FlowState,
    Value as FlowValue,
};
use serde_json::json;

#[test]
fn generated_snapshot_field_schemas_match_all_fields_set_serialization() {
    use lash_lashlang_runtime::{
        DeferredResolutionLinkKey, DeferredTriggerResolutionRecord, TriggerGrant, TriggerResolution,
    };
    let link_key = DeferredResolutionLinkKey {
        address: lash_core::EffectAddress::new(
            lash_core::ExecutionScope::turn("session", "turn"),
            "replay",
        )
        .expect("valid snapshot test address"),
    };
    let trigger_grant: TriggerGrant = serde_json::from_value(json!({
        "provider_id": "calendar",
        "constructor_path": ["calendar", "Changed"],
        "input_type": {"Union": [
            {"Process": {"kind": "unknown"}},
            {"List": {"TriggerHandle": "Str"}}
        ]},
        "event_type": {
            "name": "calendar.Change",
            "ty": {"Object": [{"name": "id", "ty": "Str", "optional": false}]}
        },
        "route": {"account": "primary"}
    }))
    .expect("valid trigger grant fixture");
    let trigger_resolution = TriggerResolution::Resolved(Box::new(trigger_grant));
    let deferred_trigger_resolutions = DeferredTriggerResolutionRecord {
        link_key: Some(link_key.clone()),
        resolutions: BTreeMap::from([("calendar.Changed".to_string(), trigger_resolution.clone())]),
    };
    let root = RlmSnapshotRoot {
        version: RLM_SNAPSHOT_VERSION,
        engine: "lashlang".to_string(),
        state_header: vec![1],
        globals: BTreeMap::from([
            (
                "inline".to_string(),
                PersistedValue::Inline { body: vec![1] },
            ),
            (
                "leaf".to_string(),
                PersistedValue::Leaf {
                    component: "sha256:test".to_string(),
                },
            ),
        ]),
        deferred_trigger_resolutions: deferred_trigger_resolutions.clone(),
    };

    assert_field_schema(
        ROOT_FIELDS,
        &[
            "version",
            "engine",
            "state_header",
            "globals",
            "deferred_trigger_resolutions",
        ],
        &[serialized_fields(&root)],
    );
    assert_field_schema(
        PERSISTED_VALUE_FIELDS,
        &["kind", "body", "component"],
        &[
            serialized_fields(&PersistedValue::Inline { body: vec![1] }),
            serialized_fields(&PersistedValue::Leaf {
                component: "sha256:test".to_string(),
            }),
        ],
    );
    assert_field_schema(
        DEFERRED_LINK_KEY_FIELDS,
        &["address"],
        &[serialized_fields(&link_key)],
    );
    assert_field_schema(
        DEFERRED_TRIGGER_RESOLUTION_FIELDS,
        &["link_key", "resolutions"],
        &[serialized_fields(&deferred_trigger_resolutions)],
    );
    assert_field_schema(
        TRIGGER_RESOLUTION_FIELDS,
        &[
            "kind",
            "provider_id",
            "constructor_path",
            "input_type",
            "event_type",
            "route",
            "provider_ids",
        ],
        &[
            serialized_fields(&trigger_resolution),
            serialized_fields(&TriggerResolution::NotAvailable),
            serialized_fields(&TriggerResolution::Ambiguous {
                provider_ids: vec!["a".to_string(), "b".to_string()],
            }),
        ],
    );
}

fn assert_field_schema(generated: &[&str], expected: &[&str], serialized: &[Vec<String>]) {
    let mut serialized_union = Vec::new();
    for fields in serialized {
        for field in fields {
            if !serialized_union.contains(field) {
                serialized_union.push(field.clone());
            }
        }
    }
    assert_eq!(generated, expected, "generated field schema changed");
    assert_eq!(serialized_union, expected, "serialized field order changed");
}

fn serialized_fields(value: &impl Serialize) -> Vec<String> {
    let encoded = serde_json::to_string(value).expect("serialize all-fields-set witness");
    top_level_json_object_keys(&encoded)
}

fn top_level_json_object_keys(encoded: &str) -> Vec<String> {
    let bytes = encoded.as_bytes();
    assert_eq!(
        bytes.first(),
        Some(&b'{'),
        "witness must serialize as a map"
    );
    let mut keys = Vec::new();
    let mut cursor = 1;
    while cursor < bytes.len() {
        while matches!(bytes.get(cursor), Some(b' ' | b'\n' | b'\r' | b'\t' | b',')) {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'}') {
            break;
        }
        let key_end = json_string_end(bytes, cursor);
        keys.push(
            serde_json::from_str::<String>(&encoded[cursor..key_end])
                .expect("decode serialized field name"),
        );
        cursor = key_end;
        while matches!(bytes.get(cursor), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            cursor += 1;
        }
        assert_eq!(bytes.get(cursor), Some(&b':'));
        cursor += 1;
        cursor = json_value_end(bytes, cursor);
    }
    keys
}

fn json_string_end(bytes: &[u8], start: usize) -> usize {
    assert_eq!(bytes.get(start), Some(&b'"'));
    let mut cursor = start + 1;
    let mut escaped = false;
    while cursor < bytes.len() {
        match (bytes[cursor], escaped) {
            (_, true) => escaped = false,
            (b'\\', false) => escaped = true,
            (b'"', false) => return cursor + 1,
            _ => {}
        }
        cursor += 1;
    }
    panic!("unterminated JSON string")
}

fn json_value_end(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if in_string {
            match (byte, escaped) {
                (_, true) => escaped = false,
                (b'\\', false) => escaped = true,
                (b'"', false) => in_string = false,
                _ => {}
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth > 0 => depth -= 1,
                b',' | b'}' if depth == 0 => return cursor,
                _ => {}
            }
        }
        cursor += 1;
    }
    cursor
}

fn hydrate(
    snapshot: lash_core::plugin::ExecutionStateSnapshot,
) -> lash_core::plugin::HydratedExecutionState {
    let components = snapshot
        .components
        .into_iter()
        .map(|(key, component)| match component {
            lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => (key, body),
            lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                panic!("fresh test snapshot unexpectedly reused `{key}`")
            }
        })
        .collect();
    lash_core::plugin::HydratedExecutionState {
        root: snapshot.root.expect("snapshot root"),
        components,
    }
}

#[test]
fn large_scalar_edit_commits_changed_state_not_retained_session() {
    let mut state = RlmExecutionState::new();
    for index in 0..50 {
        state
            .vm
            .state_mut()
            .insert_global(
                format!("page_{index}"),
                FlowValue::String(format!("page-{index}-{}", "x".repeat(100 * 1024)).into()),
            )
            .expect("seed a global");
    }
    state.mark_execution_started();
    let initial = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .expect("initial snapshot");
    state.acknowledge_execution_state_capture();

    state
        .vm
        .state_mut()
        .insert_global(
            "page_0".to_string(),
            FlowValue::String(format!("changed-{}", "y".repeat(100 * 1024)).into()),
        )
        .expect("seed a global");
    state.mark_execution_started();
    let changed = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .expect("changed snapshot");
    let retained_bytes = state
        .vm
        .state()
        .bytes()
        .expect("retained canonical state")
        .len();
    let changed_bytes = measure_snapshot(&changed).checkpoint_bytes;
    let initial_leaves = initial.components.len();
    let changed_bodies = changed
        .components
        .values()
        .filter(|component| {
            matches!(
                component,
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
            )
        })
        .count();
    println!(
        "FIG1257_LARGE_SCALAR retained_bytes={retained_bytes} changed_commit_bytes={changed_bytes} initial_leaves={initial_leaves} changed_bodies={changed_bodies}"
    );

    // The seeded state is heap-backed from its first host write (FIG-3605),
    // so the retained snapshot carries the heap form's counters and roots.
    assert_eq!(retained_bytes, 5_122_708);
    // The encoded root includes trigger metadata; tool outcomes stay in the journal.
    assert_eq!(changed_bytes, 117_977);
    assert_eq!(initial_leaves, 50);
    assert_eq!(changed_bodies, 1);
}

/// The durable fragment one binding holding `value` encodes to.
fn fragment_body(value: FlowValue) -> Vec<u8> {
    let mut state = FlowState::new();
    state
        .insert_global("value", value)
        .expect("seed the fragment's binding");
    let mut parts = state
        .durable_parts(
            &DurableBaseline::default(),
            lash_core::FleetFormat::current(),
        )
        .expect("encode the fragment");
    match parts.fragments.remove("value") {
        Some(DurableFragment::Changed(body)) => body,
        other => panic!("a fresh capture must encode every fragment, got {other:?}"),
    }
}

fn canonical_string_global_body(body_len: usize) -> Vec<u8> {
    for string_len in 0..=body_len {
        let body = fragment_body(FlowValue::String("x".repeat(string_len).into()));
        if body.len() == body_len {
            return body;
        }
    }
    panic!("no canonical string global body has length {body_len}");
}

#[test]
fn size_line_selects_literal_global_boundaries() {
    let prior_leaf_keys = BTreeSet::new();

    let global_511 = canonical_string_global_body(511);
    assert_eq!(global_511.len(), 511);
    let mut changed_leaves = BTreeMap::new();
    assert!(matches!(
        persist_value_body(global_511, &prior_leaf_keys, &mut changed_leaves),
        PersistedValue::Inline { .. }
    ));
    assert_eq!(changed_leaves.len(), 0);

    let global_512 = canonical_string_global_body(512);
    assert_eq!(global_512.len(), 512);
    let mut changed_leaves = BTreeMap::new();
    assert!(matches!(
        persist_value_body(global_512, &prior_leaf_keys, &mut changed_leaves),
        PersistedValue::Leaf { .. }
    ));
    assert_eq!(changed_leaves.len(), 1);

    let global_513 = canonical_string_global_body(513);
    assert_eq!(global_513.len(), 513);
    let mut changed_leaves = BTreeMap::new();
    assert!(matches!(
        persist_value_body(global_513, &prior_leaf_keys, &mut changed_leaves),
        PersistedValue::Leaf { .. }
    ));
    assert_eq!(changed_leaves.len(), 1);
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
    let mut state = RlmExecutionState::new();

    let error = state
        .restore_execution_state(
            &lash_core::plugin::HydratedExecutionState {
                root: old_snapshot.into(),
                components: BTreeMap::new(),
            },
            lash_core::FleetFormat::current(),
        )
        .expect_err("old JSON must not have a compatibility decoder");

    assert!(matches!(&error, RlmSnapshotError::FormatMismatch { .. }));
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

fn canonical_path(keys: &[&str]) -> Vec<CanonicalPathSegment> {
    keys.iter()
        .map(|key| CanonicalPathSegment::Key((*key).to_string()))
        .collect()
}

#[test]
fn canonical_root_recognizes_global_keys_by_position() {
    for key in ["x].y", "ordinary", "schema", "input_schema", "bindings"] {
        assert_eq!(
            root_node(&canonical_path(&["globals", key])),
            RootNode::Global
        );
        assert_eq!(
            root_node(&canonical_path(&["globals", key, "component"])),
            RootNode::Other
        );
    }
}

#[test]
fn root_classifier_prefers_envelope_entries_over_json_field_names() {
    assert_eq!(
        root_map_order(&canonical_path(&[
            "deferred_trigger_resolutions",
            "resolutions",
            "schema"
        ])),
        CanonicalMapOrder::Declared(TRIGGER_RESOLUTION_FIELDS)
    );
    assert_eq!(
        root_map_order(&canonical_path(&[
            "deferred_trigger_resolutions",
            "resolutions",
            "trigger",
            "route",
            "account"
        ])),
        CanonicalMapOrder::Sorted
    );
}

#[test]
fn canonical_resolution_field_order_is_independent_of_key_shape() {
    // Hand-written MessagePack pins ordering independently of serde's encoder.
    fn string(bytes: &mut Vec<u8>, value: &str) {
        assert!(value.len() < 32);
        bytes.push(0xa0 | u8::try_from(value.len()).expect("fixstr length"));
        bytes.extend_from_slice(value.as_bytes());
    }
    for key in ["module.operation", "bare", "x].y", "schema"] {
        for reversed in [false, true] {
            let mut bytes = vec![0x81];
            string(&mut bytes, "deferred_trigger_resolutions");
            bytes.push(0x81);
            string(&mut bytes, "resolutions");
            bytes.push(0x81);
            string(&mut bytes, key);
            bytes.push(0x82);
            let fields = if reversed {
                ["provider_id", "kind"]
            } else {
                ["kind", "provider_id"]
            };
            for field in fields {
                string(&mut bytes, field);
                string(&mut bytes, "value");
            }
            let result = validate_canonical_root(&bytes);
            if reversed {
                assert!(
                    matches!(result, Err(RlmSnapshotError::NonCanonicalEnvelope { ref reason, .. }) if reason.contains("canonical declaration order")),
                    "key {key}"
                );
            } else {
                result.expect("declared resolution order must be accepted for every key shape");
            }
        }
    }
}

#[test]
fn rlm_snapshot_accepts_inline_global_named_schema() {
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global("schema".to_string(), FlowValue::String("note".into()))
        .expect("seed schema global");
    state.mark_execution_started();

    let snapshot = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .expect("schema global snapshots as canonical RLM state");
    let hydration = hydrate(snapshot);
    let root: RlmSnapshotRoot =
        rmp_serde::from_slice(&hydration.root).expect("schema root decodes");
    assert!(matches!(
        root.globals.get("schema"),
        Some(PersistedValue::Inline { .. })
    ));
}

#[test]
fn older_snapshot_version_is_typed_rejection_with_cutover_remedy() {
    #[derive(Serialize)]
    struct PreviousEnvelope {
        version: u32,
        engine: &'static str,
        #[serde(with = "serde_bytes")]
        vars: Vec<u8>,
        files: BTreeMap<String, String>,
        deferred_resolutions: serde_json::Value,
    }
    let hydration = lash_core::plugin::HydratedExecutionState {
        root: rmp_serde::to_vec_named(&PreviousEnvelope {
            version: RLM_SNAPSHOT_VERSION - 2,
            engine: "lashlang",
            vars: lashlang::Snapshot::default()
                .to_canonical_bytes()
                .expect("previous vars"),
            files: BTreeMap::new(),
            deferred_resolutions: json!({"resolutions": {}}),
        })
        .expect("previous envelope")
        .into(),
        components: BTreeMap::new(),
    };
    let mut target = RlmExecutionState::new();

    let error = target
        .restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .expect_err("older version must be rejected before Lashlang decode");

    assert!(matches!(
        &error,
        RlmSnapshotError::VersionMismatch {
            expected: RLM_SNAPSHOT_VERSION,
            found
        } if *found == RLM_SNAPSHOT_VERSION - 2
    ));
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

#[test]
fn version_17_snapshot_is_typed_rejection_with_or_without_file_leaves() {
    const EFFECT_ADDRESS_PREDECESSOR_SNAPSHOT_VERSION: u32 = 17;
    const { assert!(RLM_SNAPSHOT_VERSION > EFFECT_ADDRESS_PREDECESSOR_SNAPSHOT_VERSION) };

    #[derive(Serialize)]
    struct PreviousEnvelope {
        version: u32,
        engine: &'static str,
        globals: BTreeMap<String, PersistedValue>,
        files: BTreeMap<String, PersistedValue>,
        deferred_resolutions: serde_json::Value,
    }

    let global_body = canonical_string_global_body(512);
    let global_component = leaf_component_key(&global_body);
    for include_file_leaf in [false, true] {
        let file_body = vec![0xa5; 513];
        let file_component = leaf_component_key(&file_body);
        let files = include_file_leaf
            .then(|| {
                (
                    "obsolete.txt".to_string(),
                    PersistedValue::Leaf {
                        component: file_component.clone(),
                    },
                )
            })
            .into_iter()
            .collect();
        let mut components =
            BTreeMap::from([(global_component.clone(), global_body.clone().into())]);
        if include_file_leaf {
            components.insert(file_component, file_body.into());
        }
        let hydration = lash_core::plugin::HydratedExecutionState {
            root: rmp_serde::to_vec_named(&PreviousEnvelope {
                version: EFFECT_ADDRESS_PREDECESSOR_SNAPSHOT_VERSION,
                engine: "lashlang",
                globals: [(
                    "kept".to_string(),
                    PersistedValue::Leaf {
                        component: global_component.clone(),
                    },
                )]
                .into_iter()
                .collect(),
                files,
                deferred_resolutions: json!({"resolutions": {}}),
            })
            .expect("encode previous root")
            .into(),
            components,
        };

        let mut target = RlmExecutionState::for_engine("lashlang");
        let error = target
            .restore_execution_state(&hydration, lash_core::FleetFormat::current())
            .expect_err("the previous snapshot version must fail closed");

        assert!(matches!(
            &error,
            RlmSnapshotError::VersionMismatch {
                expected: RLM_SNAPSHOT_VERSION,
                found
            } if *found == EFFECT_ADDRESS_PREDECESSOR_SNAPSHOT_VERSION
        ));
        let message = error.to_string();
        assert!(message.contains("drain in-flight sessions on the old build"));
        assert!(message.contains("recreate development/test stores"));
    }
}

#[test]
fn version_14_root_with_files_field_is_refused_by_the_field_validator() {
    #[derive(Serialize)]
    struct UnexpectedFilesEnvelope {
        version: u32,
        engine: &'static str,
        globals: BTreeMap<String, PersistedValue>,
        files: BTreeMap<String, PersistedValue>,
        deferred_resolutions: serde_json::Value,
    }

    let hydration = lash_core::plugin::HydratedExecutionState {
        root: rmp_serde::to_vec_named(&UnexpectedFilesEnvelope {
            version: RLM_SNAPSHOT_VERSION,
            engine: "lashlang",
            globals: BTreeMap::new(),
            files: BTreeMap::new(),
            deferred_resolutions: json!({"resolutions": {}}),
        })
        .expect("encode v14 root with unexpected files field")
        .into(),
        components: BTreeMap::new(),
    };
    let mut target = RlmExecutionState::for_engine("lashlang");

    let error = target
        .restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .expect_err("a v14 root must not accept the removed files field");

    assert!(matches!(
        error,
        RlmSnapshotError::NonCanonicalEnvelope { location, reason }
            if location == "root" && reason.contains("unknown field `files`")
    ));
}

#[test]
fn restore_validates_the_snapshot_engine_against_the_active_dialect() {
    let mut source = RlmExecutionState::for_engine("lashlang");
    let hydration = hydrate(
        source
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .expect("source snapshot"),
    );
    let mut target = RlmExecutionState::for_engine("typescript");

    let error = target
        .restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .expect_err("a snapshot from another dialect must be rejected");

    assert!(matches!(
        error,
        RlmSnapshotError::EngineMismatch { expected, found }
            if expected == "typescript" && found == "lashlang"
    ));
}

/// Fixed-byte authority for the version-26 root encoding (ADR 0056).
///
/// Encoding both sides of a comparison with the currently linked encoder
/// cannot see the drift that matters: a dependency bump or serializer change
/// moves both sides together, and the root validator deliberately accepts
/// any declared-field order, so the same logical state could silently
/// acquire different bytes — and therefore a different component identity —
/// without detection. These bytes pin the current shape. Under the pre-1.0
/// version freeze, regenerate this witness from the encoder after an intended
/// shape change; the version stays fixed.
// The golden pins N's encoding; the synthetic N+1 moves the root's stamps.
#[cfg(not(feature = "synthetic-next"))]
#[test]
fn version_26_root_encodes_to_golden_bytes() {
    const GOLDEN: &str = concat!(
        "85a776657273696f6e1aa6656e67696e65a86c6173686c616e67ac73746174655f686561646572c40a81a776657273696f6e",
        "0ea7676c6f62616c7382ad696e6c696e655f7363616c617282a46b696e64a6696e6c696e65a4626f6479c42982a576616c75",
        "6582a46b696e64a6737472696e67a576616c7565a5736d616c6ca76f626a6563747390b06c65616665645f636f6d706f7369",
        "746582a46b696e64a46c656166a9636f6d706f6e656e74d957657865637574696f6e5f73746174652f626c616b65332f6366",
        "3737383234633263313231663030663133626563343139626164306464663766653930646639313730653732303139643938",
        "633732356164653966363561bc64656665727265645f747269676765725f7265736f6c7574696f6e7381ab7265736f6c7574",
        "696f6e7380",
    );

    let prior_leaf_keys = BTreeSet::new();
    let mut changed_leaves = BTreeMap::new();
    let inline_global = persist_value_body(
        fragment_body(FlowValue::String("small".into())),
        &prior_leaf_keys,
        &mut changed_leaves,
    );
    assert!(matches!(inline_global, PersistedValue::Inline { .. }));
    let leaf_global = persist_value_body(
        canonical_string_global_body(512),
        &prior_leaf_keys,
        &mut changed_leaves,
    );
    assert!(matches!(leaf_global, PersistedValue::Leaf { .. }));
    assert_eq!(changed_leaves.len(), 1);
    let mut globals = BTreeMap::new();
    globals.insert("inline_scalar".to_string(), inline_global);
    globals.insert("leafed_composite".to_string(), leaf_global);
    let root = RlmSnapshotRoot {
        version: RLM_SNAPSHOT_VERSION,
        engine: "lashlang".to_string(),
        state_header: FlowState::new()
            .durable_parts(
                &DurableBaseline::default(),
                lash_core::FleetFormat::current(),
            )
            .expect("encode the plain state's header")
            .header,
        globals,
        deferred_trigger_resolutions:
            lash_lashlang_runtime::DeferredTriggerResolutionRecord::default(),
    };

    let encoded = rmp_serde::to_vec_named(&root).expect("encode the golden root");
    validate_canonical_root(&encoded).expect("the golden root is canonical");
    let hex = encoded
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("RLM_ROOT_GOLDEN_HEX={hex}");
    assert_eq!(
        hex, GOLDEN,
        "the version-26 root encoding changed; regenerate the golden for an intended shape change"
    );

    let decoded: RlmSnapshotRoot =
        rmp_serde::from_slice(&encoded).expect("the golden root round-trips");
    assert_eq!(decoded.version, RLM_SNAPSHOT_VERSION);
    assert_eq!(
        root_leaf_keys(&decoded),
        [
            "execution_state/blake3/cf77824c2c121f00f13bec419bad0ddf7fe90df9170e72019d98c725ade9f65a"
                .to_string(),
        ]
        .into_iter()
        .collect()
    );
}

/// A real version-22 capture, written by the build before the durable-heap
/// cutover (FIG-3605) for the cell
/// `const kept = [1, 2]; const order = { zeta: 1, alpha: 2 };` in the
/// cell-conformance harness. Both bodies are inline one-binding host-view
/// snapshots at Lashlang snapshot version 7, and the `order` body lists
/// `alpha` before `zeta`: the sorted order FIG-3606 removes.
const V22_PREDECESSOR_ROOT_HEX: &str = concat!(
    "86a776657273696f6e16a6656e67696e65aa74797065736372697074a7676c6f62616c7382a46b65707482a46b696e64",
    "a6696e6c696e65a4626f6479c46f82a776657273696f6e07a7676c6f62616c739182a46e616d65a576616c7565a57661",
    "6c756582a46b696e64a46c697374a56974656d739282a46b696e64a66e756d626572a576616c7565cb3ff00000000000",
    "0082a46b696e64a66e756d626572a576616c7565cb4000000000000000a56f7264657282a46b696e64a6696e6c696e65",
    "a4626f6479c49582a776657273696f6e07a7676c6f62616c739182a46e616d65a576616c7565a576616c756582a46b69",
    "6e64a67265636f7264a66669656c64739282a46e616d65a5616c706861a576616c756582a46b696e64a66e756d626572",
    "a576616c7565cb400000000000000082a46e616d65a47a657461a576616c756582a46b696e64a66e756d626572a57661",
    "6c7565cb3ff0000000000000b464656665727265645f7265736f6c7574696f6e7382a86c696e6b5f6b657981a7616464",
    "7265737382af657865637574696f6e5f73636f706583a474797065a47475726eaa73657373696f6e5f6964b863656c6c",
    "2d636f6e666f726d616e63652d73657373696f6ea77475726e5f6964b563656c6c2d636f6e666f726d616e63652d7475",
    "726eaa7265706c61795f6b6579d923657865632d636f64653a63656c6c2d636f6e666f726d616e63653a303030303030",
    "3030ab7265736f6c7574696f6e7380bc64656665727265645f747269676765725f7265736f6c7574696f6e7382a86c69",
    "6e6b5f6b657981a76164647265737382af657865637574696f6e5f73636f706583a474797065a47475726eaa73657373",
    "696f6e5f6964b863656c6c2d636f6e666f726d616e63652d73657373696f6ea77475726e5f6964b563656c6c2d636f6e",
    "666f726d616e63652d7475726eaa7265706c61795f6b6579d923657865632d636f64653a63656c6c2d636f6e666f726d",
    "616e63653a3030303030303030ab7265736f6c7574696f6e7380b26368696c645f6d61785f617474656d707473c0",
);

fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("fixture hex"))
        .collect()
}

/// The clean cutover refuses a predecessor's capture with the typed version
/// boundary before anything is restored, and leaves its stamp readable, so a
/// host can tell which sessions predate this build.
#[test]
fn a_predecessor_v22_capture_is_refused_by_its_version_before_anything_is_restored() {
    let root = decode_hex(V22_PREDECESSOR_ROOT_HEX);
    assert_eq!(
        probe_snapshot_version(&root).expect("the predecessor's stamp is readable"),
        22
    );
    let (_, mut live) = leaf_bearing_hydration_and_live_target();
    let error = live
        .restore_execution_state(
            &lash_core::plugin::HydratedExecutionState {
                root: root.into(),
                components: BTreeMap::new(),
            },
            lash_core::FleetFormat::current(),
        )
        .expect_err("a version-22 capture must not restore");
    assert!(
        matches!(
            &error,
            RlmSnapshotError::VersionMismatch {
                expected: RLM_SNAPSHOT_VERSION,
                found: 22,
            }
        ),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
    assert_live_state_untouched(&live);
}

/// A leaf-bearing hydration plus a distinct live target, so a rejected
/// restore can be checked for having changed nothing.
fn leaf_bearing_hydration_and_live_target()
-> (lash_core::plugin::HydratedExecutionState, RlmExecutionState) {
    let mut source = RlmExecutionState::new();
    source
        .vm
        .state_mut()
        .insert_global(
            "kept".to_string(),
            FlowValue::List(vec![FlowValue::String("source".repeat(2048).into())].into()),
        )
        .expect("seed a global");
    source.mark_execution_started();
    let hydration = hydrate(
        source
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .expect("source snapshot"),
    );
    assert!(
        !hydration.components.is_empty(),
        "the hydration must reference at least one leaf"
    );

    let mut live = RlmExecutionState::new();
    live.vm
        .state_mut()
        .insert_global("live".to_string(), FlowValue::String("untouched".into()))
        .expect("seed a global");
    live.mark_execution_started();
    (hydration, live)
}

fn assert_live_state_untouched(live: &RlmExecutionState) {
    assert_eq!(
        live.vm.state().globals().get("live"),
        Some(&FlowValue::String("untouched".into())),
        "a rejected restore must not replace live globals"
    );
    assert!(
        live.vm.state().globals().get("kept").is_none(),
        "a rejected restore must not leak the source's globals"
    );
}

#[test]
fn restore_rejects_a_hydration_that_omits_a_referenced_leaf() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target();
    let dropped = hydration
        .components
        .keys()
        .next()
        .expect("a referenced leaf")
        .clone();
    let mut tampered = hydration.clone();
    tampered.components.remove(&dropped);

    let error = live
        .restore_execution_state(&tampered, lash_core::FleetFormat::current())
        .expect_err("a root referencing an unsupplied leaf must be rejected");

    match &error {
        RlmSnapshotError::LeafSetMismatch {
            missing,
            unexpected,
        } => {
            assert_eq!(missing, &vec![dropped]);
            assert!(unexpected.is_empty());
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_live_state_untouched(&live);
    live.restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .expect("the untampered hydration still restores");
}

#[test]
fn restore_rejects_a_leaf_whose_body_does_not_match_its_content_address() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target();
    let key = hydration
        .components
        .keys()
        .next()
        .expect("a referenced leaf")
        .clone();
    let mut tampered = hydration.clone();
    tampered
        .components
        .insert(key.clone(), b"tampered body".as_slice().into());

    let error = live
        .restore_execution_state(&tampered, lash_core::FleetFormat::current())
        .expect_err("a leaf body that is not its own content address must be rejected");

    match &error {
        RlmSnapshotError::LeafHashMismatch {
            component,
            actual_component,
            ..
        } => {
            assert_eq!(component, &key);
            assert_eq!(actual_component, &leaf_component_key(b"tampered body"));
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_live_state_untouched(&live);
    live.restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .expect("the untampered hydration still restores");
}

#[test]
fn restore_rejects_a_hydration_carrying_a_leaf_the_root_does_not_reference() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target();
    let surplus = leaf_component_key(b"orphan");
    let mut tampered = hydration.clone();
    tampered
        .components
        .insert(surplus.clone(), b"orphan".as_slice().into());

    let error = live
        .restore_execution_state(&tampered, lash_core::FleetFormat::current())
        .expect_err("an orphan leaf must be rejected rather than silently ignored");

    match &error {
        RlmSnapshotError::LeafSetMismatch {
            missing,
            unexpected,
        } => {
            assert!(missing.is_empty());
            assert_eq!(unexpected, &vec![surplus]);
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_live_state_untouched(&live);
    live.restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .expect("the untampered hydration still restores");
}

/// `MissingLeaf` is the defence behind the exact-set check: restore compares
/// the supplied key set with the root's first, so a resolution that finds no
/// body cannot be reached through `restore_execution_state`. Pinning it here
/// keeps it a typed rejection rather than a panic if that order ever changes.
#[test]
fn resolving_an_absent_leaf_is_a_typed_missing_leaf_rejection() {
    let state = lash_core::plugin::HydratedExecutionState::default();

    let error = resolve_leaf(&state, "kept", "execution_state/blake3/absent")
        .expect_err("an absent leaf must not resolve");

    assert!(matches!(
        &error,
        RlmSnapshotError::MissingLeaf {
            logical_key,
            component,
        } if logical_key == "kept" && component == "execution_state/blake3/absent"
    ));
}

#[test]
fn aborted_capture_retries_leaf_bodies_instead_of_uncommitted_refs() {
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global(
            "large".to_string(),
            FlowValue::List(vec![FlowValue::String("x".repeat(8 * 1024).into())].into()),
        )
        .expect("seed a global");
    state.mark_execution_started();
    let first = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .expect("first capture");
    assert!(first.components.values().any(|component| matches!(
        component,
        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
    )));

    state.abort_execution_state_capture();
    let retry = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .expect("retry capture");
    assert!(retry.components.values().any(|component| matches!(
        component,
        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
    )));
}

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn includes_globals_excludes_history_and_named() {
    let mut state = RlmExecutionState::new();
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
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global(
            "projected".to_string(),
            FlowValue::Projected(ProjectedValue::scalar(
                "projected",
                FlowValue::String("host".into()),
            )),
        )
        .expect("seed a global");
    state
        .vm
        .state_mut()
        .insert_global("plain".to_string(), FlowValue::String("local".into()))
        .expect("seed a global");

    let vars = state.bound_variable_values(&BTreeSet::new());

    assert!(
        vars.iter()
            .any(|(name, value)| name == "plain" && value == &FlowValue::String("local".into()))
    );
    assert!(
        !vars.iter().any(|(name, _)| name == "projected"),
        "{vars:?}"
    );
}

#[test]
fn excludes_top_level_globals_containing_nested_projected_values() {
    let mut state = RlmExecutionState::new();
    let mut record = FlowRecord::new();
    record.insert(
        "body".to_string(),
        FlowValue::Projected(ProjectedValue::scalar(
            "body",
            FlowValue::String("host".into()),
        )),
    );
    record.insert("title".to_string(), FlowValue::String("local".into()));
    state
        .vm
        .state_mut()
        .insert_global("doc".to_string(), FlowValue::Record(Arc::new(record)))
        .expect("seed a global");
    state
        .vm
        .state_mut()
        .insert_global(
            "plain".to_string(),
            FlowValue::List(vec![FlowValue::Number(1.0)].into()),
        )
        .expect("seed a global");

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

    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        match request {
            ProjectedReadRequest::Render => {
                self.render_count.fetch_add(1, Ordering::SeqCst);
                Some(ProjectedReadResponse::Text("rendered".to_string()))
            }
            ProjectedReadRequest::Materialize => {
                self.materialize_count.fetch_add(1, Ordering::SeqCst);
                Some(ProjectedReadResponse::Value(FlowValue::String(
                    "materialized".into(),
                )))
            }
            _ => None,
        }
    }
}

#[test]
fn excludes_custom_projected_globals_without_rendering_or_materializing() {
    let projected = Arc::new(CountingProjectedValue::default());
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global(
            "projected".to_string(),
            FlowValue::Projected(ProjectedValue::custom("projected", projected.clone())),
        )
        .expect("seed a global");

    let vars = state.bound_variable_values(&BTreeSet::new());

    assert!(vars.is_empty(), "{vars:?}");
    assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
}

#[test]
fn the_dialect_pins_snapshot_engine_id() {
    let dialect = SessionDialect::new(
        std::sync::Arc::new(crate::dialect::TypescriptDialect),
        lash_lashlang_runtime::LashlangSurface::default(),
        RlmDialectServices {
            workers: lash_vm_client::service::Service::default(),
            artifact_store: crate::testing::sqlite_memory_artifact_store_blocking(),
            deferred_tool_resolver: None,
            deferred_trigger_resolver: None,
            execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
            execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
            code_renderer: Default::default(),
            channel: crate::plugin::RlmChannel::Cell,
        },
    );

    let mut session = dialect.create_session();
    let snapshot = session
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .expect("snapshot the session");
    let root: RlmSnapshotRoot =
        rmp_serde::from_slice(snapshot.root.as_deref().expect("fresh snapshot has a root"))
            .expect("decode snapshot root");
    assert_eq!(root.engine, "typescript");
}

#[test]
fn persisted_root_fields_and_encoder_floor_are_pinned() {
    assert_eq!(
        ROOT_FIELDS,
        &[
            "version",
            "engine",
            "state_header",
            "globals",
            "deferred_trigger_resolutions"
        ]
    );
    let root = RlmSnapshotRoot {
        version: RLM_SNAPSHOT_VERSION,
        engine: "lashlang".into(),
        state_header: vec![0, 255],
        globals: BTreeMap::new(),
        deferred_trigger_resolutions: Default::default(),
    };
    let encoded = rmp_serde::to_vec_named(&root).expect("encode root");
    assert_eq!(encoded[0], 0x85, "the root is a named five-field map");
    let decoded: BTreeMap<String, serde::de::IgnoredAny> =
        rmp_serde::from_slice(&encoded).expect("read named map");
    assert_eq!(
        decoded.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "deferred_trigger_resolutions",
            "engine",
            "globals",
            "state_header",
            "version"
        ]
    );
    assert!(
        encoded
            .windows(5)
            .any(|bytes| bytes == [0xc4, 2, 0, 255, 0xa7]),
        "state_header is binary, not an integer array"
    );
    let manifest = include_str!("../../../../../Cargo.toml");
    assert_eq!(
        manifest
            .lines()
            .find(|line| line.starts_with("rmp-serde = ")),
        Some("rmp-serde = \"1.3.1\""),
        "the minimum encoder includes the named-map fix"
    );
}
