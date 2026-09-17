//! Tests for resident session state: the snapshot projection, keyed
//! checkpoint components, and their resident bodies.

use super::*;

#[test]
fn commit_operation_identity_depends_on_caller_boundary_not_head_revision() {
    let first = boundary_operation(
        &SessionId::from("session"),
        "request-42",
        "append-session-nodes",
    );
    let retry = boundary_operation(
        &SessionId::from("session"),
        "request-42",
        "append-session-nodes",
    );
    let next = boundary_operation(
        &SessionId::from("session"),
        "request-43",
        "append-session-nodes",
    );

    assert_eq!(first, retry);
    assert_ne!(first, next);
}

fn resident_leaf_body_bytes(state: &RuntimeSessionState) -> usize {
    state
        .checkpoint_components
        .entries
        .values()
        .filter_map(|component| component.opaque_body().map(|body| body.len()))
        .sum()
}

fn commit_result_for(state: &RuntimeSessionState) -> crate::store::RuntimeCommitReceipt {
    let commit = crate::RuntimeCommit::persisted_state_for_test(state, &[]);
    crate::store::RuntimeCommitReceipt {
        head_revision: state.head_revision + 1,
        checkpoint_ref: "checkpoint-ref".to_string().into(),
        manifest: commit
            .checkpoint
            .manifest()
            .expect("project the committed manifest"),
        committed_leaf_node_id: None,
        realized_node_timestamps: Vec::new(),
        committed_usage_delta_identities: Vec::new(),
        failure_evidence: Vec::new(),
        enqueued_queue_batches: Vec::new(),
        turn_input_applications: Vec::new(),
        turn_cancel_input_outcome: Default::default(),
        receipt_replayed: false,
    }
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
    let leaf_key = "execution_state/blake3/aa".to_string();
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
    assert!(
        matches!(
            state.execution_state_hydration(),
            Err(crate::StoreError::ExecutionStateBodiesReleased)
        ),
        "a released root backed by a store refuses hydration instead of reading as no execution (FIG-2521)"
    );

    // The next turn changes the same logical value: its new leaf body is
    // dirty, so a body discard must leave it alone.
    let next_leaf_key = "execution_state/blake3/bb".to_string();
    let next_leaf_body = vec![9u8; 2048];
    let mut next = crate::plugin::ExecutionStateSnapshot::from_root(Some(b"root-2".to_vec()));
    next.changed_component(next_leaf_key, next_leaf_body.clone());
    state
        .set_execution_state_components(next)
        .expect("stage the next changed leaf");
    state
        .checkpoint_components
        .discard_known_bodies(true, AcceptedExecutionRetention::DurableHead);
    assert_eq!(
        resident_leaf_body_bytes(&state),
        next_leaf_body.len(),
        "an uncommitted leaf body must survive a body discard"
    );
}

fn two_leaf_execution(root: &[u8], leaf_key: &str, leaf_body: &[u8]) -> RuntimeSessionState {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    let mut snapshot = crate::plugin::ExecutionStateSnapshot::from_root(Some(root.to_vec()));
    snapshot.changed_component(leaf_key.to_string(), leaf_body.to_vec());
    state
        .set_execution_state_components(snapshot)
        .expect("stage the execution state");
    state
}

/// A storeless commit keeps the accepted execution bodies resident: the
/// same-frame restore that follows rebuilds from them, the next commit
/// replaces them, and a frame clear drops them (FIG-2521).
#[test]
fn storeless_body_release_keeps_the_accepted_execution_for_restore() {
    let leaf_key = "execution_state/blake3/aa";
    let mut state = two_leaf_execution(b"root-1", leaf_key, b"leaf-1");
    state.set_tool_state_snapshot(Some(crate::ToolState::default()));
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert!(
        state.tool_state_snapshot().is_none(),
        "tool and plugin snapshots are released like every other committed body"
    );
    let accepted = state
        .execution_state_hydration()
        .expect("the accepted execution hydrates")
        .expect("the accepted execution stays resident");
    assert_eq!(accepted.root, b"root-1");
    assert_eq!(accepted.components.get(leaf_key), Some(&b"leaf-1".to_vec()));

    // A restore releases again; the accepted execution stays.
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert_eq!(
        state.execution_state_hydration().expect("still resident"),
        Some(accepted)
    );

    // The next commit supersedes it.
    let mut next = crate::plugin::ExecutionStateSnapshot::from_root(Some(b"root-2".to_vec()));
    next.changed_component("execution_state/blake3/bb".to_string(), b"leaf-2".to_vec());
    state
        .set_execution_state_components(next)
        .expect("stage the next commit");
    state.discard_runtime_snapshots_retaining_accepted_execution();
    let superseded = state
        .execution_state_hydration()
        .expect("resident")
        .expect("the next commit's execution is resident");
    assert_eq!(superseded.root, b"root-2");
    assert!(!superseded.components.contains_key(leaf_key));

    // A frame switch clears the execution: nothing is kept and nothing is
    // refused, because the session no longer holds a root at all.
    state.set_execution_state_snapshot(None);
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert_eq!(
        state
            .execution_state_hydration()
            .expect("no root is not corrupt"),
        None
    );
}

