//! Tests for resident session state: the snapshot projection, keyed
//! checkpoint components, and their resident bodies.

use super::*;

/// `key`'s binding as a registry mints it: its own wire model and a 32k
/// window.
fn recorded_llm_profile(key: &str) -> crate::LlmProfileConfig {
    crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
        crate::LlmProfileKey::new(key),
        lash_core_llm::llm_profile::LlmProfileMetadata::builder(format!("{key}-wire"))
            .context_window_tokens(32_000)
            .build()
            .expect("model"),
    ))
}

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
    let commit = crate::RuntimeCommit::persisted_state_for_test(state);
    crate::store::RuntimeCommitReceipt {
        schema_version: crate::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
        head_revision: state.head_revision + 1,
        checkpoint_ref: "checkpoint-ref".to_string().into(),
        manifest: commit
            .checkpoint
            .manifest(crate::store::FleetFormat::current())
            .expect("project the committed manifest"),
        committed_leaf_node_id: None,
        realized_node_timestamps: Vec::new(),
        failure_evidence: Vec::new(),
        outcome: None,
        pending_follow_on: None,
        command_outcomes: Default::default(),
        turn_input_applications: Vec::new(),
        turn_cancel_input_outcome: Default::default(),
        receipt_replayed: false,
    }
}

#[test]
fn commit_result_mismatch_remains_sticky_until_execution_state_staging() {
    const LEAF_A: &str = "execution_state/leaf-a";
    const LEAF_B: &str = "execution_state/leaf-b";

    let mut resident = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    let root =
        br#"{"generation":"a","leaves":["execution_state/leaf-a","execution_state/leaf-b"]}"#
            .to_vec();
    let mut snapshot = crate::plugin::ExecutionStateCapture::replace(root.into());
    snapshot.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(LEAF_A).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((b"generation-a leaf-a".to_vec()).into()),
    );
    snapshot.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(LEAF_B).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((b"generation-a leaf-b".to_vec()).into()),
    );
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
        crate::plugin::ExecutionStateCapture::replace(recovered_root.clone().into());
    recovered.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(LEAF_A).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((recovered_leaf_a.clone()).into()),
    );
    recovered.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(LEAF_B).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((recovered_leaf_b.clone()).into()),
    );
    resident
        .set_execution_state_components(recovered)
        .expect("fresh execution-state staging clears the mismatch marker");

    let hydrated = resident
        .execution_state_hydration()
        .expect("fresh execution-state staging recovers hydration")
        .expect("fresh execution-state staging restores a run");
    assert_eq!(&*hydrated.root, &recovered_root[..]);
    assert_eq!(
        hydrated.components.get(LEAF_A).map(|b| &b[..]),
        Some(&recovered_leaf_a[..])
    );
    assert_eq!(
        hydrated.components.get(LEAF_B).map(|b| &b[..]),
        Some(&recovered_leaf_b[..])
    );
}

#[test]
fn committing_execution_state_leaves_releases_their_resident_bodies() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    let leaf_key = "execution_state/blake3/aa".to_string();
    let leaf_body = vec![7u8; 4096];
    let mut snapshot = crate::plugin::ExecutionStateCapture::replace(b"root".to_vec().into());
    snapshot.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(&leaf_key).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((leaf_body.clone()).into()),
    );
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
        "a released run backed by a store refuses hydration instead of reading as no execution (FIG-2521)"
    );

    // The next turn changes the same logical value: its new leaf body is
    // dirty, so a body discard must leave it alone.
    let next_leaf_key = "execution_state/blake3/bb".to_string();
    let next_leaf_body = vec![9u8; 2048];
    let mut next = crate::plugin::ExecutionStateCapture::replace(b"root-2".to_vec().into());
    next.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(&next_leaf_key).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((next_leaf_body.clone()).into()),
    );
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
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    let mut snapshot = crate::plugin::ExecutionStateCapture::replace(root.to_vec().into());
    snapshot.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(leaf_key).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((leaf_body.to_vec()).into()),
    );
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
    assert_eq!(&*accepted.root, b"root-1");
    assert_eq!(
        accepted.components.get(leaf_key).map(|b| &b[..]),
        Some(&b"leaf-1"[..])
    );

    // A restore releases again; the accepted execution stays.
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert_eq!(
        state.execution_state_hydration().expect("still resident"),
        Some(accepted)
    );

    // The next commit supersedes it.
    let mut next = crate::plugin::ExecutionStateCapture::replace(b"root-2".to_vec().into());
    next.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse("execution_state/blake3/bb")
            .expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((b"leaf-2".to_vec()).into()),
    );
    state
        .set_execution_state_components(next)
        .expect("stage the next commit");
    state.discard_runtime_snapshots_retaining_accepted_execution();
    let superseded = state
        .execution_state_hydration()
        .expect("resident")
        .expect("the next commit's execution is resident");
    assert_eq!(&*superseded.root, b"root-2");
    assert!(!superseded.components.contains_key(leaf_key));

    // A frame switch clears the execution: nothing is kept and nothing is
    // refused, because the session no longer holds a run at all.
    state.set_execution_state_snapshot(None);
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert_eq!(
        state
            .execution_state_hydration()
            .expect("no run is not corrupt"),
        None
    );
}

