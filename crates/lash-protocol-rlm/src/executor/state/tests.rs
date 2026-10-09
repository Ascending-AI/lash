//! Tests for the RLM execution-state snapshot root, its keyed leaves, and
//! restore.

use super::*;
use lash_vm::{
    DurableBaseline, DurableFragment, ProjectedReadRequest, ProjectedReadResponse,
    Record as FlowRecord, State as FlowState, Value as FlowValue,
};
use serde_json::json;

fn hydrate(
    snapshot: lash_core::plugin::ExecutionStateCapture,
) -> lash_core::plugin::HydratedExecutionState {
    let lash_core::plugin::ExecutionStateCapture::Replace { root, leaves } = snapshot else {
        panic!("expected replacement capture");
    };
    let components = leaves
        .into_iter()
        .map(|(key, component)| match component {
            lash_core::plugin::LeafChange::Changed(body) => (key, body),
            lash_core::plugin::LeafChange::Unchanged => {
                panic!("fresh test snapshot unexpectedly reused `{key}`")
            }
        })
        .collect();
    lash_core::plugin::HydratedExecutionState { root, components }
}

#[tokio::test]
async fn large_scalar_edit_commits_changed_state_not_retained_session() {
    let mut state = RlmExecutionState::new();
    for index in 0..50 {
        state
            .vm
            .state_mut()
            .insert_global(
                format!("page_{index}"),
                FlowValue::String(format!("page-{index}-{}", "x".repeat(100 * 1024)).into()),
            )
            .await
            .expect("seed a global");
    }
    state.mark_execution_started();
    let initial = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("initial snapshot");
    state.acknowledge_execution_state_capture();

    state
        .vm
        .state_mut()
        .insert_global(
            "page_0".to_string(),
            FlowValue::String(format!("changed-{}", "y".repeat(100 * 1024)).into()),
        )
        .await
        .expect("seed a global");
    state.mark_execution_started();
    let changed = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("changed snapshot");
    let retained_bytes = state
        .vm
        .state()
        .bytes()
        .expect("retained canonical state")
        .len();
    let changed_bytes = measure_snapshot(&changed).checkpoint_bytes;
    let initial_leaves = initial.leaves().len();
    let changed_bodies = changed
        .leaves()
        .values()
        .filter(|component| matches!(component, lash_core::plugin::LeafChange::Changed(_)))
        .count();
    println!(
        "FIG1257_LARGE_SCALAR retained_bytes={retained_bytes} changed_commit_bytes={changed_bytes} initial_leaves={initial_leaves} changed_bodies={changed_bodies}"
    );

    // Pin the heap-backed state and single changed leaf for the current wire shape.
    assert_eq!(retained_bytes, 5_122_656);
    assert_eq!(changed_bytes, 117_882);
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

#[tokio::test]
async fn old_json_snapshot_is_typed_format_rejection_with_cutover_remedy() {
    let old_snapshot = serde_json::to_vec(&json!({
        "version": 5,
        "engine": "lashvm",
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
        .await
        .expect_err("old JSON must not have a compatibility decoder");

    assert!(matches!(&error, RlmSnapshotError::FormatMismatch { .. }));
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

#[tokio::test]
async fn rlm_snapshot_accepts_inline_global_named_schema() {
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global("schema".to_string(), FlowValue::String("note".into()))
        .await
        .expect("seed schema global");
    state.mark_execution_started();

    let snapshot = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("schema global snapshots as canonical RLM state");
    let hydration = hydrate(snapshot);
    let root: RlmSnapshotRoot =
        rmp_serde::from_slice(&hydration.root).expect("schema root decodes");
    assert!(matches!(
        root.globals.get("schema"),
        Some(PersistedValue::Inline { .. })
    ));
}

#[tokio::test]
async fn version_14_root_with_files_field_is_refused_by_the_field_validator() {
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
            engine: "lashvm",
            globals: BTreeMap::new(),
            files: BTreeMap::new(),
            deferred_resolutions: json!({"resolutions": {}}),
        })
        .expect("encode v14 root with unexpected files field")
        .into(),
        components: BTreeMap::new(),
    };
    let mut target = RlmExecutionState::for_engine("lashvm");

    let error = target
        .restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .await
        .expect_err("a v14 root must not accept the removed files field");

    assert!(matches!(
        error,
        RlmSnapshotError::NonCanonicalEnvelope { location, reason }
            if location == "root" && reason.contains("unknown field `files`")
    ));
}

#[tokio::test]
async fn restore_validates_the_snapshot_engine_against_the_active_dialect() {
    let mut source = RlmExecutionState::for_engine("lashvm");
    let hydration = hydrate(
        source
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("source snapshot"),
    );
    let mut target = RlmExecutionState::for_engine("typescript");

    let error = target
        .restore_execution_state(&hydration, lash_core::FleetFormat::current())
        .await
        .expect_err("a snapshot from another dialect must be rejected");

    assert!(matches!(
        error,
        RlmSnapshotError::EngineMismatch { expected, found }
            if expected == "typescript" && found == "lashvm"
    ));
}

/// A real version-22 capture, written by the build before the durable-heap
/// cutover (FIG-3605) for the cell
/// `const kept = [1, 2]; const order = { zeta: 1, alpha: 2 };` in the
/// cell-conformance harness. Both bodies are inline one-binding host-view
/// snapshots at Lash VM snapshot version 7, and the `order` body lists
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
#[tokio::test]
async fn a_predecessor_v22_capture_is_refused_by_its_version_before_anything_is_restored() {
    let root = decode_hex(V22_PREDECESSOR_ROOT_HEX);
    assert_eq!(
        probe_snapshot_version(&root).expect("the predecessor's stamp is readable"),
        22
    );
    let (_, mut live) = leaf_bearing_hydration_and_live_target().await;
    let error = live
        .restore_execution_state(
            &lash_core::plugin::HydratedExecutionState {
                root: root.into(),
                components: BTreeMap::new(),
            },
            lash_core::FleetFormat::current(),
        )
        .await
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
async fn leaf_bearing_hydration_and_live_target()
-> (lash_core::plugin::HydratedExecutionState, RlmExecutionState) {
    let mut source = RlmExecutionState::new();
    source
        .vm
        .state_mut()
        .insert_global(
            "kept".to_string(),
            FlowValue::List(vec![FlowValue::String("source".repeat(2048).into())].into()),
        )
        .await
        .expect("seed a global");
    source.mark_execution_started();
    let hydration = hydrate(
        source
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
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
        .await
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

#[tokio::test]
async fn restore_rejects_a_hydration_that_omits_a_referenced_leaf() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target().await;
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
        .await
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
        .await
        .expect("the untampered hydration still restores");
}

#[tokio::test]
async fn restore_rejects_a_leaf_whose_body_does_not_match_its_content_address() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target().await;
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
        .await
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
        .await
        .expect("the untampered hydration still restores");
}

#[tokio::test]
async fn restore_rejects_a_hydration_carrying_a_leaf_the_root_does_not_reference() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target().await;
    let surplus = leaf_component_key(b"orphan");
    let mut tampered = hydration.clone();
    tampered
        .components
        .insert(surplus.clone(), b"orphan".as_slice().into());

    let error = live
        .restore_execution_state(&tampered, lash_core::FleetFormat::current())
        .await
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
        .await
        .expect("the untampered hydration still restores");
}

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn includes_globals_excludes_history_and_named() {
    let mut state = RlmExecutionState::new();
    let mut set_default = serde_json::Map::new();
    set_default.insert("inventory".to_string(), json!(["lantern"]));
    set_default.insert("secret".to_string(), json!(1));
    state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody { set_default },
            &BTreeSet::new(),
        )
        .await
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

#[tokio::test]
async fn excludes_top_level_globals_containing_nested_projected_values() {
    let mut state = RlmExecutionState::new();
    let mut record = FlowRecord::new();
    // A scalar projection is plain data (ADR 0132 §9); a resource projection
    // is the one that stays a projection inside a held value.
    record.insert(
        "body".to_string(),
        FlowValue::Projected(lash_vm::testing::projection::test_view(
            "body",
            Arc::new(CountingProjectedValue::default()),
        )),
    );
    record.insert("title".to_string(), FlowValue::String("local".into()));
    state
        .vm
        .state_mut()
        .insert_global("doc".to_string(), FlowValue::Record(Arc::new(record)))
        .await
        .expect("seed a global");
    state
        .vm
        .state_mut()
        .insert_global(
            "plain".to_string(),
            FlowValue::List(vec![FlowValue::Number(1.0)].into()),
        )
        .await
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

impl lash_vm::testing::projection::TestView for CountingProjectedValue {
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

#[tokio::test]
async fn excludes_custom_projected_globals_without_rendering_or_materializing() {
    let projected = Arc::new(CountingProjectedValue::default());
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global(
            "projected".to_string(),
            FlowValue::Projected(lash_vm::testing::projection::test_view(
                "projected",
                projected.clone(),
            )),
        )
        .await
        .expect("seed a global");

    let vars = state.bound_variable_values(&BTreeSet::new());

    assert!(vars.is_empty(), "{vars:?}");
    assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
}

/// Fixed-byte authority for the 1.0 root encoding (ADR 0056).
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
fn the_1_0_root_encodes_to_golden_bytes() {
    const GOLDEN: &str = concat!(
        "84a776657273696f6e01a6656e67696e65a66c617368766dac73746174655f686561646572c40a81a776657273696f6e",
        "01a7676c6f62616c7382ad696e6c696e655f7363616c617282a46b696e64a6696e6c696e65a4626f6479c42982a57661",
        "6c756582a46b696e64a6737472696e67a576616c7565a5736d616c6ca76f626a6563747390b06c65616665645f636f6d",
        "706f7369746582a46b696e64a46c656166a9636f6d706f6e656e74d957657865637574696f6e5f73746174652f626c61",
        "6b65332f6366373738323463326331323166303066313362656334313962616430646466376665393064663931373065",
        "3732303139643938633732356164653966363561",
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
        engine: "lashvm".to_string(),
        state_header: FlowState::new()
            .durable_parts(
                &DurableBaseline::default(),
                lash_core::FleetFormat::current(),
            )
            .expect("encode the plain state's header")
            .header,
        globals,
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
        "the 1.0 root encoding changed; regenerate the golden for an intended shape change"
    );

    let decoded: RlmSnapshotRoot =
        rmp_serde::from_slice(&encoded).expect("the golden root round-trips");
    assert_eq!(decoded.version, RLM_SNAPSHOT_VERSION);
    assert_eq!(
        root_leaf_keys(&decoded),
        [ExecutionLeafName::new(
            "blake3/cf77824c2c121f00f13bec419bad0ddf7fe90df9170e72019d98c725ade9f65a"
        ),]
        .into_iter()
        .collect()
    );
}
