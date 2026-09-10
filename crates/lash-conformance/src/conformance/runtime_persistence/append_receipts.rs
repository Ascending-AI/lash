use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn usage_ordinal_reuse_with_different_payload_survives_receipt_replay(
    store: Arc<dyn RuntimePersistence>,
) {
    let usage = |input_tokens| TokenLedgerEntry {
        source: "ordinal-reuse".to_string(),
        model: "usage-model".to_string(),
        usage: crate::TokenUsage {
            input_tokens,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        },
        usage_disposition: Default::default(),
    };
    let first_usage = usage(11);
    let later_usage = usage(29);
    let nodes = vec![crate::SessionAppendNode::plugin(
        "usage-ordinal-reuse",
        serde_json::json!({"append": "A"}),
    )];
    let mut initial_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };

    // U1 is confirmed under append operation A at ordinal zero.
    let (mut first_append, _) =
        append_request_commit(&mut initial_state, "usage-ordinal-reuse-a", &nodes, None);
    first_append.usage_deltas = crate::store::RuntimeUsageDelta::for_operation(
        &first_append.turn_commit.operation,
        std::slice::from_ref(&first_usage),
    )
    .expect("identify first usage row");
    let first_identity = first_append.usage_deltas[0].identity.clone();
    let first_result =
        commit_runtime_state_for_test(&store, first_append, "usage-ordinal-reuse-first")
            .await
            .expect("commit append A with U1");
    assert_eq!(
        first_result.committed_usage_delta_identities,
        vec![first_identity.clone()]
    );

    // U2 is recorded after U1 confirmation. Replaying A reuses ordinal zero,
    // but its content-bound full identity is distinct.
    let mut retry_state = loaded_conformance_state(&store).await;
    let (mut replay_append, _) =
        append_request_commit(&mut retry_state, "usage-ordinal-reuse-a", &nodes, None);
    replay_append.usage_deltas = crate::store::RuntimeUsageDelta::for_operation(
        &replay_append.turn_commit.operation,
        std::slice::from_ref(&later_usage),
    )
    .expect("identify later usage row");
    let later_delta = replay_append.usage_deltas[0].clone();
    assert_eq!(
        later_delta.identity.operation_storage_key,
        first_identity.operation_storage_key
    );
    assert_eq!(
        later_delta.identity.entry_ordinal,
        first_identity.entry_ordinal
    );
    assert_ne!(
        later_delta.identity.payload_hash,
        first_identity.payload_hash
    );

    let replay = commit_runtime_state_for_test(&store, replay_append, "usage-ordinal-reuse-replay")
        .await
        .expect("replay append A with U2 staged");
    assert!(replay.receipt_replayed);
    assert_eq!(
        replay.committed_usage_delta_identities,
        vec![first_identity]
    );
    assert!(
        !replay
            .committed_usage_delta_identities
            .contains(&later_delta.identity),
        "receipt replay must not confirm a different payload at the reused ordinal"
    );

    // The caller therefore retains U2 and publishes it on the next natural
    // commit. Both full identities must be durable exactly once.
    let mut natural_state = loaded_conformance_state(&store).await;
    let mut natural_commit = RuntimeCommit::persisted_state_for_test(&natural_state, &[]);
    natural_commit.usage_deltas = vec![later_delta.clone()];
    let natural =
        commit_runtime_state_for_test(&store, natural_commit, "usage-ordinal-reuse-natural")
            .await
            .expect("publish U2 on next natural commit");
    assert_eq!(
        natural.committed_usage_delta_identities,
        vec![later_delta.identity]
    );

    natural_state = loaded_conformance_state(&store).await;
    let durable = natural_state
        .token_ledger
        .iter()
        .find(|entry| entry.source == "ordinal-reuse" && entry.model == "usage-model")
        .expect("merged U1 and U2 are durable");
    assert_eq!(durable.usage.input_tokens, 40);
}