/// A store-backed release keeps nothing in process: hydrating the released
/// run is a typed refusal, never "no execution", while a session that never
/// held a run still hydrates to `None` (FIG-2521).
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
        "the released run must refuse hydration"
    );
    state.discard_runtime_snapshots();
    assert!(
        matches!(
            state.execution_state_hydration(),
            Err(crate::StoreError::ExecutionStateBodiesReleased)
        ),
        "a later store-backed release must not launder the refusal into no execution"
    );

    let mut rootless = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
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
/// and stages only the leaves it never held; the run is always restaged
/// (FIG-2521).
#[test]
fn restoring_a_capture_keeps_held_leaves_unchanged_and_stages_missing_ones() {
    const DURABLE: &str = "execution_state/blake3/durable";
    const PENDING: &str = "execution_state/blake3/pending";
    const MISSING: &str = "execution_state/blake3/missing";
    let mut state = two_leaf_execution(b"root-1", DURABLE, b"durable body");
    let result = commit_result_for(&state);
    state.apply_persisted_commit_result(result);
    let mut pending = crate::plugin::ExecutionStateCapture::replace(b"root-2".to_vec().into());
    pending.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(DURABLE).expect("execution leaf key"),
        crate::plugin::LeafChange::Unchanged,
    );
    pending.leaves_mut().expect("replacement capture").insert(
        crate::plugin::ExecutionLeafName::parse(PENDING).expect("execution leaf key"),
        crate::plugin::LeafChange::Changed((b"pending body".to_vec()).into()),
    );
    state
        .set_execution_state_components(pending)
        .expect("stage an uncommitted leaf beside the durable one");

    let restored = crate::plugin::HydratedExecutionState {
        root: b"root-3".to_vec().into(),
        components: [
            (
                crate::plugin::ExecutionLeafName::parse(DURABLE).expect("durable leaf"),
                b"durable body".to_vec().into(),
            ),
            (
                crate::plugin::ExecutionLeafName::parse(PENDING).expect("pending leaf"),
                b"pending body".to_vec().into(),
            ),
            (
                crate::plugin::ExecutionLeafName::parse(MISSING).expect("missing leaf"),
                b"missing body".to_vec().into(),
            ),
        ]
        .into_iter()
        .collect(),
    };
    state
        .stage_restored_execution_state(restored)
        .expect("stage the restored capture");

    let checkpoint = state
        .checkpoint_components
        .build_checkpoint(
            crate::PersistedTurnState::default(),
            crate::store::FleetFormat::current(),
        )
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
            crate::HydratedCheckpointComponent::Changed { body, .. } if &**body == b"pending body"
        ),
        "a pending leaf keeps its body for its first commit: {:?}",
        component(PENDING)
    );
    assert!(
        matches!(
            component(MISSING),
            crate::HydratedCheckpointComponent::Changed { body, .. } if &**body == b"missing body"
        ),
        "a leaf the set never held is staged with its body: {:?}",
        component(MISSING)
    );
    assert!(
        matches!(
            component(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
            crate::HydratedCheckpointComponent::Changed { body, .. } if &**body == b"root-3"
        ),
        "the restored root is always restaged"
    );
    assert_eq!(
        state.execution_state_snapshot(),
        Some(std::sync::Arc::from(&b"root-3"[..])),
        "the staged root is resident until the next commit releases it"
    );
}

