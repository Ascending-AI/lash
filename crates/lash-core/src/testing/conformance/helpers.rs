//! Shared fixtures for the conformance suites: paired handles opened
//! against the same durable backing store, used by the `*_reopenable`
//! suite variants.

use super::*;

pub(crate) fn assert_fresh_instances<T: ?Sized>(left: &Arc<T>, right: &Arc<T>, suite: &str) {
    assert!(
        !Arc::ptr_eq(left, right),
        "{suite} factory reused one Arc across conformance roles"
    );
}

pub(crate) fn durable_turn_scope(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> ExecutionScope {
    ExecutionScope::turn(session_id, turn_id)
}

pub(crate) fn durable_turn_address(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> crate::TurnAddress {
    crate::TurnAddress::new(session_id, turn_id)
}

/// A pair of [`ProcessRegistry`] handles opened against the same durable
/// backing store.
pub struct ReopenableProcessRegistry {
    pub open: Arc<dyn ProcessRegistry>,
    pub reopen: Arc<dyn ProcessRegistry>,
}

/// A pair of [`RuntimePersistence`] handles opened against the same durable
/// backing store.
pub struct ReopenableRuntimePersistence {
    pub open: Arc<dyn RuntimePersistence>,
    pub reopen: Arc<dyn RuntimePersistence>,
}

/// Bind every conformance read to the session it intends to inspect.
///
/// This makes SQLite's exactly-one unbound lookup and PostgreSQL's ASC-first
/// unbound lookup unreachable from conformance laws.
pub(crate) async fn bind_conformance_session(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
) {
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .admit_and_bind_session(&crate::SessionBinding::root(session_id, &state.policy))
        .await
        .expect("bind conformance store to its explicit session");
}

/// A pair of [`AttachmentStore`](crate::AttachmentStore) handles opened against
/// the same durable backing store.
pub struct ReopenableAttachmentStore {
    pub open: Arc<dyn crate::AttachmentStore>,
    pub reopen: Arc<dyn crate::AttachmentStore>,
}

/// A pair of [`TriggerStore`](crate::TriggerStore) handles opened against
/// the same durable backing store.
pub struct ReopenableTriggerStore {
    pub open: Arc<dyn crate::TriggerStore>,
    pub reopen: Arc<dyn crate::TriggerStore>,
}

/// Push an unpersisted event node onto `state`'s active path and make it the
/// resident leaf. Pair with [`commit_conformance_state`] to advance a session's
/// durable head from outside any runtime.
pub(crate) fn append_conformance_event_node(
    state: &mut crate::RuntimeSessionState,
    id: &str,
    content: &str,
) {
    let parent_node_id = state.session_graph.leaf_node_id.clone();
    let node = crate::SessionNodeRecord {
        node_id: id.to_string(),
        parent_node_id,
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: crate::SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                crate::ProtocolEvent::typed(
                    "conformance-event",
                    serde_json::json!({ "content": content }),
                )
                .expect("conformance event"),
            ),
        },
    };
    state.session_graph.push_node_record(node);
    state.session_graph.set_leaf_node_id(Some(id.to_string()));
}

pub(crate) async fn commit_conformance_state(
    store: &Arc<dyn crate::RuntimePersistence>,
    state: &mut crate::RuntimeSessionState,
) -> Result<(), crate::StoreError> {
    let operation = crate::OperationId::turn(
        &state.session_id,
        format!("conformance-commit-{}", state.head_revision),
        "commit",
    );
    let (commit, new_node_ids) =
        crate::RuntimeCommit::persisted_state_with_operation(state, &[], operation)?;
    let result = store.commit_runtime_state(commit).await?;
    state.apply_persisted_commit_result(result);
    state.mark_node_ids_persisted(new_node_ids);
    Ok(())
}