pub(super) fn append_request_commit(
    state: &mut RuntimeSessionState,
    operation_id: &str,
    nodes: &[crate::SessionAppendNode],
    requested_ancestor_node_id: Option<&str>,
) -> (RuntimeCommit, Vec<String>) {
    let operation = lash_core::testing::conformance_support::boundary_operation(
        &state.session_id,
        operation_id,
        "append-session-nodes",
    );
    let stamp = RuntimeTurnCommitStamp::append_session_nodes(
        operation.clone(),
        requested_ancestor_node_id,
        nodes,
    )
    .expect("append request identity");
    let draft_namespace = operation
        .storage_key()
        .expect("append operation storage key");
    let requested_node_count =
        lash_core::testing::conformance_support::append_session_nodes_to_state_with_clock(
            state,
            nodes,
            &draft_namespace,
            &crate::SystemClock,
        )
        .len();
    let mut graph = state.pending_graph_commit();
    let mapping = graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive append node ids");
    let persisted = mapping
        .iter()
        .map(|(_, derived)| derived.clone())
        .collect::<Vec<_>>();
    let requested_ids = persisted[persisted.len().saturating_sub(requested_node_count)..].to_vec();
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
    )
    .expect("build append request commit");
    commit.turn_commit = stamp;
    (commit, requested_ids)
}

pub(super) async fn loaded_conformance_state(
    store: &Arc<dyn RuntimePersistence>,
) -> RuntimeSessionState {
    crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load conformance append state")
        .expect("conformance append state exists")
}

pub(super) async fn seed_append_receipt_state(
    store: &Arc<dyn RuntimePersistence>,
) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt-seed",
        serde_json::json!({"seed": true}),
    )];
    let (commit, _) = append_request_commit(&mut state, "append-receipt-seed", &nodes, None);
    commit_runtime_state_for_test(store, commit, "append-receipt-seed")
        .await
        .expect("seed append receipt state");
    loaded_conformance_state(store).await
}

pub(super) async fn append_request_receipt_replays_after_head_advance(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let required = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": 1}),
    )];
    let (first_commit, first_node_ids) =
        append_request_commit(&mut state, "head-advanced-retry", &nodes, Some(&required));
    let first_hash = first_commit.turn_commit_hash().expect("first append hash");
    let first = commit_runtime_state_for_test(&store, first_commit, "head-advanced-first")
        .await
        .expect("first append receipt commit");

    let mut advanced = loaded_conformance_state(&store).await;
    let advance_nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": 2}),
    )];
    let (advance_commit, _) =
        append_request_commit(&mut advanced, "head-advanced-other", &advance_nodes, None);
    commit_runtime_state_for_test(&store, advance_commit, "head-advanced-other")
        .await
        .expect("advance append receipt head");

    let mut retry_state = loaded_conformance_state(&store).await;
    let (retry_commit, retry_node_ids) = append_request_commit(
        &mut retry_state,
        "head-advanced-retry",
        &nodes,
        Some(&required),
    );
    assert_ne!(
        retry_commit.turn_commit_hash().expect("retry hash"),
        first_hash,
        "head movement must change the whole-commit hash used by the legacy receipt arm"
    );
    let retry = store
        .commit_runtime_state(retry_commit)
        .await
        .expect("head-advanced append retry replays");
    assert!(retry.receipt_replayed);
    assert_eq!(retry_node_ids, first_node_ids);
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(retry.committed_leaf_node_id, first.committed_leaf_node_id);
    assert_eq!(
        retry.realized_node_timestamps,
        first.realized_node_timestamps
    );
    let read = store
        .load_session()
        .await
        .expect("load exactly-once append")
        .expect("append session");
    for node_id in first_node_ids {
        assert_eq!(
            read.graph
                .nodes
                .iter()
                .filter(|node| node.node_id == node_id)
                .count(),
            1,
            "the retried append node must exist exactly once"
        );
    }
}

pub(super) async fn append_request_receipt_rejects_changed_content(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let original_nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "original"}),
    )];
    let (first_commit, first_ids) =
        append_request_commit(&mut state, "changed-content", &original_nodes, None);
    commit_runtime_state_for_test(&store, first_commit, "changed-content-first")
        .await
        .expect("first changed-content append");
    let before = store
        .load_session()
        .await
        .expect("load before conflict")
        .unwrap();

    let mut retry_state = loaded_conformance_state(&store).await;
    let changed_nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "changed"}),
    )];
    let (changed_commit, _) =
        append_request_commit(&mut retry_state, "changed-content", &changed_nodes, None);
    let error = store
        .commit_runtime_state(changed_commit)
        .await
        .expect_err("operation id reuse with changed content must conflict");
    assert!(matches!(
        error,
        StoreError::AppendOperationIdentityConflict { ref session_id, .. }
            if session_id == "root"
    ));
    let after = store
        .load_session()
        .await
        .expect("load after conflict")
        .unwrap();
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(after.graph.leaf_node_id, before.graph.leaf_node_id);
    assert_eq!(after.graph.nodes.len(), before.graph.nodes.len());
    assert!(
        first_ids
            .iter()
            .all(|id| after.graph.find_node(id).is_some())
    );
}