#[test]
fn descriptorless_execution_state_leaves_without_a_root_remain_corrupt() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.checkpoint_components.entries.insert(
        "execution_state/blake3/corrupt".to_string(),
        ResidentCheckpointComponent::Changed {
            descriptor: None,
            body: PendingCheckpointComponentBody::Opaque(b"orphan".to_vec().into()),
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
            model: Some(recorded_llm_profile("mock")),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024))
        },
        head_revision: 42,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.set_tool_state_snapshot(Some(crate::ToolState::default()));
    state.set_plugin_state(Some(crate::PluginState::default()));
    state.set_execution_state_snapshot(Some(vec![1, 2, 3].into()));
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
    assert_eq!(hydrated.policy.model, Some(recorded_llm_profile("mock")));
    assert_eq!(hydrated.head_revision, 0);
    assert!(hydrated.tool_state_snapshot().is_none());
    assert!(hydrated.plugin_state().is_none());
    assert!(hydrated.execution_state_snapshot().is_none());
    assert!(!hydrated.agent_frames.is_empty());
}

/// FIG-3107: a read view carries the session graph, so the snapshot it projects
/// must carry the frame identity derived from that graph. Dropping it made a
/// durable frame switch invisible to every read-view consumer, and left the
/// standard-compaction recovery deriving its next frame key from an empty parent.
#[test]
fn read_view_snapshot_projects_frame_identity_from_the_graph() {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("read-view-frames"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    assert!(state.current_frame_node_id.is_some());

    let projected = state.read_view().to_snapshot();

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
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state
        .authority
        .tool_access
        .hide_tool("hidden")
        .expect("valid hidden name");
    state.authority.subagent = Some(crate::SubagentSessionContext {
        capability: "research".to_string(),
        depth: 1,
    });

    let mut value = serde_json::to_value(&state).expect("serialize runtime state");
    assert!(value.get("authority").is_none());
    assert_eq!(
        value["tool_access"]["hidden_tools"],
        serde_json::json!(["hidden"])
    );
    assert_eq!(
        value["subagent"],
        serde_json::json!({"capability": "research", "depth": 1})
    );

    let object = value.as_object_mut().expect("runtime state object");
    object.remove("tool_access");
    let error = serde_json::from_value::<RuntimeSessionState>(value)
        .expect_err("missing serialized tool authority must refuse");
    assert!(error.to_string().contains("missing field `tool_access`"));

    let current = serde_json::to_value(RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
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
    let projected = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .to_snapshot();
    let state = RuntimeSessionState::from_snapshot(projected);

    let error = state
        .checkpoint_components
        .build_checkpoint(
            crate::PersistedTurnState::default(),
            crate::store::FleetFormat::current(),
        )
        .expect_err("snapshot projection cannot prove the complete keyed set");

    assert!(matches!(
        error,
        crate::StoreError::IncompleteCheckpointComponentSet
    ));
}

#[test]
fn new_session_rejects_unproven_checkpoint_component_projection() {
    let projected = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
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
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    let mut receipt = commit_result_for(&state);
    receipt.head_revision = state.head_revision;
    state.apply_persisted_commit_result(receipt);
}

#[test]
#[should_panic(expected = "adopted head revision must advance")]
fn persisted_commit_cannot_adopt_regressing_revision() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.head_revision = 2;
    let mut receipt = commit_result_for(&state);
    receipt.head_revision = 1;
    state.apply_persisted_commit_result(receipt);
}

fn fresh_state_with_initial_frame() -> RuntimeSessionState {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.ensure_agent_frame_initialized();
    state
}

fn initial_frame_protocol_turn_options(state: &RuntimeSessionState) -> crate::ProtocolTurnOptions {
    state
        .current_agent_frame()
        .expect("the initial frame is open")
        .protocol_turn_options()
}

/// The initial frame's committed payload, without its wall-clock timestamp.
fn initial_frame_payload(state: &RuntimeSessionState) -> (crate::NodeId, serde_json::Value) {
    let [node] = state.session_graph.nodes.as_slice() else {
        panic!("the state holds only its initial frame");
    };
    (
        node.node_id.clone(),
        serde_json::to_value(&node.payload).expect("encode the frame payload"),
    )
}

/// A plugin configuration whose protocol namespace is `payload`.
fn protocol_config(payload: serde_json::Value) -> crate::PluginConfig {
    let mut config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
    config.insert("protocol", payload);
    config
}

/// A fresh session's eagerly opened initial frame is re-stamped under the
/// configuration the state installs before it persists, and a reopen of its
/// durable head opens it under that configuration: both commit the same
/// frame (FIG-3684, FIG-4379).
#[test]
fn an_unpersisted_initial_frame_opens_under_the_installed_plugin_config() {
    let mut fresh = fresh_state_with_initial_frame();
    assert_eq!(
        initial_frame_protocol_turn_options(&fresh),
        crate::ProtocolTurnOptions::default()
    );
    let settled = protocol_config(serde_json::json!({ "channel": "cell" }));
    fresh.authority.plugin_config = settled.clone();
    fresh.open_unpersisted_initial_frame_under_current_assignment();
    assert_eq!(
        initial_frame_protocol_turn_options(&fresh),
        settled.protocol_turn_options()
    );

    let mut reopened = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    reopened.authority.plugin_config = settled;
    reopened.ensure_agent_frame_initialized();
    assert_eq!(
        initial_frame_payload(&fresh),
        initial_frame_payload(&reopened),
        "a fresh session and a reopen of its head commit the same initial frame"
    );
}

/// A persisted frame is a historical snapshot: a later configuration never
/// rewrites it.
#[test]
fn a_persisted_initial_frame_keeps_the_config_it_opened_under() {
    let mut state = fresh_state_with_initial_frame();
    let opened_under = initial_frame_protocol_turn_options(&state);
    let persisted = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    state.mark_node_ids_persisted(persisted);
    state.authority.plugin_config = protocol_config(serde_json::json!({ "channel": "cell" }));
    state.open_unpersisted_initial_frame_under_current_assignment();
    assert_eq!(initial_frame_protocol_turn_options(&state), opened_under);
}

/// The head config is the revision's durable home: it round-trips through
/// `persisted_session_config_from_state`, and adopting a durable head
/// restores it.
#[test]
fn config_revision_round_trips_through_the_persisted_head_config() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.config_revision = 5;

    let config = crate::store::persisted_session_config_from_state(&state);
    assert_eq!(config.config_revision, 5);

    let mut restored = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    apply_persisted_session_config(&mut restored, &config);
    assert_eq!(restored.config_revision, 5);
    assert_eq!(restored.policy.model, config.model);
}

