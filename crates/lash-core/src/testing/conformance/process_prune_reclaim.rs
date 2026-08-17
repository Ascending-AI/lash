//! Tombstone-reclaim conformance for the *process-prune* delete path.
//!
//! The session-delete path has its own reclaim cases in
//! [`session_store_factory_vacuum`](super::session_store_factory_vacuum). Prune
//! is a second, independent delete path — SQL backends implement it as a batch
//! delete — so the reclaim law has to be asserted through it as well. Both cases
//! below use the same observation trick: reads hide tombstones, so a stale
//! handle's session-scoped `vacuum` count is the only backend-agnostic way to
//! tell a physically reclaimed row from a merely hidden one. A reclaiming prune
//! or delete leaves nothing for that vacuum to remove.

use std::sync::Arc;

/// A prune's ancestry retire tombstones nodes regardless of who owns them, so a
/// batch can tombstone a node owned by a session *outside* the batch. When that
/// owner is already deleted, its id is unbindable and no session-scoped vacuum
/// can ever reach the row again — the prune itself has to reclaim it.
pub async fn process_prune_reclaims_tombstones_owned_by_deleted_sessions(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    const PROCESS_ID: &str = "prune-reclaims-outside-owner";
    const OWNER_SESSION_ID: &str = "prune-reclaim-outside-owner-session";
    let policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);

    let owner_store = create_store(&factory, OWNER_SESSION_ID, &policy).await;
    let owner_leaf = commit_root_node(owner_store.as_ref(), OWNER_SESSION_ID, &policy).await;

    // The process session forks at the owner's tip and grows its own node, so
    // the owner's node has a live child owned by another session.
    let process_session_id = crate::process_runtime_session_ids(PROCESS_ID)[0].clone();
    register_process(registry.as_ref(), PROCESS_ID).await;
    fork_and_advance(
        &factory,
        &owner_leaf,
        &process_session_id,
        "prune-reclaim-outside-child-node",
        &policy,
    )
    .await;

    // The owner's delete cannot reclaim its own leaf: the fork child still hangs
    // off it. The row only becomes a tombstone when the prune retires the
    // child's ancestry, by which point the owner is long gone.
    factory
        .delete_session(OWNER_SESSION_ID)
        .await
        .expect("delete the outside owner session");

    prune_completed_process(registry.as_ref(), PROCESS_ID).await;

    let report = owner_store
        .vacuum()
        .await
        .expect("vacuum the deleted owner's stale handle");
    assert_eq!(
        report.removed_node_count, 0,
        "the prune must reclaim tombstoned rows owned by an already-deleted session; \
         the owner's id is unbindable, so nothing else ever could"
    );
}

/// The mirror case: a pruned process session's own node survives its prune
/// because another session's node hangs off it, and is only tombstoned by that
/// session's later delete. Draining it then depends on the prune having recorded
/// the process-owned id in the deleted set — the frontier every delete-time
/// reclaim arm reads.
pub async fn process_prune_records_deletions_for_later_reclaim(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    const PROCESS_ID: &str = "prune-records-deleted-set";
    const FORK_SESSION_ID: &str = "prune-recorded-fork-child-session";
    let policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);

    let process_session_id = crate::process_runtime_session_ids(PROCESS_ID)[0].clone();
    register_process(registry.as_ref(), PROCESS_ID).await;
    let process_store = create_store(&factory, &process_session_id, &policy).await;
    let process_leaf = commit_root_node(process_store.as_ref(), &process_session_id, &policy).await;

    fork_and_advance(
        &factory,
        &process_leaf,
        FORK_SESSION_ID,
        "prune-recorded-fork-child-node",
        &policy,
    )
    .await;

    prune_completed_process(registry.as_ref(), PROCESS_ID).await;
    assert!(
        factory
            .session_was_deleted(&process_session_id)
            .await
            .expect("probe the deleted set after the prune"),
        "the prune must record {process_session_id} in the deleted set"
    );

    // Now the fork child's delete tombstones the pruned process session's node.
    // Its owner is gone, so this delete's reclaim arm is the last chance to
    // physically remove it.
    factory
        .delete_session(FORK_SESSION_ID)
        .await
        .expect("delete the fork child session");

    let report = process_store
        .vacuum()
        .await
        .expect("vacuum the pruned process session's stale handle");
    assert_eq!(
        report.removed_node_count, 0,
        "a delete must drain tombstoned rows owned by a pruned process session; \
         omitting pruned ids from the deleted set strands them forever"
    );
}

async fn create_store(
    factory: &Arc<dyn crate::SessionStoreFactory>,
    session_id: &str,
    policy: &crate::SessionPolicy,
) -> Arc<dyn crate::RuntimePersistence> {
    factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .unwrap_or_else(|error| panic!("create store {session_id}: {error}"))
}

/// Commit one root node and return its node id.
async fn commit_root_node(
    store: &dyn crate::RuntimePersistence,
    session_id: &str,
    policy: &crate::SessionPolicy,
) -> String {
    let mut state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..crate::RuntimeSessionState::new(policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let leaf = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("root leaf node id");
    store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit root node");
    leaf
}

/// Fork `child_session_id` at `node_id` and grow one node of its own, so
/// `node_id` gains a live child owned by another session.
async fn fork_and_advance(
    factory: &Arc<dyn crate::SessionStoreFactory>,
    node_id: &str,
    child_session_id: &str,
    child_node_id: &str,
    policy: &crate::SessionPolicy,
) {
    factory
        .fork_at(&crate::ForkSessionRequest {
            session_id: child_session_id.to_string(),
            node_id: node_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("fork at a live tip");
    let child = factory
        .open_existing_store(&crate::SessionStoreCreateRequest {
            session_id: child_session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("open the forked child")
        .expect("the forked child exists");
    let mut state = crate::store::load_persisted_session_state(child.as_ref())
        .await
        .expect("load the forked child's state")
        .expect("the forked child has state");
    let parent_node_id = state.session_graph.leaf_node_id.clone();
    state
        .session_graph
        .push_node_record(crate::SessionNodeRecord {
            node_id: child_node_id.to_string(),
            parent_node_id,
            timestamp: "2026-08-17T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed(
                        "prune-reclaim-child-event",
                        serde_json::json!({ "content": "child node" }),
                    )
                    .expect("typed child event"),
                ),
            },
        });
    state
        .session_graph
        .set_leaf_node_id(Some(child_node_id.to_string()));
    child
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("advance the forked child");
}

async fn register_process(registry: &dyn crate::ProcessRegistry, process_id: &str) {
    registry
        .register_process(crate::ProcessRegistration::new(
            process_id,
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryDisposition::ExternallyOwned,
            crate::ProcessProvenance::host(),
        ))
        .await
        .expect("register the pruned process");
}

async fn prune_completed_process(registry: &dyn crate::ProcessRegistry, process_id: &str) {
    let terminal = registry
        .complete_process(
            process_id,
            crate::ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the process under prune");
    let report = registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the terminal process");
    assert_eq!(
        report.pruned_processes, 1,
        "the completed process must be prunable"
    );
}