pub(super) async fn append_request_exact_hash_rejects_changed_ancestor(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let required = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "changed-ancestor"}),
    )];
    let (first, _) = append_request_commit(
        &mut state,
        "changed-ancestor-exact-hash",
        &nodes,
        Some(&required),
    );
    let mut changed_ancestor = first.clone();
    changed_ancestor.turn_commit = RuntimeTurnCommitStamp::append_session_nodes(
        first.turn_commit.operation.clone(),
        None,
        &nodes,
    )
    .expect("changed ancestor identity");
    assert_eq!(
        first.turn_commit_hash().expect("first hash"),
        changed_ancestor.turn_commit_hash().expect("retry hash"),
        "ancestor metadata is intentionally outside the whole-commit hash"
    );
    commit_runtime_state_for_test(&store, first, "changed-ancestor-first")
        .await
        .expect("first changed-ancestor append");

    let error = store
        .commit_runtime_state(changed_ancestor)
        .await
        .expect_err("changed requested ancestor must conflict despite an exact commit hash");
    assert!(matches!(
        error,
        StoreError::AppendOperationIdentityConflict { .. }
    ));
}

pub(super) async fn append_request_receipt_rejects_corrupt_node_count(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "count-cross-check"}),
    )];
    let (first, _) = append_request_commit(&mut state, "count-cross-check", &nodes, None);
    let mut corrupt_retry = first.clone();
    let crate::AppendRequestIdentity::Append {
        requested_node_count,
        ..
    } = &mut corrupt_retry.turn_commit.append_request_identity
    else {
        panic!("append identity");
    };
    *requested_node_count += 1;
    commit_runtime_state_for_test(&store, first, "count-cross-check-first")
        .await
        .expect("first count-cross-check append");

    let error = store
        .commit_runtime_state(corrupt_retry)
        .await
        .expect_err("matching receipt hashes with a different node count are corruption");
    assert!(matches!(
        error,
        StoreError::AppendReceiptRequestedNodeCountCorrupt {
            stored: Some(1),
            attempted: Some(2),
            ..
        }
    ));
}

/// Adopted FIG-2480 semantic-boundary operations, paired with a distinct
/// boundary id so each iteration owns its own receipt row.
const SEMANTIC_BOUNDARY_OPERATIONS: [(&str, &str); 3] = [
    ("record-config", "protocol-materialization"),
    ("create-session", "semantic-child"),
    ("usage-ledger", "semantic-child-turn"),
];

pub(super) fn semantic_boundary_commit(
    state: &RuntimeSessionState,
    boundary_id: &str,
    operation_key: &str,
) -> RuntimeCommit {
    let operation = lash_core::testing::conformance_support::boundary_operation(
        &state.session_id,
        boundary_id,
        operation_key,
    );
    let mut commit =
        RuntimeCommit::persisted_state_with_operation_for_testing(state, &[], operation);
    commit
        .stamp_semantic_boundary()
        .expect("stamp semantic-boundary receipt identity");
    commit
}

pub(super) async fn semantic_boundary_receipt_replays_after_head_advance(
    store: Arc<dyn RuntimePersistence>,
) {
    seed_append_receipt_state(&store).await;
    for (key, boundary) in SEMANTIC_BOUNDARY_OPERATIONS {
        let state = loaded_conformance_state(&store).await;
        let first_commit = semantic_boundary_commit(&state, boundary, key);
        let first_hash = first_commit
            .turn_commit_hash()
            .expect("first semantic hash");
        let first = commit_runtime_state_for_test(&store, first_commit, "semantic-first")
            .await
            .expect("first semantic-boundary commit");

        let mut advanced = loaded_conformance_state(&store).await;
        let nodes = vec![crate::SessionAppendNode::plugin(
            "semantic-boundary-advance",
            serde_json::json!({ "advance": key }),
        )];
        let (advance_commit, _) =
            append_request_commit(&mut advanced, &format!("advance-{key}"), &nodes, None);
        commit_runtime_state_for_test(&store, advance_commit, "semantic-advance")
            .await
            .expect("advance head between semantic-boundary attempts");

        let retry_state = loaded_conformance_state(&store).await;
        let retry_commit = semantic_boundary_commit(&retry_state, boundary, key);
        assert_ne!(
            retry_commit
                .turn_commit_hash()
                .expect("retry semantic hash"),
            first_hash,
            "head movement must change the whole-commit hash; only the semantic identity replays"
        );
        let retry = store
            .commit_runtime_state(retry_commit)
            .await
            .expect("rebuilt same-request semantic retry must replay");
        assert!(
            retry.receipt_replayed,
            "{key} rebuilt retry must be answered from receipt evidence"
        );
        assert_eq!(retry.head_revision, first.head_revision);
        assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    }
}