/// Install a run view that runs under `config`, resolved against the
/// state's sticky config.
fn install_view(state: &mut RuntimeSessionState, config: &crate::PersistedSessionConfig) {
    let mut run = crate::run_spec::ResolvedRun::snapshot(
        crate::store::persisted_session_config_from_state(state),
        crate::run_spec::TerminationPolicy::default(),
        crate::store::DEFAULT_MAX_FOLLOW_ON_RECOVERIES,
    );
    run.resolved = (*config != run.base).then(|| Box::new(config.clone()));
    state.install_run_view(&run);
}

/// FIG-4646: uninstalling a run view restores the sticky config under it,
/// every fact of it, so nothing read from the state afterwards is the
/// run's. A second view installed over the first keeps the session's
/// sticky config, not the first view's.
#[test]
fn taking_a_run_view_restores_the_sticky_config() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.session_id = SessionId::from("take-run-view-law");
    state.policy.model = Some(recorded_llm_profile("sticky-route"));
    state.config_revision = 3;
    let sticky = crate::store::persisted_session_config_from_state(&state);
    assert!(state.take_run_view().is_none(), "no view is installed");

    let mut first = sticky.clone();
    first.model = Some(recorded_llm_profile("first-run-route"));
    first.generation.seed = Some(7);
    first.tool_access = crate::SessionToolAccess::ambient()
        .with_hidden_tools(["hidden-by-run-view"])
        .expect("valid hidden tool");
    install_view(&mut state, &first);
    let mut second = sticky.clone();
    second.model = Some(recorded_llm_profile("second-run-route"));
    install_view(&mut state, &second);
    assert_eq!(
        crate::store::execution_session_config_from_state(&state),
        second
    );
    assert_eq!(
        state.authority.run_view().map(|view| &view.sticky),
        Some(&sticky),
        "a view installed over another keeps the session's sticky config"
    );

    let taken = state.take_run_view().expect("the installed view");
    assert_eq!(taken.sticky, sticky);
    assert_eq!(taken.run.config(), &second);
    assert!(state.authority.run_view().is_none());
    assert_eq!(
        crate::store::execution_session_config_from_state(&state),
        sticky,
        "the resident config is the sticky one again"
    );
    assert_eq!(
        crate::store::persisted_session_config_from_state(&state),
        sticky
    );
}

