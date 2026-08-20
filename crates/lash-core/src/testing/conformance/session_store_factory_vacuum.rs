//! Vacuum/retention conformance for
//! [`SessionStoreFactory`](crate::SessionStoreFactory) backends.
//!
//! Split out of `session_store_factory.rs` to keep it under the file-size
//! budget; these cases are driven from that module's suite entry.

use super::session_store_factory::session_store_request;
use super::*;

pub(super) async fn session_store_factory_vacuums_organic_retained_tombstone(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "retained-tombstone-source",
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let source = factory
        .create_store(&request)
        .await
        .expect("create retained-tombstone source");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("retained-tombstone leaf");
    source
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit retained-tombstone source");
    factory
        .pin(&leaf_node_id)
        .await
        .expect("pin retained-tombstone leaf");
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete retained-tombstone source");
    factory
        .unpin(&leaf_node_id)
        .await
        .expect("unpin deleted source leaf to zero");

    assert!(
        source
            .load_node(&leaf_node_id)
            .await
            .expect("read retained tombstone")
            .is_none(),
        "decrement-to-zero tombstones must be hidden before vacuum"
    );
    let fork_error = factory
        .fork_at(&crate::ForkSessionRequest {
            session_id: "retained-tombstone-fork".to_string(),
            node_id: leaf_node_id.clone(),
            relation: crate::SessionRelation::Root,
            policy: request.policy,
        })
        .await
        .expect_err("a retained tombstone must not be forkable");
    assert!(matches!(
        fork_error,
        crate::StoreError::ForkPointNotRetained { node_id } if node_id == leaf_node_id
    ));

    let report = source.vacuum().await.expect("vacuum retained tombstone");
    assert_eq!(
        report.removed_node_count, 1,
        "vacuum must physically remove the organically created tombstone"
    );
    assert_eq!(
        source
            .vacuum()
            .await
            .expect("repeat retained-tombstone vacuum")
            .removed_node_count,
        0,
        "vacuum must consume each retained tombstone exactly once"
    );
}

pub(super) async fn session_store_factory_vacuum_is_scoped_to_bound_session(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    // 1. Live sessions: scope agreement over pending turn input tombstones
    let req_a = session_store_request(
        "vacuum-scope-live-a",
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let req_b = session_store_request(
        "vacuum-scope-live-b",
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let store_a = factory.create_store(&req_a).await.expect("create store a");
    let store_b = factory.create_store(&req_b).await.expect("create store b");

    let input_a = store_a
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                &req_a.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("prunable-input-a"),
            )
            .with_source_key("source-a"),
        )
        .await
        .expect("enqueue pending input a");
    store_a
        .cancel_pending_turn_input(&req_a.session_id, &input_a.input_id)
        .await
        .expect("cancel pending input a");

    let input_b = store_b
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                &req_b.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("prunable-input-b"),
            )
            .with_source_key("source-b"),
        )
        .await
        .expect("enqueue pending input b");
    store_b
        .cancel_pending_turn_input(&req_b.session_id, &input_b.input_id)
        .await
        .expect("cancel pending input b");

    // Vacuum store A: only session A's pending input tombstone is removed
    let report_a = store_a.vacuum().await.expect("vacuum session a");
    assert_eq!(
        report_a.removed_node_count, 0,
        "session A vacuum had no node tombstones"
    );
    assert_eq!(
        report_a.removed_pending_turn_input_tombstone_count, 1,
        "session A vacuum must remove session A's cancelled turn input"
    );

    // Repeat vacuum on store A should remove 0
    let repeat_a = store_a.vacuum().await.expect("repeat vacuum session a");
    assert_eq!(repeat_a.removed_node_count, 0);
    assert_eq!(repeat_a.removed_pending_turn_input_tombstone_count, 0);

    // Verify session B's pending input tombstone is untouched: re-enqueue returns existing Cancelled input
    let replay_b = store_b
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                &req_b.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("prunable-input-b"),
            )
            .with_source_key("source-b"),
        )
        .await
        .expect("replay input b");
    assert_eq!(
        replay_b.input_id, input_b.input_id,
        "session B pending tombstone must still exist after session A vacuum"
    );
    assert_eq!(replay_b.state, crate::TurnInputState::Cancelled);

    // Vacuum store B: now removes session B's pending input tombstone
    let report_b = store_b.vacuum().await.expect("vacuum session b");
    assert_eq!(
        report_b.removed_node_count, 0,
        "session B vacuum had no node tombstones"
    );
    assert_eq!(
        report_b.removed_pending_turn_input_tombstone_count, 1,
        "session B vacuum must remove session B's cancelled turn input"
    );

    let repeat_b = store_b.vacuum().await.expect("repeat vacuum session b");
    assert_eq!(repeat_b.removed_node_count, 0);
    assert_eq!(repeat_b.removed_pending_turn_input_tombstone_count, 0);

    // 2. Deleted sessions: scope agreement over tombstoned graph nodes
    let req_c = session_store_request(
        "vacuum-scope-nodes-c",
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let req_d = session_store_request(
        "vacuum-scope-nodes-d",
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let store_c = factory.create_store(&req_c).await.expect("create store c");
    let store_d = factory.create_store(&req_d).await.expect("create store d");

    let mut state_c = crate::RuntimeSessionState {
        session_id: req_c.session_id.clone(),
        ..crate::RuntimeSessionState::new(req_c.policy.clone())
    };
    state_c.ensure_agent_frame_initialized();
    let leaf_c = state_c
        .session_graph
        .leaf_node_id
        .clone()
        .expect("session c leaf");
    store_c
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &state_c,
            &[],
        ))
        .await
        .expect("commit session c");
    factory.pin(&leaf_c).await.expect("pin leaf c");

    let mut state_d = crate::RuntimeSessionState {
        session_id: req_d.session_id.clone(),
        ..crate::RuntimeSessionState::new(req_d.policy.clone())
    };
    state_d.ensure_agent_frame_initialized();
    let leaf_d = state_d
        .session_graph
        .leaf_node_id
        .clone()
        .expect("session d leaf");
    store_d
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &state_d,
            &[],
        ))
        .await
        .expect("commit session d");
    factory.pin(&leaf_d).await.expect("pin leaf d");

    // Both deletes happen before either unpin on purpose. `delete_session` also
    // reclaims tombstoned rows owned by already-deleted sessions (otherwise a
    // tombstone created after its owner's delete could never be reclaimed), so
    // deleting D after unpinning C would legitimately reclaim C's tombstone and
    // leave this case with nothing to say about vacuum scope. Tombstoning both
    // nodes after both deletes keeps each row waiting for its own session's
    // vacuum, which is the property under test.
    factory
        .delete_session(&req_c.session_id)
        .await
        .expect("delete session c");
    factory
        .delete_session(&req_d.session_id)
        .await
        .expect("delete session d");
    factory
        .unpin(&leaf_c)
        .await
        .expect("unpin leaf c to tombstone");
    factory
        .unpin(&leaf_d)
        .await
        .expect("unpin leaf d to tombstone");

    // Vacuum store C: removes session C's tombstoned node only
    let report_c = store_c.vacuum().await.expect("vacuum session c");
    assert_eq!(
        report_c.removed_node_count, 1,
        "session C vacuum must remove session C's tombstoned node"
    );
    assert_eq!(
        report_c.removed_pending_turn_input_tombstone_count, 0,
        "session C had no pending input tombstones"
    );

    let repeat_c = store_c.vacuum().await.expect("repeat vacuum session c");
    assert_eq!(repeat_c.removed_node_count, 0);
    assert_eq!(repeat_c.removed_pending_turn_input_tombstone_count, 0);

    // Vacuum store D: removes session D's tombstoned node (was untouched by session C vacuum)
    let report_d = store_d.vacuum().await.expect("vacuum session d");
    assert_eq!(
        report_d.removed_node_count, 1,
        "session D vacuum must remove session D's tombstoned node (was untouched by session C vacuum)"
    );
    assert_eq!(
        report_d.removed_pending_turn_input_tombstone_count, 0,
        "session D had no pending input tombstones"
    );

    let repeat_d = store_d.vacuum().await.expect("repeat vacuum session d");
    assert_eq!(repeat_d.removed_node_count, 0);
    assert_eq!(repeat_d.removed_pending_turn_input_tombstone_count, 0);
}