pub(super) async fn semantic_boundary_receipt_rejects_changed_content(
    store: Arc<dyn RuntimePersistence>,
) {
    seed_append_receipt_state(&store).await;
    for (key, boundary) in SEMANTIC_BOUNDARY_OPERATIONS {
        let state = loaded_conformance_state(&store).await;
        let first = semantic_boundary_commit(&state, boundary, key);
        commit_runtime_state_for_test(&store, first, "semantic-changed-first")
            .await
            .expect("first semantic-boundary commit");
        let before = store
            .load_session()
            .await
            .expect("load before semantic conflict")
            .expect("semantic conflict session");

        let retry_state = loaded_conformance_state(&store).await;
        let operation = lash_core::testing::conformance_support::boundary_operation(
            &retry_state.session_id,
            boundary,
            key,
        );
        let mut changed =
            RuntimeCommit::persisted_state_with_operation_for_testing(&retry_state, &[], operation);
        changed.config.provider_id = format!("changed-{key}");
        changed
            .stamp_semantic_boundary()
            .expect("stamp changed semantic-boundary identity");
        let error = store
            .commit_runtime_state(changed)
            .await
            .expect_err("semantic-boundary reuse with different request content must be refused");
        assert!(
            matches!(
                error,
                StoreError::SemanticBoundaryIdentityConflict {
                    ref session_id,
                    ref operation_key,
                } if session_id == "root" && operation_key == key
            ),
            "{key} differing canonical encoding must refuse, got {error:?}"
        );
        let after = store
            .load_session()
            .await
            .expect("load after semantic conflict")
            .expect("semantic conflict session");
        assert_eq!(after.head_revision, before.head_revision);
    }
}