/// FIG-4529: an observer's view of a state under a recorded run view reads
/// the sticky config, while the state's own policy stays the run's.
#[test]
fn recorded_session_view_reads_the_sticky_config_under_a_run_view() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.session_id = SessionId::from("recorded-view-law");
    state.policy.model = Some(recorded_llm_profile("sticky-route"));
    let sticky = state.policy.clone();
    assert_eq!(
        crate::SessionReadView::recorded_from_runtime_state(&state)
            .policy()
            .model,
        sticky.model,
        "without a run view the recorded view is the resident policy"
    );

    let mut root = crate::store::persisted_session_config_from_state(&state);
    root.model = Some(recorded_llm_profile("run-route"));
    root.generation.seed = Some(7);
    install_view(&mut state, &root);

    assert_eq!(
        state.policy.model, root.model,
        "the run executes its own view"
    );
    let recorded = crate::SessionReadView::recorded_from_runtime_state(&state);
    assert_eq!(recorded.policy().model, sticky.model);
    assert_eq!(recorded.policy().generation, sticky.generation);
    assert_eq!(
        crate::store::recorded_session_policy_from_state(&state).model,
        sticky.model
    );
}

#[test]
fn recorded_run_view_never_becomes_sticky_after_commit_replay_or_failed_settlement() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.session_id = SessionId::from("run-config-law");
    state.policy.model = Some(recorded_llm_profile("sticky-route"));
    let sticky = crate::store::persisted_session_config_from_state(&state);
    let mut run = sticky.clone();
    run.model = Some(recorded_llm_profile("run-route"));
    run.autonomous = !sticky.autonomous;

    install_view(&mut state, &run);
    assert_eq!(
        crate::store::execution_session_config_from_state(&state),
        run
    );
    let commit =
        crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            &state,
            crate::store::GraphAppend::PreserveHead,
            boundary_operation(&state.session_id, "root", "commit"),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::store::FleetFormat::current(),
        )
        .expect("run commit");
    assert_eq!(commit.config, sticky);
    assert_eq!(
        commit.config.model,
        Some(recorded_llm_profile("sticky-route"))
    );

    // A failed settlement leaves only the in-memory execution view. A
    // subsequent head reload and recorded replay must still commit the head.
    let head = crate::store::SessionWindowRead::new(
        state.session_id.clone(),
        1,
        sticky.clone(),
        None,
        crate::SessionGraph::default(),
        None,
        None,
    )
    .expect("head window");
    adopt_durable_head(
        &mut state,
        head.clone(),
        crate::store::FleetFormat::current(),
    )
    .expect("head reload");
    assert_eq!(
        crate::store::persisted_session_config_from_state(&state),
        sticky
    );
    install_view(&mut state, &run);
    assert_eq!(
        crate::store::persisted_session_config_from_state(&state),
        sticky
    );
    adopt_durable_head(
        &mut state,
        head.clone(),
        crate::store::FleetFormat::current(),
    )
    .expect("next root reload");
    assert_eq!(
        state.to_snapshot().policy.model,
        Some(recorded_llm_profile("sticky-route"))
    );
    assert_eq!(state.to_snapshot().policy.autonomous, sticky.autonomous);
}

