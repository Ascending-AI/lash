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
fn large_scalar_edit_commits_changed_state_not_retained_session() {
    let mut state = RlmExecutionState::new().expect("state");
    let mut runtime = state.rlm.snapshot();
    for index in 0..50 {
        runtime.globals.insert(
            format!("page_{index}"),
            FlowValue::String(format!("page-{index}-{}", "x".repeat(100 * 1024)).into()),
        );
    }
    state.rlm = FlowState::from_snapshot(runtime);
    state.mark_execution_started();
    let initial = state.snapshot_execution_state().expect("initial snapshot");
    state.acknowledge_execution_state_capture();

    let mut runtime = state.rlm.snapshot();
    runtime.globals.insert(
        "page_0".to_string(),
        FlowValue::String(format!("changed-{}", "y".repeat(100 * 1024)).into()),
    );
    state.rlm = FlowState::from_snapshot(runtime);
    state.dirty_globals.insert("page_0".to_string());
    state.root_dirty = true;
    let changed = state.snapshot_execution_state().expect("changed snapshot");
    let retained_bytes = state
        .rlm
        .snapshot()
        .to_canonical_bytes()
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

    assert_eq!(retained_bytes, 5_122_593);
    assert_eq!(changed_bytes, 117_910);
    assert_eq!(initial_leaves, 50);
    assert_eq!(changed_bodies, 1);
}

#[test]
fn tiny_files_inline_and_avoid_per_file_leaf_overhead() {
    let mut state = RlmExecutionState::new().expect("state");
    for index in 0..500 {
        let body = format!("file-{index:03}-{}", "x".repeat(41));
        assert_eq!(body.len(), 50);
        state
            .write_scratch_file_for_testing(&format!("scratch-{index:03}.txt"), body.as_bytes())
            .expect("write tiny scratch file");
    }
    let mut runtime = state.rlm.snapshot();
    runtime
        .globals
        .insert("revision".to_string(), FlowValue::Number(1.0));
    state.rlm = FlowState::from_snapshot(runtime);
    state.mark_execution_started();
    let initial = state.snapshot_execution_state().expect("initial snapshot");
    state.acknowledge_execution_state_capture();

    let mut runtime = state.rlm.snapshot();
    runtime
        .globals
        .insert("revision".to_string(), FlowValue::Number(2.0));
    state.rlm = FlowState::from_snapshot(runtime);
    state.dirty_globals.insert("revision".to_string());
    state.root_dirty = true;
    let changed = state.snapshot_execution_state().expect("changed snapshot");
    let changed_bytes = measure_snapshot(&changed).checkpoint_bytes;
    println!(
        "FIG1257_TINY_FILES data_bytes={} changed_commit_bytes={changed_bytes} initial_leaves={} retained_leaf_refs={}",
        500 * 50,
        initial.components.len(),
        changed.components.len()
    );

    assert!(
        initial.components.is_empty(),
        "sub-threshold files must inline instead of minting {} leaves",
        initial.components.len()
    );
    assert!(
        changed.components.is_empty(),
        "an unrelated edit must not carry per-file leaf manifest rows"
    );
    assert_eq!(changed_bytes, 43_493);
}

fn retained_file_commit_bytes(body_len: usize, leaf: bool) -> usize {
    let body = vec![b'x'; body_len];
    let persisted = if leaf {
        PersistedValue::Leaf {
            component: leaf_component_key(&body),
        }
    } else {
        PersistedValue::Inline { body: body.clone() }
    };
    let root = RlmSnapshotRoot {
        version: RLM_SNAPSHOT_VERSION,
        engine: "lashlang".to_string(),
        globals: BTreeMap::new(),
        files: [("scratch.bin".to_string(), persisted)]
            .into_iter()
            .collect(),
        deferred_resolutions: Default::default(),
    };
    let encoded = rmp_serde::to_vec_named(&root).expect("encode measured root");
    validate_canonical_root(&encoded).expect("measured root is canonical");
    let mut snapshot = lash_core::plugin::ExecutionStateSnapshot::from_root(Some(encoded));
    if leaf {
        snapshot.unchanged_component(leaf_component_key(&body));
    }
    measure_snapshot(&snapshot).checkpoint_bytes
}

#[test]
fn measured_file_break_even_stays_near_the_profile_line_basis() {
    let empty_bytes = retained_file_commit_bytes(0, false);
    let break_even = (0..=1024)
        .find(|body_len| {
            retained_file_commit_bytes(*body_len, false)
                >= retained_file_commit_bytes(*body_len, true)
        })
        .expect("inline and leaf layouts must cross");
    let inline_bytes = retained_file_commit_bytes(break_even, false);
    let leaf_bytes = retained_file_commit_bytes(break_even, true);
    let leaf_fixed_overhead = leaf_bytes - empty_bytes;
    println!(
        "FIG1257_FILE_BREAK_EVEN body_bytes={break_even} inline_commit_bytes={inline_bytes} leaf_commit_bytes={leaf_bytes} leaf_fixed_overhead_bytes={leaf_fixed_overhead}"
    );

    assert_eq!(break_even, 272);
    assert_eq!(inline_bytes, 711);
    assert_eq!(leaf_bytes, 711);
    assert_eq!(leaf_fixed_overhead, 273);
    assert_eq!(lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES, 512);
}