pub(super) async fn semantic_boundary_receipt_rejects_mislabeled_identity(
    store: Arc<dyn RuntimePersistence>,
) {
    seed_append_receipt_state(&store).await;
    for (key, boundary) in SEMANTIC_BOUNDARY_OPERATIONS {
        // An Append-labeled identity on a semantic-boundary operation is refused.
        let state = loaded_conformance_state(&store).await;
        let operation = lash_core::testing::conformance_support::boundary_operation(
            &state.session_id,
            boundary,
            key,
        );
        let mut appendish = RuntimeCommit::persisted_state_with_operation_for_testing(
            &state,
            &[],
            operation.clone(),
        );
        appendish.turn_commit.append_request_identity = lash_core::AppendRequestIdentity::Append {
            encoding_version: 1,
            request_hash: "mislabeled-append".into(),
            requested_node_count: 0,
            requested_ancestor_node_id: None,
        };
        let error = store
            .commit_runtime_state(appendish)
            .await
            .expect_err("append identity is operation-specific");
        assert!(
            matches!(
                error,
                StoreError::Backend(ref message)
                    if message
                        == &format!("append receipt identity metadata is invalid for operation `{key}`")
            ),
            "{key} append-labeled identity must refuse, got {error:?}"
        );

        // A semantic identity carrying a foreign operation tag is refused.
        let mut foreign = semantic_boundary_commit(&state, boundary, key);
        let lash_core::AppendRequestIdentity::SemanticBoundary {
            operation: tag,
            encoding_version,
            request_hash,
        } = foreign.turn_commit.append_request_identity.clone()
        else {
            panic!("semantic identity");
        };
        let wrong = match tag {
            lash_core::SemanticBoundaryOperation::RecordConfig => {
                lash_core::SemanticBoundaryOperation::CreateSession
            }
            lash_core::SemanticBoundaryOperation::CreateSession => {
                lash_core::SemanticBoundaryOperation::UsageLedger
            }
            lash_core::SemanticBoundaryOperation::UsageLedger => {
                lash_core::SemanticBoundaryOperation::RecordConfig
            }
        };
        foreign.turn_commit.append_request_identity =
            lash_core::AppendRequestIdentity::SemanticBoundary {
                operation: wrong,
                encoding_version,
                request_hash,
            };
        let error = store
            .commit_runtime_state(foreign)
            .await
            .expect_err("a foreign semantic-boundary operation tag must be refused");
        assert!(
            matches!(
                error,
                StoreError::Backend(ref message)
                    if message
                        == &format!(
                            "semantic-boundary receipt identity `{}` is invalid for operation `{key}`",
                            wrong.operation_key()
                        )
            ),
            "{key} foreign tag must refuse, got {error:?}"
        );
    }

    // A non-adopting operation cannot claim a semantic identity.
    let state = loaded_conformance_state(&store).await;
    let operation = lash_core::testing::conformance_support::boundary_operation(
        &state.session_id,
        "semantic-park",
        "initial-park",
    );
    let mut park =
        RuntimeCommit::persisted_state_with_operation_for_testing(&state, &[], operation);
    park.turn_commit.append_request_identity = lash_core::AppendRequestIdentity::SemanticBoundary {
        operation: lash_core::SemanticBoundaryOperation::RecordConfig,
        encoding_version: 1,
        request_hash: "foreign-family".into(),
    };
    let error = store
        .commit_runtime_state(park)
        .await
        .expect_err("non-adopting operations must refuse semantic-boundary identities");
    assert!(
        matches!(
            error,
            StoreError::Backend(ref message)
                if message
                    == "semantic-boundary receipt identity `record-config` is invalid for \
                        operation `initial-park`"
        ),
        "non-adopting operation must refuse the semantic family, got {error:?}"
    );
}

/// Prove that a SQL backend refuses a receipt whose stored append-identity
/// encoding version cannot be represented by the public identity type.
///
/// `corrupt` installs the backend-native malformed value after the canonical
/// first receipt has been written and before an exact retry reads it.
pub async fn append_receipt_corrupt_identity_encoding_version_is_refused<F, Fut>(
    store: Arc<dyn RuntimePersistence>,
    corrupt: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "corrupt-identity-version"}),
    )];
    let (first, _) = append_request_commit(&mut state, "corrupt-identity-version", &nodes, None);
    let exact_retry = first.clone();
    commit_runtime_state_for_test(&store, first, "corrupt-identity-version-first")
        .await
        .expect("write canonical append receipt");

    corrupt().await;

    let error = store
        .commit_runtime_state(exact_retry)
        .await
        .expect_err("corrupt stored append identity version must be refused");
    assert!(
        matches!(
            &error,
            StoreError::StoredDataCorrupt { record_kind, message }
                if *record_kind == "RuntimeCommitReceipt append identity"
                    && message.contains("identity_encoding_version")
        ),
        "corrupt stored append identity version must be a typed refusal, got {error:?}"
    );
}

pub(super) async fn concurrent_same_append_operation_applies_exactly_once(
    store: Arc<dyn RuntimePersistence>,
) {
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt-race",
        serde_json::json!({"value": "same-operation"}),
    )];
    let (left, node_ids) =
        append_request_commit(&mut state.clone(), "same-operation-race", &nodes, None);
    let (right, right_node_ids) =
        append_request_commit(&mut state.clone(), "same-operation-race", &nodes, None);
    assert_eq!(node_ids, right_node_ids);
    assert_eq!(
        left.turn_commit_hash().expect("left race hash"),
        right.turn_commit_hash().expect("right race hash")
    );

    let (left_result, right_result) = tokio::join!(
        store.commit_runtime_state(left),
        store.commit_runtime_state(right)
    );
    let left_result = left_result.expect("left same-operation race result");
    let right_result = right_result.expect("right same-operation race result");
    assert_ne!(
        left_result.receipt_replayed, right_result.receipt_replayed,
        "one concurrent attempt must publish and the other must replay"
    );
    let read = store
        .load_session()
        .await
        .expect("load same-operation race")
        .expect("same-operation race session");
    for node_id in node_ids {
        assert_eq!(
            read.graph
                .nodes
                .iter()
                .filter(|node| node.node_id == node_id)
                .count(),
            1,
            "the concurrent same-operation node must be durable exactly once"
        );
    }
}