/// A run's commit is identified by the view it ran under, never by the
/// head's sticky config it writes back: a redrive that replays a committed
/// run after a config change moved the head builds the same identity, so
/// the store answers its receipt instead of refusing a conflict. The written
/// config is still the head's.
#[test]
fn a_run_commit_identity_covers_its_view_not_the_sticky_config_it_writes() {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.session_id = SessionId::from("run-config-identity");
    state.policy.model = Some(recorded_llm_profile("first-route"));
    let first = crate::store::persisted_session_config_from_state(&state);
    let commit_under = |state: &RuntimeSessionState| {
        crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            crate::store::GraphAppend::PreserveHead,
            boundary_operation(&state.session_id, "root", "final"),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::store::FleetFormat::current(),
        )
        .expect("run commit")
    };

    // The first execution: the run's recorded view is the head's config.
    install_view(&mut state, &first);
    let original = commit_under(&state);
    assert_eq!(original.config, first);
    assert!(
        original.execution_config.is_none(),
        "the view is the head's"
    );

    // A config change lands on the head after the run committed; the
    // redrive adopts that head, then replays the run's recorded view.
    let mut changed = first.clone();
    changed.model = Some(recorded_llm_profile("second-route"));
    changed.config_revision += 1;
    state.take_run_view();
    adopt_session_config(&mut state, &changed);
    install_view(&mut state, &first);
    let replayed = commit_under(&state);
    assert_eq!(
        replayed.config, changed,
        "the commit writes the head's config"
    );
    assert_eq!(
        replayed.turn_commit_hash().expect("replay identity"),
        original.turn_commit_hash().expect("original identity"),
        "the replay is the run's committed operation"
    );

    // A run that ran under another view is another operation.
    let mut other = first.clone();
    other.model = Some(recorded_llm_profile("other-route"));
    install_view(&mut state, &other);
    assert_ne!(
        commit_under(&state)
            .turn_commit_hash()
            .expect("other identity"),
        original.turn_commit_hash().expect("original identity"),
    );
}

fn projection_text(id: &str) -> crate::Message {
    crate::Message {
        id: id.to_string(),
        role: crate::MessageRole::User,
        parts: crate::shared_parts(vec![crate::Part::text(
            format!("{id}.p0"),
            id.to_string(),
            None,
        )]),
        origin: None,
    }
}