fn canonical_string_global_body(body_len: usize) -> Vec<u8> {
    for string_len in 0..=body_len {
        let body = snapshot_runtime_value(&FlowValue::String("x".repeat(string_len).into()))
            .expect("canonical string global body");
        if body.len() == body_len {
            return body;
        }
    }
    panic!("no canonical string global body has length {body_len}");
}

#[test]
fn size_line_selects_literal_global_and_file_boundaries() {
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

    let mut changed_leaves = BTreeMap::new();
    assert!(matches!(
        persist_value_body(vec![0xff; 511], &prior_leaf_keys, &mut changed_leaves),
        PersistedValue::Inline { .. }
    ));
    assert_eq!(changed_leaves.len(), 0);

    let mut changed_leaves = BTreeMap::new();
    assert!(matches!(
        persist_value_body(vec![0xff; 512], &prior_leaf_keys, &mut changed_leaves),
        PersistedValue::Leaf { .. }
    ));
    assert_eq!(changed_leaves.len(), 1);

    let mut changed_leaves = BTreeMap::new();
    assert!(matches!(
        persist_value_body(vec![0xff; 513], &prior_leaf_keys, &mut changed_leaves),
        PersistedValue::Leaf { .. }
    ));
    assert_eq!(changed_leaves.len(), 1);
}

fn measured_file_edit_commits(initial_len: usize, edit_lengths: &[usize]) -> Vec<usize> {
    let mut state = RlmExecutionState::new().expect("state");
    state
        .write_scratch_file_for_testing("straddle.bin", &vec![0; initial_len])
        .expect("initial file");
    let _ = state.snapshot_execution_state().expect("initial snapshot");
    state.acknowledge_execution_state_capture();

    edit_lengths
        .iter()
        .enumerate()
        .map(|(turn, body_len)| {
            state
                .write_scratch_file_for_testing("straddle.bin", &vec![(turn + 1) as u8; *body_len])
                .expect("edit file");
            let snapshot = state.snapshot_execution_state().expect("edit snapshot");
            let bytes = measure_snapshot(&snapshot).checkpoint_bytes;
            state.acknowledge_execution_state_capture();
            bytes
        })
        .collect()
}