/// Prove that a durable append receipt wins after a branch switch removes the
/// request's ancestor from the active path.
///
/// `supersede` is a backend test hook that atomically moves the durable leaf to
/// the supplied earlier node and advances the head revision. Conformance-suite
/// embedders use their backend's raw test access for that single mutation.
///
/// Integrator class (ADR 0051): **conformance-suite embedders**.
pub async fn append_request_receipt_replays_after_ancestor_superseded<F, Fut>(
    store: Arc<dyn RuntimePersistence>,
    supersede: F,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut state = seed_append_receipt_state(&store).await;
    let required = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let superseding_leaf = state
        .session_graph
        .find_node(&required)
        .and_then(|node| node.parent_node_id.clone())
        .expect("seed append has the initial frame as parent");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "ancestor"}),
    )];
    let (first_commit, _) =
        append_request_commit(&mut state, "ancestor-replay", &nodes, Some(&required));
    let first = commit_runtime_state_for_test(&store, first_commit, "ancestor-replay-first")
        .await
        .expect("first ancestor append");

    supersede(superseding_leaf).await;
    let mut retry_state = loaded_conformance_state(&store).await;
    assert!(
        !retry_state.session_graph.active_path_contains(&required),
        "the backend hook must move the requested ancestor off the active path"
    );
    let (retry, _) =
        append_request_commit(&mut retry_state, "ancestor-replay", &nodes, Some(&required));
    let replay = store
        .commit_runtime_state(retry)
        .await
        .expect("receipt replay must precede the fresh ancestor fence");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, first.head_revision);
}

/// Prove that an inactive append ancestor wins over a simultaneously stale
/// head revision.
///
/// `supersede` atomically moves the durable head to the supplied earlier node
/// and advances its revision, constructing both rejected conditions without
/// creating a receipt.
///
/// Integrator class (ADR 0051): **conformance-suite embedders**.
pub async fn inactive_append_ancestor_precedes_stale_head<F, Fut>(
    store: Arc<dyn RuntimePersistence>,
    supersede: F,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut state = seed_append_receipt_state(&store).await;
    let required = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let superseding_leaf = state
        .session_graph
        .find_node(&required)
        .and_then(|node| node.parent_node_id.clone())
        .expect("seed append has the initial frame as parent");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-precedence",
        serde_json::json!({"value": "stale-head-and-ancestor"}),
    )];
    let (fresh, _) = append_request_commit(
        &mut state,
        "stale-head-and-ancestor",
        &nodes,
        Some(&required),
    );

    supersede(superseding_leaf).await;
    let error = store
        .commit_runtime_state(fresh)
        .await
        .expect_err("inactive ancestor must reject even when the head is also stale");
    assert!(
        matches!(
            &error,
            StoreError::AppendAncestorNotActive { required_node_id }
                if required_node_id == &required
        ),
        "inactive ancestor must precede stale head, got {error:?}"
    );
}

/// Prove that a head pointing at a tombstoned leaf is rejected as graph
/// corruption instead of being treated as a valid same-leaf commit.
///
/// Integrator class (ADR 0051): **conformance-suite embedders**.
pub async fn tombstoned_old_leaf_is_rejected<F, Fut>(
    store: Arc<dyn RuntimePersistence>,
    tombstone: F,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut state = seed_append_receipt_state(&store).await;
    let old_leaf = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "tombstoned-old-leaf",
        serde_json::json!({"value": "must-not-append"}),
    )];
    let (append, _) = append_request_commit(&mut state, "tombstoned-old-leaf", &nodes, None);

    tombstone(old_leaf.clone()).await;
    let error = store
        .commit_runtime_state(append)
        .await
        .expect_err("a tombstoned published leaf must reject the commit");
    assert!(
        matches!(
            &error,
            StoreError::InvalidGraphLeaf {
                leaf_node_id: Some(leaf)
            } if leaf == &old_leaf
        ),
        "tombstoned old leaf must be a typed invalid leaf, got {error:?}"
    );
}

