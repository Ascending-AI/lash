//! Tests for resident session state: the snapshot projection, keyed
//! checkpoint components, and their resident bodies.

use super::*;
use lash_sansio::sync::MutexExt;

#[test]
fn commit_operation_identity_depends_on_caller_boundary_not_head_revision() {
    let first = boundary_operation("session", "request-42", "append-session-nodes");
    let retry = boundary_operation("session", "request-42", "append-session-nodes");
    let next = boundary_operation("session", "request-43", "append-session-nodes");

    assert_eq!(first, retry);
    assert_ne!(first, next);
}
use std::sync::{Arc, Mutex};

struct DynamicSnapshotTools {
    names: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for DynamicSnapshotTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.names
            .lock_recover()
            .iter()
            .map(|name| {
                crate::ToolDefinition::raw(
                    format!("tool:{name}"),
                    name,
                    "dynamic snapshot tool",
                    crate::ToolDefinition::default_input_schema(),
                    serde_json::json!({}),
                )
                .manifest()
            })
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.names
            .lock_recover()
            .iter()
            .any(|candidate| candidate == name)
            .then(|| {
                Arc::new(
                    crate::ToolDefinition::raw(
                        format!("tool:{name}"),
                        name,
                        "dynamic snapshot tool",
                        crate::ToolDefinition::default_input_schema(),
                        serde_json::json!({}),
                    )
                    .contract(),
                )
            })
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
        crate::ToolResult::ok(serde_json::json!("ok"))
    }
}

fn resident_leaf_body_bytes(state: &RuntimeSessionState) -> usize {
    state
        .checkpoint_components
        .entries
        .values()
        .filter_map(|component| match &component.body {
            ResidentCheckpointComponentBody::Opaque(Some(body)) => Some(body.len()),
            _ => None,
        })
        .sum()
}

fn commit_result_for(state: &RuntimeSessionState) -> crate::store::RuntimeCommitResult {
    let commit = crate::RuntimeCommit::persisted_state_for_test(state, &[]);
    crate::store::RuntimeCommitResult {
        head_revision: state.head_revision + 1,
        checkpoint_ref: "checkpoint-ref".to_string().into(),
        manifest: commit
            .checkpoint
            .manifest()
            .expect("project the committed manifest"),
        committed_leaf_node_id: None,
        realized_node_timestamps: Vec::new(),
        committed_usage_delta_identities: Vec::new(),
        enqueued_queue_batches: Vec::new(),
        turn_input_applications: Vec::new(),
        receipt_replayed: false,
    }
}