/// A store-backed release keeps nothing in process: hydrating the released
/// root is a typed refusal, never "no execution", while a session that never
/// held a root still hydrates to `None` (FIG-2521).
#[test]
fn released_execution_bodies_without_a_retained_snapshot_refuse_hydration() {
    let mut state = two_leaf_execution(b"root", "execution_state/blake3/aa", b"leaf");
    let result = commit_result_for(&state);
    state.apply_persisted_commit_result(result);
    assert!(
        matches!(
            state.execution_state_hydration(),
            Err(crate::StoreError::ExecutionStateBodiesReleased)
        ),
        "the released root must refuse hydration"
    );
    state.discard_runtime_snapshots();
    assert!(
        matches!(
            state.execution_state_hydration(),
            Err(crate::StoreError::ExecutionStateBodiesReleased)
        ),
        "a later store-backed release must not launder the refusal into no execution"
    );

    let mut rootless =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    rootless.discard_runtime_snapshots();
    assert_eq!(
        rootless
            .execution_state_hydration()
            .expect("a session that never held execution is not refused"),
        None
    );
}

/// Staging a restored capture over the resident set keeps every leaf the set
/// already holds — durably or as a pending body — as unchanged bookkeeping,
/// and stages only the leaves it never held; the root is always restaged
/// (FIG-2521).
#[test]
fn restoring_a_capture_keeps_held_leaves_unchanged_and_stages_missing_ones() {
    const DURABLE: &str = "execution_state/blake3/durable";
    const PENDING: &str = "execution_state/blake3/pending";
    const MISSING: &str = "execution_state/blake3/missing";
    let mut state = two_leaf_execution(b"root-1", DURABLE, b"durable body");
    let result = commit_result_for(&state);
    state.apply_persisted_commit_result(result);
    let mut pending = crate::plugin::ExecutionStateSnapshot::from_root(Some(b"root-2".to_vec()));
    pending.unchanged_component(DURABLE.to_string());
    pending.changed_component(PENDING.to_string(), b"pending body".to_vec());
    state
        .set_execution_state_components(pending)
        .expect("stage an uncommitted leaf beside the durable one");

    let restored = crate::plugin::HydratedExecutionState {
        root: b"root-3".to_vec(),
        components: [
            (DURABLE.to_string(), b"durable body".to_vec()),
            (PENDING.to_string(), b"pending body".to_vec()),
            (MISSING.to_string(), b"missing body".to_vec()),
        ]
        .into_iter()
        .collect(),
    };
    state
        .stage_restored_execution_state(restored)
        .expect("stage the restored capture");

    let checkpoint = state
        .checkpoint_components
        .build_checkpoint(crate::PersistedTurnState::default())
        .expect("build the next commit's checkpoint");
    let component = |key: &str| checkpoint.components.get(key).expect(key);
    assert!(
        matches!(
            component(DURABLE),
            crate::HydratedCheckpointComponent::Unchanged { .. }
        ),
        "a durable leaf keeps its ref: {:?}",
        component(DURABLE)
    );
    assert!(
        matches!(
            component(PENDING),
            crate::HydratedCheckpointComponent::Changed { body, .. } if body == b"pending body"
        ),
        "a pending leaf keeps its body for its first commit: {:?}",
        component(PENDING)
    );
    assert!(
        matches!(
            component(MISSING),
            crate::HydratedCheckpointComponent::Changed { body, .. } if body == b"missing body"
        ),
        "a leaf the set never held is staged with its body: {:?}",
        component(MISSING)
    );
    assert!(
        matches!(
            component(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
            crate::HydratedCheckpointComponent::Changed { body, .. } if body == b"root-3"
        ),
        "the restored root is always restaged"
    );
    assert_eq!(
        state.execution_state_snapshot(),
        Some(b"root-3".as_slice()),
        "the staged root is resident until the next commit releases it"
    );
}

#[test]
fn descriptorless_execution_state_leaves_without_a_root_remain_corrupt() {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.checkpoint_components.entries.insert(
        "execution_state/blake3/corrupt".to_string(),
        ResidentCheckpointComponent::Changed {
            descriptor: None,
            body: PendingCheckpointComponentBody::Opaque(b"orphan".to_vec()),
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
        session_id: SessionId::from("snapshot-test"),
        policy: SessionPolicy {
            provider_id: "mock".to_string(),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
        head_revision: 42,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(crate::ToolState::default()));
    state.set_plugin_state(Some(crate::PluginState::default()));
    state.set_execution_state_snapshot(Some(vec![1, 2, 3]));
    state.ensure_agent_frame_initialized();

    let value = serde_json::to_value(state.to_snapshot()).expect("serialize snapshot");

    for runtime_key in [
        "head_revision",
        "persisted_node_ids",
        "tool_state_snapshot",
        "plugin_state",
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
    assert!(hydrated.plugin_state().is_none());
    assert!(hydrated.execution_state_snapshot().is_none());
    assert!(!hydrated.agent_frames.is_empty());
}

/// FIG-3107: a read view carries the session graph, so the snapshot it projects
/// must carry the frame identity derived from that graph. Dropping it made a
/// durable frame switch invisible to every read-view consumer, and left the
/// rolling-history recovery deriving its next frame key from an empty parent.
#[test]
fn read_view_snapshot_projects_frame_identity_from_the_graph() {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("read-view-frames"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    assert!(state.current_frame_node_id.is_some());

    let projected = state
        .read_view()
        .expect("runtime frame scope resolves")
        .to_snapshot();

    assert_eq!(
        projected.current_frame_node_id, state.current_frame_node_id,
        "the read view dropped the frame the session is resident in"
    );
    assert_eq!(
        projected.agent_frames.len(),
        state.agent_frames.len(),
        "the read view dropped the session's frame records"
    );
}

#[test]
fn boxed_runtime_authority_keeps_flat_json_and_requires_tool_access() {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state
        .authority
        .tool_access
        .hide_tool("hidden")
        .expect("valid hidden name");
    state.authority.subagent = Some(crate::SubagentSessionContext {
        parent_session_id: SessionId::from("parent"),
        capability: "research".to_string(),
        depth: 1,
        max_depth: 3,
    });

    let mut value = serde_json::to_value(&state).expect("serialize runtime state");
    assert!(value.get("authority").is_none());
    assert_eq!(
        value["tool_access"]["hidden_tools"],
        serde_json::json!(["hidden"])
    );
    assert_eq!(value["subagent"]["parent_session_id"], "parent");

    let object = value.as_object_mut().expect("runtime state object");
    object.remove("tool_access");
    let error = serde_json::from_value::<RuntimeSessionState>(value)
        .expect_err("missing serialized tool authority must refuse");
    assert!(error.to_string().contains("missing field `tool_access`"));

    let current = serde_json::to_value(RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
    )))
    .expect("serialize current runtime state");
    assert_eq!(
        current.get("tool_access"),
        Some(&serde_json::json!({ "mode": "ambient" }))
    );
    assert_eq!(current.get("subagent"), Some(&serde_json::Value::Null));
}

#[test]
fn incomplete_checkpoint_component_projection_is_a_typed_error() {
    let projected =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            .to_snapshot();
    let state = RuntimeSessionState::from_snapshot(projected);

    let error = state
        .checkpoint_components
        .build_checkpoint(crate::PersistedTurnState::default())
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

#[test]
#[should_panic(expected = "adopted head revision must advance")]
fn persisted_commit_cannot_adopt_nonadvancing_revision() {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    let mut receipt = commit_result_for(&state);
    receipt.head_revision = state.head_revision;
    state.apply_persisted_commit_result(receipt);
}

#[test]
#[should_panic(expected = "adopted head revision must advance")]
fn persisted_commit_cannot_adopt_regressing_revision() {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.head_revision = 2;
    let mut receipt = commit_result_for(&state);
    receipt.head_revision = 1;
    state.apply_persisted_commit_result(receipt);
}
