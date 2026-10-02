//! Vacuum/retention conformance for
//! [`DeploymentStore`](crate::DeploymentStore) backends.
//!
//! Split out of `session_store_factory.rs` to keep it under the file-size
//! budget; these cases are driven from that module's suite entry.

use super::session_store_factory::session_store_request;
use super::*;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

/// A pin is deleted with its session (FIG-4731): it does not keep the deleted
/// session's graph, so the delete itself reclaims the pinned revision's
/// ancestry, a fork of that revision refuses the deleted session, and a stale
/// handle's vacuum finds nothing left to remove.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_store_factory_delete_takes_the_sessions_pins(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("pinned-at-delete-source"),
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let source = factory
        .admit_view(&request)
        .await
        .expect("create pinned-at-delete source");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("pinned-at-delete leaf");
    let pinned_revision = source
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("commit pinned-at-delete source")
        .head_revision;
    factory
        .pin(
            &request.session_id,
            &crate::Target::Revision(pinned_revision),
        )
        .await
        .expect("pin the committed revision");
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete the pinned source");

    assert!(
        !crate::conformance::helpers::node_readable_through_deleted(&source, &leaf_node_id)
            .await
            .expect("read through the deleted session"),
        "a deleted session's pinned node must not stay readable"
    );
    let fork_error = factory
        .fork_session(&crate::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("pinned-at-delete-fork"),
            source_session_id: request.session_id.clone(),
            head_revision: pinned_revision,
            relation: crate::SessionRelation::Root,
            config: request.config.session_policy().into(),
        })
        .await
        .expect_err("a deleted session's pinned revision must not be forkable");
    assert!(
        matches!(
            &fork_error,
            crate::StoreError::SessionDeleted { session_id }
                if *session_id == request.session_id
        ),
        "the pin went with its session: {fork_error:?}"
    );

    let report = source.vacuum().await.expect("vacuum after delete");
    assert_eq!(
        report.removed_node_count, 0,
        "the delete reclaimed the pinned ancestry; a stale handle's vacuum \
         is not the reclaiming step"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_store_factory_vacuum_is_scoped_to_bound_session(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    // Scope agreement over pending turn input tombstones.
    let req_a = session_store_request(
        &SessionId::from("vacuum-scope-live-a"),
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let req_b = session_store_request(
        &SessionId::from("vacuum-scope-live-b"),
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let store_a = factory.admit_view(&req_a).await.expect("create store a");
    let store_b = factory.admit_view(&req_b).await.expect("create store b");

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
        .cancel_pending_turn_input(&input_a.input_id)
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
        .cancel_pending_turn_input(&input_b.input_id)
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
    assert_eq!(replay_b.state.kind(), crate::TurnInputStateKind::Cancelled);

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
}

/// With its pin released before the delete, the delete is the tombstoning
/// step, so the backend's delete-time reclaim — not a later vacuum through a
/// stale handle — must be what physically drops the rows. Every backend has to report
/// the same post-delete vacuum count for this order, otherwise a stale handle is
/// load-bearing for reclaim on some backends and inert on others.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_store_factory_vacuum_agrees_on_unpin_before_delete(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("vacuum-unpin-before-delete"),
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let store = factory.admit_view(&request).await.expect("create store");

    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let pinned = crate::Target::Revision(
        store
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
            .await
            .expect("commit session")
            .head_revision,
    );

    factory
        .pin(&request.session_id, &pinned)
        .await
        .expect("pin the committed revision");
    factory
        .unpin(&request.session_id, &pinned)
        .await
        .expect("unpin before delete");
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
