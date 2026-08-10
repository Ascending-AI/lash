//! Tests for the RLM execution-state snapshot root, its keyed leaves, and
//! restore.

use super::*;
use lashlang::{
    ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest, ProjectedReadResponse,
    ProjectedValue, Record as FlowRecord, Value as FlowValue,
};
use serde_json::json;

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
        .restore_execution_state(&lash_core::plugin::HydratedExecutionState {
            root: old_snapshot,
            components: BTreeMap::new(),
        })
        .expect_err("old JSON must not have a compatibility decoder");

    assert!(matches!(&error, RlmSnapshotError::FormatMismatch { .. }));
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

#[test]
fn canonical_root_recognizes_quoted_global_keys_as_direct_children() {
    assert!(is_global_location(r#"root.globals["x].y"]"#));
    assert!(is_global_location("root.globals.ordinary"));
    assert!(!is_global_location(r#"root.globals["x].y"].component"#));
}

#[test]
fn old_snapshot_version_is_typed_rejection_with_cutover_remedy() {
    #[derive(Serialize)]
    struct PreviousEnvelope {
        version: u32,
        engine: &'static str,
        #[serde(with = "serde_bytes")]
        vars: Vec<u8>,
        files: BTreeMap<String, String>,
        deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
    }
    let hydration = lash_core::plugin::HydratedExecutionState {
        root: rmp_serde::to_vec_named(&PreviousEnvelope {
            version: RLM_SNAPSHOT_VERSION - 1,
            engine: "lashlang",
            vars: lashlang::Snapshot::default()
                .to_canonical_bytes()
                .expect("previous vars"),
            files: BTreeMap::new(),
            deferred_resolutions: Default::default(),
        })
        .expect("previous envelope"),
        components: BTreeMap::new(),
    };
    let mut target = RlmExecutionState::new().expect("target state");

    let error = target
        .restore_execution_state(&hydration)
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

/// Fixed-byte authority for the version-7 root encoding (ADR 0056).
///
/// Encoding both sides of a comparison with the currently linked encoder
/// cannot see the drift that matters: a dependency bump or serializer change
/// moves both sides together, and the root validator deliberately accepts
/// any declared-field order, so the same logical state could silently
/// acquire different bytes — and therefore a different component identity —
/// without a version bump. These bytes are that pin. If this test fails, the
/// persisted shape changed: decide on a version bump, then update the
/// golden, never the reverse.
#[test]
fn version_7_root_encodes_to_golden_bytes() {
    const GOLDEN: &str = concat!(
        "85a776657273696f6e07a6656e67696e65a86c6173686c616e67a7676c6f62616c7382ad696e6c696e655f736361",
        "6c617282a46b696e64a6696e6c696e65a4626f6479c43581a7676c6f62616c739182a46e616d65a576616c7565a5",
        "76616c756582a46b696e64a6737472696e67a576616c7565a5736d616c6cb06c65616665645f636f6d706f736974",
        "6582a46b696e64a46c656166a9636f6d706f6e656e74d957657865637574696f6e5f73746174652f736861323536",
        "2f656532323763393032306136386534653737316262633439346266643563313635316262366461393265363363",
        "31313235376534393435366534333864666137a566696c657381b16e6f7465732f736372617463682e747874d957",
        "657865637574696f6e5f73746174652f7368613235362f6137666631373032643137376130623466346532646131",
        "3361313262316138353735323536613864343031633238363731623833623331383063323237643838b464656665",
        "727265645f7265736f6c7574696f6e7382a86c696e6b5f6b657986aa73657373696f6e5f6964ae73657373696f6e",
        "2d676f6c64656ea77475726e5f6964a67475726e2d37aa7475726e5f696e64657803b270726f746f636f6c5f6974",
        "65726174696f6e02a96566666563745f6964a86566666563742d39aa7265706c61795f6b6579a87265706c61792d",
        "31ab7265736f6c7574696f6e7382a97765622e666574636884a46b696e64a87265736f6c766564aa646566696e69",
        "74696f6e85a26964aa746f6f6c3a6665746368a46e616d65a56665746368ab6465736372697074696f6eae466574",
        "6368206f6e652055524c2eac696e7075745f736368656d6181a963616e6f6e6963616c82aa70726f706572746965",
        "7381a375726c81a474797065a6737472696e67a474797065a66f626a656374ad6f75747075745f736368656d6181",
        "a963616e6f6e6963616c81a474797065a6737472696e67a9736f757263655f6964ac72656769737472793a776562",
        "b1657865637574696f6e5f62696e64696e6781a76163636f756e74a6616363742d31a87a2e616273656e7481a46b",
        "696e64ad6e6f745f617661696c61626c65",
    );

    let mut resolutions = BTreeMap::new();
    resolutions.insert(
        "web.fetch".to_string(),
        lash_lashlang_runtime::Resolution::Resolved(Box::new(
            lash_lashlang_runtime::ToolGrant::new(lash_core::ToolDefinition::raw(
                "tool:fetch",
                "fetch",
                "Fetch one URL.",
                serde_json::json!({"type": "object", "properties": {"url": {"type": "string"}}}),
                serde_json::json!({"type": "string"}),
            ))
            .with_source_id("registry:web")
            .with_execution_binding(serde_json::json!({"account": "acct-1"})),
        )),
    );
    resolutions.insert(
        "z.absent".to_string(),
        lash_lashlang_runtime::Resolution::NotAvailable,
    );
    let mut globals = BTreeMap::new();
    globals.insert(
        "inline_scalar".to_string(),
        PersistedGlobal::Inline {
            body: snapshot_runtime_value(&FlowValue::String("small".into())).expect("inline body"),
        },
    );
    globals.insert(
        "leafed_composite".to_string(),
        PersistedGlobal::Leaf {
            component: leaf_component_key(b"composite-body"),
        },
    );
    let root = RlmSnapshotRoot {
        version: RLM_SNAPSHOT_VERSION,
        engine: "lashlang".to_string(),
        globals,
        files: [(
            "notes/scratch.txt".to_string(),
            leaf_component_key(b"file-body"),
        )]
        .into_iter()
        .collect(),
        deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord {
            link_key: Some(lash_lashlang_runtime::DeferredResolutionLinkKey {
                session_id: "session-golden".to_string(),
                turn_id: Some("turn-7".to_string()),
                turn_index: Some(3),
                protocol_iteration: Some(2),
                effect_id: "effect-9".to_string(),
                replay_key: Some("replay-1".to_string()),
            }),
            resolutions,
        },
    };

    let encoded = rmp_serde::to_vec_named(&root).expect("encode the golden root");
    validate_canonical_root(&encoded).expect("the golden root is canonical");
    let hex = encoded
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        hex, GOLDEN,
        "the version-7 root encoding changed; decide on a version bump before updating the golden"
    );

    let decoded: RlmSnapshotRoot =
        rmp_serde::from_slice(&encoded).expect("the golden root round-trips");
    assert_eq!(decoded.version, RLM_SNAPSHOT_VERSION);
    assert_eq!(
        root_leaf_keys(&decoded),
        [
            leaf_component_key(b"composite-body"),
            leaf_component_key(b"file-body"),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
    );
}

#[test]
fn execution_root_and_raw_leaves_are_deterministic_for_scratch_file_insertion_order() {
    fn state_with_files(files: &[(&str, &str)]) -> RlmExecutionState {
        let mut state = RlmExecutionState::new().expect("state");
        for (path, contents) in files {
            state
                .write_scratch_file(path, contents.as_bytes())
                .expect("write scratch file");
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

    let left = left.snapshot_execution_state().expect("left snapshot");
    let right = right.snapshot_execution_state().expect("right snapshot");

    assert_eq!(left, right);
}

#[test]
fn cold_reopen_restores_binary_scratch_files_byte_exactly() {
    let mut source = RlmExecutionState::new().expect("source state");
    let binary = [0xff, 0xfe, 0x80, 0x00, 0x7f];
    let embedded_nul = b"prefix\0suffix";
    source
        .write_scratch_file("binary.bin", &binary)
        .expect("write non-UTF-8 scratch file");
    source
        .write_scratch_file("nested/embedded-nul.dat", embedded_nul)
        .expect("write embedded-NUL scratch file");

    let snapshot = hydrate(source.snapshot_execution_state().expect("snapshot"));
    let mut reopened = RlmExecutionState::new().expect("reopened state");
    reopened
        .restore_execution_state(&snapshot)
        .expect("restore binary scratch files");

    assert_eq!(
        std::fs::read(reopened.scratch_dir.path().join("binary.bin"))
            .expect("read restored binary file"),
        binary
    );
    assert_eq!(
        std::fs::read(reopened.scratch_dir.path().join("nested/embedded-nul.dat"))
            .expect("read restored embedded-NUL file"),
        embedded_nul
    );
}

/// A leaf-bearing hydration plus a distinct live target, so a rejected
/// restore can be checked for having changed nothing.
fn leaf_bearing_hydration_and_live_target()
-> (lash_core::plugin::HydratedExecutionState, RlmExecutionState) {
    let mut source = RlmExecutionState::new().expect("source state");
    let mut snapshot = source.rlm.snapshot();
    snapshot.globals.insert(
        "kept".to_string(),
        FlowValue::List(vec![FlowValue::String("source".repeat(2048).into())].into()),
    );
    source.rlm = FlowState::from_snapshot(snapshot);
    source.mark_execution_started();
    source
        .write_scratch_file("keep.txt", b"source-file")
        .expect("write source scratch file");
    let hydration = hydrate(source.snapshot_execution_state().expect("source snapshot"));
    assert!(
        !hydration.components.is_empty(),
        "the hydration must reference at least one leaf"
    );

    let mut live = RlmExecutionState::new().expect("live state");
    let mut snapshot = live.rlm.snapshot();
    snapshot
        .globals
        .insert("live".to_string(), FlowValue::String("untouched".into()));
    live.rlm = FlowState::from_snapshot(snapshot);
    live.mark_execution_started();
    live.write_scratch_file("live.txt", b"live-file")
        .expect("write live scratch file");
    (hydration, live)
}

fn assert_live_state_untouched(live: &RlmExecutionState, live_dir: &std::path::Path) {
    assert_eq!(
        live.rlm.snapshot().globals.get("live"),
        Some(&FlowValue::String("untouched".into())),
        "a rejected restore must not replace live globals"
    );
    assert!(
        live.rlm.snapshot().globals.get("kept").is_none(),
        "a rejected restore must not leak the source's globals"
    );
    assert_eq!(
        live.scratch_dir.path(),
        live_dir,
        "a rejected restore must not swap the live scratch directory"
    );
    assert_eq!(
        std::fs::read(live_dir.join("live.txt")).expect("read live scratch file"),
        b"live-file",
        "a rejected restore must not rewrite live scratch files"
    );
    assert!(
        !live_dir.join("keep.txt").exists(),
        "a rejected restore must not leak the source's scratch files"
    );
}

#[test]
fn restore_rejects_a_hydration_that_omits_a_referenced_leaf() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target();
    let live_dir = live.scratch_dir.path().to_path_buf();
    let dropped = hydration
        .components
        .keys()
        .next()
        .expect("a referenced leaf")
        .clone();
    let mut tampered = hydration.clone();
    tampered.components.remove(&dropped);

    let error = live
        .restore_execution_state(&tampered)
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
    assert_live_state_untouched(&live, &live_dir);
    live.restore_execution_state(&hydration)
        .expect("the untampered hydration still restores");
}

#[test]
fn restore_rejects_a_leaf_whose_body_does_not_match_its_content_address() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target();
    let live_dir = live.scratch_dir.path().to_path_buf();
    let key = hydration
        .components
        .keys()
        .next()
        .expect("a referenced leaf")
        .clone();
    let mut tampered = hydration.clone();
    tampered
        .components
        .insert(key.clone(), b"tampered body".to_vec());

    let error = live
        .restore_execution_state(&tampered)
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
    assert_live_state_untouched(&live, &live_dir);
    live.restore_execution_state(&hydration)
        .expect("the untampered hydration still restores");
}

#[test]
fn restore_rejects_a_hydration_carrying_a_leaf_the_root_does_not_reference() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target();
    let live_dir = live.scratch_dir.path().to_path_buf();
    let surplus = leaf_component_key(b"orphan");
    let mut tampered = hydration.clone();
    tampered
        .components
        .insert(surplus.clone(), b"orphan".to_vec());

    let error = live
        .restore_execution_state(&tampered)
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
    assert_live_state_untouched(&live, &live_dir);
    live.restore_execution_state(&hydration)
        .expect("the untampered hydration still restores");
}

/// `MissingLeaf` is the defence behind the exact-set check: restore compares
/// the supplied key set with the root's first, so a resolution that finds no
/// body cannot be reached through `restore_execution_state`. Pinning it here
/// keeps it a typed rejection rather than a panic if that order ever changes.
#[test]
fn resolving_an_absent_leaf_is_a_typed_missing_leaf_rejection() {
    let state = lash_core::plugin::HydratedExecutionState::default();

    let error = resolve_leaf(&state, "kept", "execution_state/sha256/absent")
        .expect_err("an absent leaf must not resolve");

    assert!(matches!(
        &error,
        RlmSnapshotError::MissingLeaf {
            logical_key,
            component,
        } if logical_key == "kept" && component == "execution_state/sha256/absent"
    ));
}

#[cfg(unix)]
#[test]
fn failed_file_collection_keeps_dirty_globals_retryable() {
    use std::os::unix::ffi::OsStringExt as _;

    let mut state = RlmExecutionState::new().expect("state");
    let mut snapshot = state.rlm.snapshot();
    snapshot.globals.insert(
        "large".to_string(),
        FlowValue::List(vec![FlowValue::String("x".repeat(8 * 1024).into())].into()),
    );
    state.rlm = FlowState::from_snapshot(snapshot);
    state.mark_execution_started();
    let invalid_path = state
        .scratch_dir
        .path()
        .join(std::ffi::OsString::from_vec(vec![0xff, 0xfe]));
    std::fs::write(&invalid_path, b"invalid path").expect("write invalid path");

    let error = state
        .snapshot_execution_state()
        .expect_err("non-UTF-8 path must fail collection");
    assert!(error.to_string().contains("not valid UTF-8"));
    std::fs::remove_file(invalid_path).expect("remove invalid path");

    let retry = state
        .snapshot_execution_state()
        .expect("retry snapshot succeeds");
    assert_eq!(
        retry
            .components
            .values()
            .filter(|component| matches!(
                component,
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
            ))
            .count(),
        1,
        "the failed capture must not poison the changed leaf as an unchanged ref"
    );
}

#[test]
fn same_size_scratch_file_rewrite_emits_a_changed_leaf() {
    let mut state = RlmExecutionState::new().expect("state");
    state
        .write_scratch_file("same-size.bin", b"aaa")
        .expect("initial file");
    let initial = state.snapshot_execution_state().expect("initial snapshot");
    assert_eq!(
        initial
            .components
            .values()
            .filter(|component| matches!(
                component,
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
            ))
            .count(),
        1
    );
    state.acknowledge_execution_state_capture();
    state
        .write_scratch_file("same-size.bin", b"bbb")
        .expect("rewrite file");
    let changed = state.snapshot_execution_state().expect("changed snapshot");
    assert_eq!(
        changed
            .components
            .values()
            .filter(|component| matches!(
                component,
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
            ))
            .count(),
        1
    );
}

#[test]
fn aborted_capture_retries_leaf_bodies_instead_of_uncommitted_refs() {
    let mut state = RlmExecutionState::new().expect("state");
    let mut snapshot = state.rlm.snapshot();
    snapshot.globals.insert(
        "large".to_string(),
        FlowValue::List(vec![FlowValue::String("x".repeat(8 * 1024).into())].into()),
    );
    state.rlm = FlowState::from_snapshot(snapshot);
    state.mark_execution_started();
    let first = state.snapshot_execution_state().expect("first capture");
    assert!(first.components.values().any(|component| matches!(
        component,
        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
    )));

    state.abort_execution_state_capture();
    let retry = state.snapshot_execution_state().expect("retry capture");
    assert!(retry.components.values().any(|component| matches!(
        component,
        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
    )));
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