/// Unpinning *before* the delete makes the delete itself the tombstoning step,
/// so the backend's delete-time reclaim — not a later vacuum through a stale
/// handle — must be what physically drops the rows. Every backend has to report
/// the same post-delete vacuum count for this order, otherwise a stale handle is
/// load-bearing for reclaim on some backends and inert on others.
pub(super) async fn session_store_factory_vacuum_agrees_on_unpin_before_delete(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "vacuum-unpin-before-delete",
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.expect("create store");

    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let leaf = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("session leaf");
    store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit session");

    factory.pin(&leaf).await.expect("pin leaf");
    factory
        .unpin(&leaf)
        .await
        .expect("unpin leaf before delete");
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete session");

    // The delete already reclaimed the unpinned ancestry, scoped to this
    // session, so the stale handle's vacuum finds nothing left to remove.
    let report = store.vacuum().await.expect("vacuum after delete");
    assert_eq!(
        report.removed_node_count, 0,
        "delete-time reclaim must have removed the unpinned node already; \
         a stale handle's vacuum is not allowed to be the reclaiming step"
    );
    assert_eq!(
        report.removed_pending_turn_input_tombstone_count, 0,
        "session had no pending input tombstones"
    );
}

/// `vacuum` is session-scoped by contract, so a handle with no session binding
/// has no scope to vacuum and must say so with
/// [`StoreError::SessionNotBound`](crate::StoreError::SessionNotBound) rather
/// than fall back to a catalog-wide sweep.
///
/// Backends whose store handle cannot exist without a session id have nothing to
/// police here and report `None`. `None` is a claim about the backend — it is
/// always bound, so it owns reclaim itself and offers no unbound sweep to fence
/// — so the skip is a named `tracing` warning rather than a silent pass.
pub(super) async fn session_store_factory_unbound_vacuum_is_typed_error(
    backend: &str,
    unbound: Option<Arc<dyn crate::store::StoreMaintenance>>,
) {
    let Some(unbound) = unbound else {
        tracing::warn!(
            backend,
            "skipping unbound-vacuum conformance: backend reports no unbound store handle shape, \
             so it takes responsibility for reclaim itself"
        );
        return;
    };
    let error = unbound
        .vacuum()
        .await
        .expect_err("an unbound handle must refuse to vacuum");
    assert!(
        matches!(
            error.stop,
            crate::store::MaintenanceStop::Failed(crate::StoreError::SessionNotBound)
        ),
        "expected StoreError::SessionNotBound, got {error:?}"
    );
}