#[test]
fn threshold_straddling_has_no_material_churn_premium() {
    let line = lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES;
    let crossing = measured_file_edit_commits(line - 1, &[line, line - 1, line, line - 1]);
    let stays_inline =
        measured_file_edit_commits(line - 2, &[line - 1, line - 2, line - 1, line - 2]);
    let stays_leaf = measured_file_edit_commits(line, &[line + 1, line, line + 1, line]);
    println!(
        "FIG1257_THRESHOLD_STRADDLING line={line} crossing={crossing:?} stays_inline={stays_inline:?} stays_leaf={stays_leaf:?}"
    );

    assert_eq!(line, 512);
    assert_eq!(crossing, vec![1_224, 951, 1_224, 951]);
    assert_eq!(stays_inline, vec![951, 950, 951, 950]);
    assert_eq!(stays_leaf, vec![1_225, 1_224, 1_225, 1_224]);
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
fn canonical_root_recognizes_quoted_file_paths_as_direct_children() {
    assert!(is_file_location(r#"root.files["x.y/bin"]"#));
    assert!(is_file_location("root.files.ordinary"));
    assert!(!is_file_location(r#"root.files["x.y/bin"].component"#));
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

#[test]
fn restore_validates_the_snapshot_engine_against_the_active_dialect() {
    let mut source = RlmExecutionState::for_engine("lashlang").expect("source state");
    let hydration = hydrate(source.snapshot_execution_state().expect("source snapshot"));
    let mut target = RlmExecutionState::for_engine("typescript").expect("target state");

    let error = target
        .restore_execution_state(&hydration)
        .expect_err("a snapshot from another dialect must be rejected");

    assert!(matches!(
        error,
        RlmSnapshotError::EngineMismatch { expected, found }
            if expected == "typescript" && found == "lashlang"
    ));
}

/// Fixed-byte authority for the version-8 root encoding (ADR 0056).
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
fn version_8_root_encodes_to_golden_bytes() {
    const GOLDEN: &str = concat!(
        "85a776657273696f6e08a6656e67696e65a86c6173686c616e67a7676c6f62616c7382ad696e6c696e655f736361",
        "6c617282a46b696e64a6696e6c696e65a4626f6479c43581a7676c6f62616c739182a46e616d65a576616c7565a5",
        "76616c756582a46b696e64a6737472696e67a576616c7565a5736d616c6cb06c65616665645f636f6d706f736974",
        "6582a46b696e64a46c656166a9636f6d706f6e656e74d957657865637574696f6e5f73746174652f736861323536",
        "2f3039333562656436626133363463663565333435613136326365386539303162323838643935396264623730306466",
        "3331386363363164633136376331326331a566696c657382b06e6f7465732f696e6c696e652e62696e82a46b",
        "696e64a6696e6c696e65a4626f6479c402ff00af6e6f7465732f6c617267652e62696e82a46b696e64a46c656166",
        "a9636f6d706f6e656e74d957657865637574696f6e5f73746174652f7368613235362f326561313639383863613961",
        "3362393733666631313639336536646534626430373837373536353563643637313563356130366131323066373162",
        "3365383237b464656665",
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
    let prior_leaf_keys = BTreeSet::new();
    let mut changed_leaves = BTreeMap::new();
    let inline_global = persist_value_body(
        snapshot_runtime_value(&FlowValue::String("small".into())).expect("inline body"),
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
    let inline_file = persist_value_body(vec![0xff, 0x00], &prior_leaf_keys, &mut changed_leaves);
    assert!(matches!(inline_file, PersistedValue::Inline { .. }));
    let leaf_file = persist_value_body(vec![0xa5; 512], &prior_leaf_keys, &mut changed_leaves);
    assert!(matches!(leaf_file, PersistedValue::Leaf { .. }));
    assert_eq!(changed_leaves.len(), 2);
    let mut globals = BTreeMap::new();
    globals.insert("inline_scalar".to_string(), inline_global);
    globals.insert("leafed_composite".to_string(), leaf_global);
    let root = RlmSnapshotRoot {
        version: RLM_SNAPSHOT_VERSION,
        engine: "lashlang".to_string(),
        globals,
        files: [
            ("notes/inline.bin".to_string(), inline_file),
            ("notes/large.bin".to_string(), leaf_file),
        ]
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
        "the version-8 root encoding changed; decide on a version bump before updating the golden"
    );

    let decoded: RlmSnapshotRoot =
        rmp_serde::from_slice(&encoded).expect("the golden root round-trips");
    assert_eq!(decoded.version, RLM_SNAPSHOT_VERSION);
    assert_eq!(
        root_leaf_keys(&decoded),
        [
            "execution_state/sha256/0935bed6ba364cf5e345a162ce8e901b288d959bdb700df318cc61dc167c12c1"
                .to_string(),
            "execution_state/sha256/2ea16988ca9a3b973ff11693e6de4bd078775655cd6715c5a06a120f71b3e827"
                .to_string(),
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn execution_root_and_raw_leaves_are_deterministic_for_scratch_file_insertion_order() {
    fn state_with_files(files: &[(&str, &str)]) -> RlmExecutionState {
        let mut state = RlmExecutionState::new().expect("state");
        for (path, contents) in files {
            state
                .write_scratch_file_for_testing(path, contents.as_bytes())
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
fn in_memory_reopen_restores_inline_and_leaf_binary_files_byte_exactly() {
    let mut source = RlmExecutionState::new().expect("source state");
    source
        .write_scratch_file_for_testing("inline-invalid-utf8.bin", &[0xff, 0xfe, 0x80, 0x00, 0x7f])
        .expect("write inline non-UTF-8 scratch file");
    source
        .write_scratch_file_for_testing("leaf-invalid-utf8.bin", &vec![0xff; 513])
        .expect("write leaf non-UTF-8 scratch file");

    let snapshot = source.snapshot_execution_state().expect("snapshot");
    let root: RlmSnapshotRoot =
        rmp_serde::from_slice(snapshot.root.as_deref().expect("snapshot root"))
            .expect("decode snapshot root");
    assert!(matches!(
        root.files.get("inline-invalid-utf8.bin"),
        Some(PersistedValue::Inline { .. })
    ));
    assert!(matches!(
        root.files.get("leaf-invalid-utf8.bin"),
        Some(PersistedValue::Leaf { .. })
    ));
    assert_eq!(snapshot.components.len(), 1);
    let snapshot = hydrate(snapshot);
    let mut reopened = RlmExecutionState::new().expect("reopened state");
    reopened
        .restore_execution_state(&snapshot)
        .expect("restore binary scratch files");

    assert_eq!(
        std::fs::read(reopened.scratch_dir.path().join("inline-invalid-utf8.bin"))
            .expect("read restored inline binary file"),
        vec![0xff, 0xfe, 0x80, 0x00, 0x7f]
    );
    let leaf = std::fs::read(reopened.scratch_dir.path().join("leaf-invalid-utf8.bin"))
        .expect("read restored leaf binary file");
    assert_eq!(leaf.len(), 513);
    assert_eq!(
        format!("{:x}", Sha256::digest(&leaf)),
        "ea032debaa72c17dae01588597abe1bf263f08612fe41bd4a599e6b3480f0bec"
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
        .write_scratch_file_for_testing("keep.txt", b"source-file")
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
    live.write_scratch_file_for_testing("live.txt", b"live-file")
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
    let body_len = lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES;
    state
        .write_scratch_file_for_testing("same-size.bin", &vec![b'a'; body_len])
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
        .write_scratch_file_for_testing("same-size.bin", &vec![b'b'; body_len])
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