/// ADR 0112 §9, §14 test 7: the state's read model and every read view built
/// from it hand out the same `Arc`s, until an append folds once.
#[test]
fn the_state_and_its_read_views_share_one_projection() {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("shared-projection"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.append_active_conversation_messages(&[projection_text("m1")]);

    let model = state.read_model();
    let again = state.read_model();
    assert!(lash_sansio::AppendVec::ptr_eq(
        &model.messages,
        &again.messages
    ));
    assert!(lash_sansio::AppendVec::ptr_eq(
        &model.active_events,
        &again.active_events
    ));
    assert!(std::sync::Arc::ptr_eq(
        &model.prompt_render_cache,
        &again.prompt_render_cache
    ));

    let view = crate::SessionReadView::from_persisted_state(&state);
    let relation_view = crate::SessionReadView::from_persisted_state_with_relation(
        &state,
        crate::SessionRelation::Root,
    );
    for read in [&view, &relation_view, &state.read_view()] {
        assert!(std::ptr::eq(read.messages(), model.messages.as_slice()));
        assert!(std::ptr::eq(
            read.active_events(),
            model.active_events.as_slice()
        ));
    }

    state.append_active_conversation_messages(&[projection_text("m2")]);
    let folded = state.read_model();
    assert!(!lash_sansio::AppendVec::ptr_eq(
        &model.messages,
        &folded.messages
    ));
    assert_eq!(folded.messages.len(), 2);
    assert!(lash_sansio::AppendVec::ptr_eq(
        &folded.messages,
        &state.read_model().messages
    ));
}

/// ADR 0112 §9, §14 test 6: once a frame switch is durable, the resident
/// graph is the new frame. The window base is the new `FrameOpen`,
/// `persisted_node_ids` stays a subset of the resident ids, and one frame
/// record remains, continuing from the old frame.
#[test]
fn a_durable_frame_switch_leaves_only_the_new_frame_resident() {
    use crate::facade_support::AgentFrameReasonFacadeOps as _;
    let clock = crate::SystemClock;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("frame-residency"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.append_active_conversation_messages(&[projection_text("a1"), projection_text("a2")]);
    let old_frame = state.current_frame_node_id.clone().expect("initial frame");
    let durable = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    state.mark_node_ids_persisted(durable);
    assert_eq!(
        state.session_graph.nodes.len(),
        3,
        "nothing retires in one frame"
    );

    let opened = open_agent_frame_in_state_with_clock(
        &mut state,
        crate::OpenAgentFrameRequest::new(
            crate::FrameKey::from_caller_material("continued").expect("frame material"),
            crate::AgentFrameReason::continue_as(),
        ),
        &clock,
    )
    .expect("open the new frame");
    assert!(opened.opened);
    assert!(
        state.read_model().messages.is_empty(),
        "the pending frame already owns the projection"
    );
    state.append_active_conversation_messages(&[projection_text("b1")]);
    assert_eq!(
        state.session_graph.nodes.len(),
        5,
        "pending nodes stay resident"
    );

    let new_frame = state.current_frame_node_id.clone().expect("new frame");
    let committed = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    state.mark_node_ids_persisted(committed);

    assert_eq!(state.current_frame_node_id.as_ref(), Some(&new_frame));
    let anchor = state.session_graph.anchor().expect("re-anchored").clone();
    assert_eq!(anchor.frame_node_id, new_frame);
    assert_eq!(anchor.previous_frame_node_id.as_ref(), Some(&old_frame));
    assert_eq!(anchor.generation, 3);
    assert_eq!(state.session_graph.nodes.len(), 2);
    assert!(state.persisted_node_ids.iter().all(|id| {
        state
            .session_graph
            .nodes
            .iter()
            .any(|node| node.node_id == id)
    }));
    assert_eq!(state.agent_frames.len(), 1);
    assert_eq!(
        state.agent_frames[0].previous_frame_node_id.as_ref(),
        Some(&old_frame)
    );
    assert_eq!(state.read_model().messages.len(), 1);
}

fn capped_plugin_config(cap: u64) -> crate::PluginConfig {
    let mut config = crate::PluginConfig::default();
    config.insert("cap_owner", serde_json::json!({ "cap": cap }));
    config
}

/// FIG-4379: a run executes under the configuration it was admitted under. A
/// redrive installs the run's recorded `ResolvedRun` over a head a later
/// config patch moved on, and the hook input and a process the run starts
/// both carry the admitted configuration at its admitted revision, not the
/// head's.
#[test]
fn a_redriven_run_executes_under_its_admitted_plugin_config_revision() {
    use crate::session_state::facade_ops::RuntimeSessionStateFacadeOps as _;

    let policy =
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024));
    let mut admitted = crate::PersistedSessionConfig::from(&policy);
    admitted.plugin_config = capped_plugin_config(12);
    admitted.config_revision = 4;
    let resolved = crate::run_spec::RunSpec::default()
        .resolve(
            &admitted,
            None,
            crate::run_spec::TerminationPolicy::default(),
            crate::store::DEFAULT_MAX_FOLLOW_ON_RECOVERIES,
            &crate::provider::EmptyLlmProfiles,
            &crate::run_spec::NoRunOptionsOwner,
        )
        .expect("resolve the run");

    let mut head = admitted.clone();
    head.plugin_config = capped_plugin_config(20);
    head.config_revision = 5;
    let mut state = RuntimeSessionState::new(policy.clone());
    adopt_session_config(&mut state, &head);
    assert_eq!(
        state.admitted_plugin_config(),
        crate::AdmittedPluginConfig::new(capped_plugin_config(20), 5),
        "outside a run the head's configuration is the installed one"
    );

    state.install_run_view(&resolved);
    let expected = crate::AdmittedPluginConfig::new(capped_plugin_config(12), 4);
    assert_eq!(state.admitted_plugin_config(), expected);
    assert_eq!(
        state.process_execution_env_spec(&policy).plugin_config,
        expected,
        "a process the run starts captures the run's admitted configuration"
    );
}

/// FIG-4379: every frame open captures the installed configuration, so a
/// fork point carries the configuration its frame ran under.
#[test]
fn a_frame_captures_the_installed_plugin_config() {
    let mut state = fresh_state_with_initial_frame();
    state.authority.plugin_config = capped_plugin_config(9);
    state.open_unpersisted_initial_frame_under_current_assignment();
    let frame = state
        .session_graph
        .nodes
        .first()
        .expect("the initial frame");
    assert_eq!(
        frame
            .frame_config()
            .expect("a frame carries its config")
            .plugin_config,
        capped_plugin_config(9)
    );
}
