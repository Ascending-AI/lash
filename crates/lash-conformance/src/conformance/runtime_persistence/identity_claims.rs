use super::append_receipts::{
    append_request_commit, loaded_conformance_state, seed_append_receipt_state,
};
use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn receipt_replay_rehydrates_recorded_node_clocks_and_ids(store: Arc<dyn RuntimeStore>) {
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "recorded-clock",
        serde_json::json!({"value":7}),
    )];
    let (mut commit, ids) =
        append_request_commit(&mut state, "recorded-clock-operation", &nodes, None);
    if let crate::GraphAppend::Extend { nodes } = &mut commit.graph {
        for node in nodes {
            node.timestamp = "2026-09-01T00:00:00Z".into();
        }
    }
    let first = commit_runtime_state_for_test(&store, commit.clone(), "recorded-clock-first")
        .await
        .expect("commit recorded clock");
    assert!(!first.realized_node_timestamps.is_empty());
    assert!(
        first
            .realized_node_timestamps
            .iter()
            .all(|node| node.timestamp == "2026-09-01T00:00:00Z")
    );
    let head = store
        .load_session_head_meta(&SessionId::from("root"))
        .await
        .expect("head before replay");
    if let crate::GraphAppend::Extend { nodes } = &mut commit.graph {
        for node in nodes {
            node.timestamp = "2026-09-30T23:59:59Z".into();
        }
    }
    let replay = store
        .commit_runtime_state(commit)
        .await
        .expect("replay changed observation");
    assert!(replay.receipt_replayed);
    assert_eq!(
        replay.realized_node_timestamps,
        first.realized_node_timestamps
    );
    assert_eq!(replay.committed_leaf_node_id, first.committed_leaf_node_id);
    assert_eq!(replay.head_revision, first.head_revision);
    let pending = state.pending_graph_commit();
    let mapping = pending
        .nodes()
        .iter()
        .map(|node| node.node_id.clone())
        .zip(
            first
                .realized_node_timestamps
                .iter()
                .map(|node| node.node_id.clone()),
        )
        .collect::<Vec<_>>();
    state
        .session_graph
        .remap_node_ids(&state.session_id, &mapping);
    let observed = first
        .realized_node_timestamps
        .iter()
        .map(|node| {
            let mut observed = node.clone();
            observed.timestamp = "2026-09-30T23:59:59Z".into();
            observed
        })
        .collect::<Vec<_>>();
    state
        .session_graph
        .apply_realized_node_timestamps(&observed);
    assert!(
        state
            .session_graph
            .nodes
            .iter()
            .filter(|node| observed.iter().any(|clock| clock.node_id == node.node_id))
            .all(|node| node.timestamp == "2026-09-30T23:59:59Z")
    );
    state.apply_persisted_commit_result(replay);
    for realized in &first.realized_node_timestamps {
        let node = state
            .session_graph
            .nodes
            .iter()
            .find(|node| node.node_id == realized.node_id)
            .expect("receipt id rehydrates the resident graph");
        assert_eq!(node.timestamp, realized.timestamp);
    }
    let durable = loaded_conformance_state(&store, &SessionId::from("root")).await;
    for id in ids {
        let resident = state
            .session_graph
            .nodes
            .iter()
            .find(|node| node.node_id == id)
            .expect("resident node");
        let recorded = durable
            .session_graph
            .nodes
            .iter()
            .find(|node| node.node_id == id)
            .expect("durable node");
        assert_eq!(resident.timestamp, recorded.timestamp);
    }
    assert_eq!(
        format!(
            "{:?}",
            store
                .load_session_head_meta(&SessionId::from("root"))
                .await
                .expect("head after replay")
        ),
        format!("{head:?}")
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn checkpoint_identity_is_independent_of_compression_profile(
    stores: Vec<Arc<dyn RuntimeStore>>,
) {
    assert!(stores.len() >= 2, "independent writer profiles or handles");
    let session = SessionId::from("profile-identity");
    let bytes = vec![b'x'; 12_288];
    let expected = crate::BlobRef::for_content(&bytes);
    for (index, store) in stores.iter().enumerate() {
        admit_conformance_session(store, &session).await;
        let mut state = if index == 0 {
            RuntimeSessionState {
                session_id: session.clone(),
                ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            }
        } else {
            loaded_conformance_state(store, &session).await
        };
        state.ensure_agent_frame_initialized();
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.checkpoint.components.insert(
            "profile/leaf".into(),
            crate::HydratedCheckpointComponent::changed(bytes.clone()),
        );
        let result =
            commit_runtime_state_for_test(store, commit, &format!("profile-identity-{index}"))
                .await
                .expect("commit on writer profile");
        assert_eq!(
            result.manifest.components["profile/leaf"].blob_ref,
            expected
        );
        let read = store
            .load_session_window(&session, crate::store::WindowSelector::Current)
            .await
            .expect("hydrate on writer profile")
            .expect("profile checkpoint");
        assert_eq!(
            read.checkpoint
                .expect("hydrated checkpoint")
                .component_body("profile/leaf"),
            Some(bytes.as_slice())
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn checkpoint_profile_change_preserves_refs_budget_and_atomic_root_leaves(
    stores: Vec<Arc<dyn RuntimeStore>>,
) {
    assert!(stores.len() >= 2);
    let session = SessionId::from("profile-root-leaves");
    let first_store = &stores[0];
    admit_conformance_session(first_store, &session).await;
    let state = RuntimeSessionState {
        session_id: session.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let bytes = vec![b'a'; 12_288];
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        "profile/leaf".into(),
        crate::HydratedCheckpointComponent::changed(bytes.clone()),
    );
    let first = commit_runtime_state_for_test(first_store, commit, "profile-root-first")
        .await
        .expect("first profile commits leaf");
    let descriptor = first.manifest.components["profile/leaf"].clone();
    for (index, store) in stores.iter().enumerate().skip(1) {
        let state = loaded_conformance_state(store, &session).await;
        let mut unchanged = RuntimeCommit::persisted_state_for_test(&state, &[]);
        unchanged.checkpoint.components.insert(
            "profile/leaf".into(),
            crate::HydratedCheckpointComponent::Unchanged {
                descriptor: descriptor.clone(),
            },
        );
        let reference_budget = unchanged.measure_budget().expect("measure unchanged leaf");
        let mut changed = unchanged.clone();
        changed.checkpoint.components.insert(
            "profile/leaf".into(),
            crate::HydratedCheckpointComponent::changed(bytes.clone()),
        );
        assert_eq!(
            changed
                .measure_budget()
                .expect("measure changed body")
                .checkpoint_bytes,
            reference_budget.checkpoint_bytes + bytes.len(),
            "profiles never change logical body accounting"
        );
        let receipt =
            commit_runtime_state_for_test(store, unchanged, &format!("profile-ref-only-{index}"))
                .await
                .expect("another profile resolves unchanged ref");
        assert_eq!(receipt.manifest.components["profile/leaf"], descriptor);
        let head = format!(
            "{:?}",
            store
                .load_session_head_meta(&session)
                .await
                .expect("head before invalid root")
        );
        let state = loaded_conformance_state(store, &session).await;
        let mut invalid = RuntimeCommit::persisted_state_for_test(&state, &[]);
        invalid.checkpoint.components.insert(
            "profile/new-leaf".into(),
            crate::HydratedCheckpointComponent::changed(b"must roll back".to_vec()),
        );
        let mut missing = descriptor.clone();
        missing.blob_ref = crate::BlobRef::for_content(b"never written");
        invalid.checkpoint.components.insert(
            "profile/missing".into(),
            crate::HydratedCheckpointComponent::Unchanged {
                descriptor: missing,
            },
        );
        let error =
            commit_runtime_state_for_test(store, invalid, &format!("profile-invalid-{index}"))
                .await
                .expect_err("a missing leaf refuses the complete root");
        assert!(
            matches!(error, StoreError::CheckpointComponentMissing { .. }),
            "{error:?}"
        );
        assert_eq!(
            format!(
                "{:?}",
                store
                    .load_session_head_meta(&session)
                    .await
                    .expect("head after invalid root")
            ),
            head
        );
        let read = store
            .load_session_window(&session, crate::store::WindowSelector::Current)
            .await
            .expect("read after failed transaction")
            .expect("retained checkpoint");
        let checkpoint = read.checkpoint.expect("complete checkpoint retained");
        assert_eq!(
            checkpoint.component_body("profile/leaf"),
            Some(bytes.as_slice())
        );
        assert!(checkpoint.component_body("profile/new-leaf").is_none());
    }
}