#[tokio::test]
async fn corrupt_commit_result_cannot_forge_discarded_execution_state_residency() {
    use crate::store::SessionCommitStore as _;

    const LEAF_A: &str = "execution_state/leaf-a";
    const LEAF_B: &str = "execution_state/leaf-b";

    let store = crate::InMemorySessionStore::new();
    let mut generation_a =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    let mut snapshot_a = crate::plugin::ExecutionStateSnapshot::from_root(Some(
        br#"{"generation":"a","leaves":["execution_state/leaf-a","execution_state/leaf-b"]}"#
            .to_vec(),
    ));
    snapshot_a.changed_component(LEAF_A, b"generation-a leaf-a".to_vec());
    snapshot_a.changed_component(LEAF_B, b"generation-a leaf-b".to_vec());
    generation_a
        .set_execution_state_components(snapshot_a)
        .expect("stage valid generation-A two-leaf execution state");

    let result_a = store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &generation_a,
            &[],
        ))
        .await
        .expect("persist valid generation-A state");

    let mut generation_b = generation_a.clone();
    generation_b.apply_persisted_commit_result(result_a.clone());
    let mut snapshot_b = crate::plugin::ExecutionStateSnapshot::from_root(Some(
        br#"{"generation":"b","leaves":["execution_state/leaf-a","execution_state/leaf-b"]}"#
            .to_vec(),
    ));
    snapshot_b.unchanged_component(LEAF_A);
    snapshot_b.unchanged_component(LEAF_B);
    generation_b
        .set_execution_state_components(snapshot_b)
        .expect("stage valid generation-B root over unchanged leaves");
    let result_b = store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &generation_b,
            &[],
        ))
        .await
        .expect("persist valid generation-B state");

    let generation_b_root = result_b
        .manifest
        .components
        .get(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        .expect("generation-B root descriptor")
        .clone();
    let generation_a_leaf_b = result_a
        .manifest
        .components
        .get(LEAF_B)
        .expect("generation-A leaf-b descriptor")
        .clone();

    let cases = [
        (
            "leaf ref absent from store",
            Box::new(|result: &mut crate::store::RuntimeCommitResult| {
                result
                    .manifest
                    .components
                    .get_mut(LEAF_A)
                    .expect("leaf-a descriptor")
                    .blob_ref = "execution-state-missing-leaf".to_string().into();
            }) as Box<dyn Fn(&mut crate::store::RuntimeCommitResult)>,
        ),
        (
            "leaf ref hashes different bytes",
            Box::new(move |result: &mut crate::store::RuntimeCommitResult| {
                result
                    .manifest
                    .components
                    .insert(LEAF_A.to_string(), generation_a_leaf_b.clone());
            }),
        ),
        (
            "generation-B root with generation-A leaves",
            Box::new(move |result: &mut crate::store::RuntimeCommitResult| {
                result.manifest.components.insert(
                    crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                    generation_b_root.clone(),
                );
            }),
        ),
        (
            "manifest omits leaf-b still listed by root",
            Box::new(|result: &mut crate::store::RuntimeCommitResult| {
                result.manifest.components.remove(LEAF_B);
            }),
        ),
    ];

    let mut failures = Vec::new();
    for (case, tamper) in cases {
        let mut tampered = result_a.clone();
        tamper(&mut tampered);
        let mut resident = generation_a.clone();
        resident.apply_persisted_commit_result(tampered);

        let hydration = resident.execution_state_hydration();
        if !matches!(hydration, Err(crate::StoreError::StoredDataCorrupt { .. })) {
            failures.push(format!(
                "{case}: corrupt commit-result evidence must not authorize skipped hydration; got {hydration:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn commit_result_mismatch_remains_sticky_until_execution_state_staging() {
    const LEAF_A: &str = "execution_state/leaf-a";
    const LEAF_B: &str = "execution_state/leaf-b";

    let mut resident =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    let root =
        br#"{"generation":"a","leaves":["execution_state/leaf-a","execution_state/leaf-b"]}"#
            .to_vec();
    let mut snapshot = crate::plugin::ExecutionStateSnapshot::from_root(Some(root));
    snapshot.changed_component(LEAF_A, b"generation-a leaf-a".to_vec());
    snapshot.changed_component(LEAF_B, b"generation-a leaf-b".to_vec());
    resident
        .set_execution_state_components(snapshot)
        .expect("stage valid two-leaf execution state");

    let mut tampered = commit_result_for(&resident);
    tampered.manifest.components.remove(LEAF_B);
    resident.apply_persisted_commit_result(tampered);

    let hydration_before_discard = resident.execution_state_hydration();
    assert!(
        matches!(
            hydration_before_discard,
            Err(crate::StoreError::StoredDataCorrupt { .. })
        ),
        "a mismatched commit result must refuse hydration; got {hydration_before_discard:?}"
    );
    resident.discard_runtime_snapshots();
    let hydration_after_discard = resident.execution_state_hydration();
    assert!(
        matches!(
            hydration_after_discard,
            Err(crate::StoreError::StoredDataCorrupt { .. })
        ),
        "discard_runtime_snapshots must not launder a mismatched commit result; got {hydration_after_discard:?}"
    );

    let recovered_root = br#"{"generation":"recovered","leaves":["execution_state/leaf-a","execution_state/leaf-b"]}"#
        .to_vec();
    let recovered_leaf_a = b"recovered leaf-a".to_vec();
    let recovered_leaf_b = b"recovered leaf-b".to_vec();
    let mut recovered =
        crate::plugin::ExecutionStateSnapshot::from_root(Some(recovered_root.clone()));
    recovered.changed_component(LEAF_A, recovered_leaf_a.clone());
    recovered.changed_component(LEAF_B, recovered_leaf_b.clone());
    resident
        .set_execution_state_components(recovered)
        .expect("fresh execution-state staging clears the mismatch marker");

    let hydrated = resident
        .execution_state_hydration()
        .expect("fresh execution-state staging recovers hydration")
        .expect("fresh execution-state staging restores a root");
    assert_eq!(hydrated.root, recovered_root);
    assert_eq!(hydrated.components.get(LEAF_A), Some(&recovered_leaf_a));
    assert_eq!(hydrated.components.get(LEAF_B), Some(&recovered_leaf_b));
}

#[test]
fn committing_execution_state_leaves_releases_their_resident_bodies() {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    let leaf_key = "execution_state/sha256/aa".to_string();
    let leaf_body = vec![7u8; 4096];
    let mut snapshot = crate::plugin::ExecutionStateSnapshot::from_root(Some(b"root".to_vec()));
    snapshot.changed_component(leaf_key.clone(), leaf_body.clone());
    state
        .set_execution_state_components(snapshot)
        .expect("stage the changed leaf");

    assert_eq!(
        resident_leaf_body_bytes(&state),
        leaf_body.len(),
        "an uncommitted leaf body is the next commit's only source, so it stays resident"
    );

    let result = commit_result_for(&state);
    assert!(result.manifest.components.contains_key(&leaf_key));
    state.apply_persisted_commit_result(result);

    assert_eq!(
        resident_leaf_body_bytes(&state),
        0,
        "a committed leaf body is a second resident copy of state the protocol already holds"
    );
    assert!(
        state.execution_state_ref().is_some(),
        "the committed root ref stays authoritative after its body is released"
    );
    assert!(
        state
            .checkpoint_components
            .component_ref(&leaf_key)
            .is_some(),
        "the committed leaf keeps its durable ref so the next commit can reuse it"
    );
    assert_eq!(
        state
            .execution_state_hydration()
            .expect("descriptor-backed discarded residency is not corrupt"),
        None,
        "live post-commit state leaves execution restore to the protocol's resident state"
    );

    // The next turn changes the same logical value: its new leaf body is
    // dirty, so a body discard must leave it alone.
    let next_leaf_key = "execution_state/sha256/bb".to_string();
    let next_leaf_body = vec![9u8; 2048];
    let mut next = crate::plugin::ExecutionStateSnapshot::from_root(Some(b"root-2".to_vec()));
    next.changed_component(next_leaf_key, next_leaf_body.clone());
    state
        .set_execution_state_components(next)
        .expect("stage the next changed leaf");
    state.checkpoint_components.discard_known_bodies(true);
    assert_eq!(
        resident_leaf_body_bytes(&state),
        next_leaf_body.len(),
        "an uncommitted leaf body must survive a body discard"
    );
}

#[test]
fn descriptorless_execution_state_leaves_without_a_root_remain_corrupt() {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.checkpoint_components.entries.insert(
        "execution_state/sha256/corrupt".to_string(),
        ResidentCheckpointComponent {
            descriptor: None,
            body: ResidentCheckpointComponentBody::Opaque(None),
            dirty: false,
        },
    );

    let error = state
        .execution_state_hydration()
        .expect_err("descriptorless leaves do not prove a legitimate body discard");
    assert!(matches!(
        error,
        crate::StoreError::StoredDataCorrupt { ref message, .. }
            if message == "execution-state leaves exist without a root component"
    ));
}

#[test]
fn session_snapshot_serialization_excludes_runtime_only_fields_and_round_trips() {
    let mut state = RuntimeSessionState {
        session_id: "snapshot-test".to_string(),
        policy: SessionPolicy {
            provider_id: "mock".to_string(),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
        head_revision: 42,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(crate::ToolState::default()));
    state.set_plugin_snapshot(Some(crate::PluginSessionSnapshot::default()));
    state.set_execution_state_snapshot(Some(vec![1, 2, 3]));
    state.ensure_agent_frame_initialized();

    let value = serde_json::to_value(state.to_snapshot()).expect("serialize snapshot");

    for runtime_key in [
        "head_revision",
        "persisted_node_ids",
        "tool_state_snapshot",
        "plugin_snapshot",
        "execution_state_snapshot",
    ] {
        assert!(
            value.get(runtime_key).is_none(),
            "snapshot unexpectedly exposed {runtime_key}"
        );
    }
    assert!(value.get("agent_frames").is_none());

    let snapshot: SessionSnapshot = serde_json::from_value(value).expect("round-trip snapshot");
    let hydrated = RuntimeSessionState::from_snapshot(snapshot);

    assert_eq!(hydrated.session_id, "snapshot-test");
    assert_eq!(hydrated.policy.recorded_provider_id(), "mock");
    assert_eq!(hydrated.head_revision, 0);
    assert!(hydrated.tool_state_snapshot().is_none());
    assert!(hydrated.plugin_snapshot().is_none());
    assert!(hydrated.execution_state_snapshot().is_none());
    assert!(!hydrated.agent_frames.is_empty());
}

#[test]
fn reconciled_generation_forces_next_plugin_snapshot_export() {
    let names = Arc::new(Mutex::new(vec!["dynamic_one".to_string()]));
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(DynamicSnapshotTools {
        names: Arc::clone(&names),
    });
    let plugins = crate::runtime::tests::helpers::plugin_session_with_tools("root", tools);
    let snapshot = plugins.tool_registry().export_state();
    let persisted_generation = snapshot.generation();
    let mut projected =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            .to_snapshot();
    projected.tool_state_ref = Some("persisted-tool-state".to_string().into());
    projected.tool_state_generation = Some(persisted_generation);
    let mut state = RuntimeSessionState::from_snapshot(projected);

    names.lock_recover().push("dynamic_two".to_string());
    let report = plugins
        .tool_registry()
        .restore_state(snapshot)
        .expect("live surface restore");
    assert_eq!(report.generation, persisted_generation + 1);

    state.refresh_plugin_snapshots(&plugins);
    let refreshed = state
        .tool_state_snapshot()
        .expect("generation change re-exports the tool snapshot");
    assert_eq!(refreshed.generation(), report.generation);
    assert!(refreshed.contains(&crate::ToolId::from("tool:dynamic_two")));
}

#[test]
fn incomplete_checkpoint_component_projection_is_a_typed_error() {
    let projected =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            .to_snapshot();
    let state = RuntimeSessionState::from_snapshot(projected);

    let error = state
        .checkpoint_components
        .build_checkpoint(crate::PersistedTurnState::default(), None)
        .expect_err("snapshot projection cannot prove the complete keyed set");

    assert!(matches!(
        error,
        crate::StoreError::IncompleteCheckpointComponentSet
    ));
}

#[test]
fn new_session_rejects_unproven_checkpoint_component_projection() {
    let projected =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            .to_snapshot();
    let state = RuntimeSessionState::from_snapshot(projected);

    let error = state
        .checkpoint_components
        .complete_for_new_session()
        .expect_err("a public projection cannot prove a complete new-session root");

    assert!(matches!(
        error,
        crate::StoreError::IncompleteCheckpointComponentSet
    ));
}