pub(super) async fn legacy_append_receipt_keeps_exact_hash_semantics(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "legacy"}),
    )];
    let (mut legacy, _) = append_request_commit(&mut state, "legacy-receipt", &nodes, None);
    legacy.turn_commit = RuntimeTurnCommitStamp::new(legacy.turn_commit.operation.clone());
    let exact_retry = legacy.clone();
    commit_runtime_state_for_test(&store, legacy, "legacy-receipt-first")
        .await
        .expect("first legacy receipt");
    let replay = store
        .commit_runtime_state(exact_retry.clone())
        .await
        .expect("legacy exact-hash retry replays");
    assert!(replay.receipt_replayed);

    let mut changed = exact_retry;
    changed.checkpoint.turn_state.turn_index += 1;
    let error = store
        .commit_runtime_state(changed)
        .await
        .expect_err("legacy changed-hash retry conflicts");
    assert!(matches!(
        error,
        StoreError::RuntimeTurnCommitConflict { .. }
    ));
}

pub(super) async fn append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "versioned"}),
    )];
    let (mut future_version, _) =
        append_request_commit(&mut state, "version-mismatch", &nodes, None);
    let crate::AppendRequestIdentity::Append {
        encoding_version, ..
    } = &mut future_version.turn_commit.append_request_identity
    else {
        panic!("append identity");
    };
    *encoding_version += 1;
    let mut exact_retry = future_version.clone();
    let crate::AppendRequestIdentity::Append {
        encoding_version, ..
    } = &mut exact_retry.turn_commit.append_request_identity
    else {
        panic!("append identity");
    };
    *encoding_version = 1;
    commit_runtime_state_for_test(&store, future_version, "version-mismatch-first")
        .await
        .expect("first future-version receipt");
    let exact = store
        .commit_runtime_state(exact_retry.clone())
        .await
        .expect("version mismatch exact-hash retry replays");
    assert!(exact.receipt_replayed);

    let mut changed = exact_retry;
    changed.checkpoint.turn_state.turn_index += 1;
    let error = store
        .commit_runtime_state(changed)
        .await
        .expect_err("version mismatch changed-hash retry uses legacy conflict");
    assert!(matches!(
        error,
        StoreError::RuntimeTurnCommitConflict { .. }
    ));
}

pub(super) async fn append_receipt_and_graph_append_are_atomic(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "atomic"}),
    )];
    let (clean, ids) = append_request_commit(&mut state, "atomic-append", &nodes, None);
    let mut failing = clean.clone();
    failing
        .enqueued_queue_batches
        .push(QueuedWorkBatchDraft::new(
            "different-session",
            DeliveryPolicy::AfterCurrentTurnCommit,
            crate::TurnWorkPayload::agent_frame_task(
                crate::session_graph::frame_node_id("different-session", "atomic-frame"),
                "must roll back",
                None,
            ),
        ));
    let failing_lease =
        claim_session_execution_lease_for_test(&store, "root", "atomic-append-failing").await;
    let error = store
        .commit_runtime_state(failing.releasing_session_execution_lease(failing_lease.completion()))
        .await
        .expect_err("mid-commit outbox failure rolls back append and receipt");
    assert!(matches!(error, StoreError::SessionBindingMismatch { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load failed append")
            .is_none()
    );
    release_session_execution_lease_for_test(&store, &failing_lease).await;

    commit_runtime_state_for_test(&store, clean, "atomic-append-retry")
        .await
        .expect("fresh retry after rollback succeeds");
    let read = store.load_session().await.expect("load retry").unwrap();
    assert!(ids.iter().all(|id| read.graph.find_node(id).is_some()));
}

pub(super) async fn fresh_append_receipt_enforces_ancestor_precondition(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let before = store
        .load_session()
        .await
        .expect("load before stale")
        .unwrap();
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "stale"}),
    )];
    let (fresh, _) = append_request_commit(
        &mut state,
        "fresh-stale-ancestor",
        &nodes,
        Some("not-on-the-active-path"),
    );
    let error = store
        .commit_runtime_state(fresh)
        .await
        .expect_err("fresh append must enforce ancestor precondition");
    assert!(matches!(
        error,
        StoreError::AppendAncestorNotActive { ref required_node_id }
            if required_node_id == "not-on-the-active-path"
    ));
    let after = store
        .load_session()
        .await
        .expect("load after stale")
        .unwrap();
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(after.graph.leaf_node_id, before.graph.leaf_node_id);
    assert_eq!(after.graph.nodes.len(), before.graph.nodes.len());
}
