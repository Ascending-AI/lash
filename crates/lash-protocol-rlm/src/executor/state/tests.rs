//! Tests for the RLM execution-state snapshot root, its keyed leaves, and
//! restore.

use super::*;
use crate::dialect::{LashlangDialect, LashlangDialectServices, RlmDialect};
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
    let mut state = RlmExecutionState::new();
    for index in 0..50 {
        state
            .rlm
            .insert_global(
                format!("page_{index}"),
                FlowValue::String(format!("page-{index}-{}", "x".repeat(100 * 1024)).into()),
            )
            .expect("seed a global");
    }
    state.mark_execution_started();
    let initial = state.snapshot_execution_state().expect("initial snapshot");
    state.acknowledge_execution_state_capture();

    state
        .rlm
        .insert_global(
            "page_0".to_string(),
            FlowValue::String(format!("changed-{}", "y".repeat(100 * 1024)).into()),
        )
        .expect("seed a global");
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

    assert_eq!(retained_bytes, 5_122_602);
    assert_eq!(changed_bytes, 117_912);
    assert_eq!(initial_leaves, 50);
    assert_eq!(changed_bodies, 1);
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
fn root_classifier_prefers_envelope_entries_over_json_field_names() {
    for location in [
        "root.globals.schema",
        "root.globals.input_schema",
        "root.globals.bindings",
    ] {
        assert!(matches!(
            root_map_order(location),
            CanonicalMapOrder::Declared(fields) if fields == PERSISTED_VALUE_FIELDS
        ));
    }
    assert!(matches!(
        root_map_order("root.deferred_resolutions.resolutions.schema"),
        CanonicalMapOrder::Declared(fields) if fields == RESOLUTION_FIELDS
    ));
    assert_eq!(
        root_map_order("root.deferred_resolutions.resolutions.tool.execution_binding.account"),
        CanonicalMapOrder::Sorted
    );
}

#[test]
fn rlm_snapshot_accepts_inline_global_named_schema() {
    let mut state = RlmExecutionState::new();
    state
        .rlm
        .insert_global("schema".to_string(), FlowValue::String("note".into()))
        .expect("seed schema global");
    state.mark_execution_started();

    let snapshot = state
        .snapshot_execution_state()
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
        deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
    }
    let hydration = lash_core::plugin::HydratedExecutionState {
        root: rmp_serde::to_vec_named(&PreviousEnvelope {
            version: RLM_SNAPSHOT_VERSION - 2,
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
    let mut target = RlmExecutionState::new();

    let error = target
        .restore_execution_state(&hydration)
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
fn previous_snapshot_version_is_typed_rejection_with_or_without_file_leaves() {
    #[derive(Serialize)]
    struct PreviousEnvelope {
        version: u32,
        engine: &'static str,
        globals: BTreeMap<String, PersistedValue>,
        files: BTreeMap<String, PersistedValue>,
        deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
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
        let mut components = BTreeMap::from([(global_component.clone(), global_body.clone())]);
        if include_file_leaf {
            components.insert(file_component, file_body);
        }
        let hydration = lash_core::plugin::HydratedExecutionState {
            root: rmp_serde::to_vec_named(&PreviousEnvelope {
                version: RLM_SNAPSHOT_VERSION - 1,
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
                deferred_resolutions: Default::default(),
            })
            .expect("encode previous root"),
            components,
        };

        let mut target = RlmExecutionState::for_engine("lashlang");
        let error = target
            .restore_execution_state(&hydration)
            .expect_err("the previous snapshot version must fail closed");

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
}

#[test]
fn version_14_root_with_files_field_is_refused_by_the_field_validator() {
    #[derive(Serialize)]
    struct UnexpectedFilesEnvelope {
        version: u32,
        engine: &'static str,
        globals: BTreeMap<String, PersistedValue>,
        files: BTreeMap<String, PersistedValue>,
        deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
    }

    let hydration = lash_core::plugin::HydratedExecutionState {
        root: rmp_serde::to_vec_named(&UnexpectedFilesEnvelope {
            version: RLM_SNAPSHOT_VERSION,
            engine: "lashlang",
            globals: BTreeMap::new(),
            files: BTreeMap::new(),
            deferred_resolutions: Default::default(),
        })
        .expect("encode v14 root with unexpected files field"),
        components: BTreeMap::new(),
    };
    let mut target = RlmExecutionState::for_engine("lashlang");

    let error = target
        .restore_execution_state(&hydration)
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
    let hydration = hydrate(source.snapshot_execution_state().expect("source snapshot"));
    let mut target = RlmExecutionState::for_engine("typescript");

    let error = target
        .restore_execution_state(&hydration)
        .expect_err("a snapshot from another dialect must be rejected");

    assert!(matches!(
        error,
        RlmSnapshotError::EngineMismatch { expected, found }
            if expected == "typescript" && found == "lashlang"
    ));
}

/// Fixed-byte authority for the version-14 root encoding (ADR 0056).
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
fn version_14_root_encodes_to_golden_bytes() {
    const GOLDEN: &str = concat!(
        "84a776657273696f6e0ea6656e67696e65a86c6173686c616e67a7676c6f62616c7382ad696e6c696e655f7363616c617282",
        "a46b696e64a6696e6c696e65a4626f6479c43e82a776657273696f6e07a7676c6f62616c739182a46e616d65a576616c7565",
        "a576616c756582a46b696e64a6737472696e67a576616c7565a5736d616c6cb06c65616665645f636f6d706f7369746582a4",
        "6b696e64a46c656166a9636f6d706f6e656e74d957657865637574696f6e5f73746174652f7368613235362f653133366335",
        "3033396230303164613166306261356265353865363866313163616136343562303938636431356334346361383034663531",
        "3764623065383662b464656665727265645f7265736f6c7574696f6e7382a86c696e6b5f6b657986aa73657373696f6e5f69",
        "64ae73657373696f6e2d676f6c64656ea77475726e5f6964a67475726e2d37aa7475726e5f696e64657803b270726f746f63",
        "6f6c5f697465726174696f6e02a96566666563745f6964a86566666563742d39aa7265706c61795f6b6579a87265706c6179",
        "2d31ab7265736f6c7574696f6e7382a97765622e666574636884a46b696e64a87265736f6c766564aa646566696e6974696f",
        "6e85a26964aa746f6f6c3a6665746368a46e616d65a56665746368ab6465736372697074696f6eae4665746368206f6e6520",
        "55524c2eac696e7075745f736368656d6181a963616e6f6e6963616c82aa70726f7065727469657381a375726c81a4747970",
        "65a6737472696e67a474797065a66f626a656374ad6f75747075745f736368656d6181a963616e6f6e6963616c81a4747970",
        "65a6737472696e67a9736f757263655f6964ac72656769737472793a776562b1657865637574696f6e5f62696e64696e6781",
        "a76163636f756e74a6616363742d31a87a2e616273656e7481a46b696e64ad6e6f745f617661696c61626c65",
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
    assert_eq!(changed_leaves.len(), 1);
    let mut globals = BTreeMap::new();
    globals.insert("inline_scalar".to_string(), inline_global);
    globals.insert("leafed_composite".to_string(), leaf_global);
    let root = RlmSnapshotRoot {
        version: RLM_SNAPSHOT_VERSION,
        engine: "lashlang".to_string(),
        globals,
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
        "the version-14 root encoding changed; decide on a version bump before updating the golden"
    );

    let decoded: RlmSnapshotRoot =
        rmp_serde::from_slice(&encoded).expect("the golden root round-trips");
    assert_eq!(decoded.version, RLM_SNAPSHOT_VERSION);
    assert_eq!(
        root_leaf_keys(&decoded),
        [
            "execution_state/sha256/e136c5039b001da1f0ba5be58e68f11caa645b098cd15c44ca804f517db0e86b"
                .to_string(),
        ]
        .into_iter()
        .collect()
    );
}

/// A leaf-bearing hydration plus a distinct live target, so a rejected
/// restore can be checked for having changed nothing.
fn leaf_bearing_hydration_and_live_target()
-> (lash_core::plugin::HydratedExecutionState, RlmExecutionState) {
    let mut source = RlmExecutionState::new();
    source
        .rlm
        .insert_global(
            "kept".to_string(),
            FlowValue::List(vec![FlowValue::String("source".repeat(2048).into())].into()),
        )
        .expect("seed a global");
    source.mark_execution_started();
    let hydration = hydrate(source.snapshot_execution_state().expect("source snapshot"));
    assert!(
        !hydration.components.is_empty(),
        "the hydration must reference at least one leaf"
    );

    let mut live = RlmExecutionState::new();
    live.rlm
        .insert_global("live".to_string(), FlowValue::String("untouched".into()))
        .expect("seed a global");
    live.mark_execution_started();
    (hydration, live)
}

fn assert_live_state_untouched(live: &RlmExecutionState) {
    assert_eq!(
        live.rlm.snapshot().globals().get("live"),
        Some(&FlowValue::String("untouched".into())),
        "a rejected restore must not replace live globals"
    );
    assert!(
        live.rlm.snapshot().globals().get("kept").is_none(),
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
    assert_live_state_untouched(&live);
    live.restore_execution_state(&hydration)
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
    assert_live_state_untouched(&live);
    live.restore_execution_state(&hydration)
        .expect("the untampered hydration still restores");
}

#[test]
fn restore_rejects_a_hydration_carrying_a_leaf_the_root_does_not_reference() {
    let (hydration, mut live) = leaf_bearing_hydration_and_live_target();
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
    assert_live_state_untouched(&live);
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

#[test]
fn aborted_capture_retries_leaf_bodies_instead_of_uncommitted_refs() {
    let mut state = RlmExecutionState::new();
    state
        .rlm
        .insert_global(
            "large".to_string(),
            FlowValue::List(vec![FlowValue::String("x".repeat(8 * 1024).into())].into()),
        )
        .expect("seed a global");
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
        .rlm
        .insert_global(
            "projected".to_string(),
            FlowValue::Projected(ProjectedValue::scalar(
                "projected",
                FlowValue::String("host".into()),
            )),
        )
        .expect("seed a global");
    state
        .rlm
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
        .rlm
        .insert_global("doc".to_string(), FlowValue::Record(Arc::new(record)))
        .expect("seed a global");
    state
        .rlm
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
    let mut state = RlmExecutionState::new();
    state
        .rlm
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
fn lashlang_dialect_pins_snapshot_engine_id() {
    let dialect = LashlangDialect::new(
        lash_lashlang_runtime::LashlangSurface::default(),
        LashlangDialectServices {
            projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
            artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
            deferred_tool_resolver: None,
            execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
            execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        },
    );

    assert_eq!(dialect.snapshot_engine_id(), "lashlang");

    let mut session = dialect.create_session().expect("create Lashlang session");
    let snapshot = session
        .snapshot_execution_state()
        .expect("snapshot Lashlang session");
    let root: RlmSnapshotRoot = rmp_serde::from_slice(
        snapshot
            .root
            .as_deref()
            .expect("fresh Lashlang snapshot has a root"),
    )
    .expect("decode Lashlang snapshot root");
    assert_eq!(root.engine, "lashlang");
}
