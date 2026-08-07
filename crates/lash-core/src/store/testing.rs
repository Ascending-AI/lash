use super::{RuntimeCommit, RuntimeTurnCommitStamp, StoreError};

/// Build an identity-bearing append commit with a caller-owned clock.
#[doc(hidden)]
pub fn append_request_commit_with_clock_for_testing(
    state: &mut crate::RuntimeSessionState,
    operation_id: &str,
    nodes: &[crate::SessionAppendNode],
    requested_ancestor_node_id: Option<&str>,
    clock: &dyn crate::Clock,
) -> Result<RuntimeCommit, StoreError> {
    let operation = crate::runtime::state::boundary_operation(
        &state.session_id,
        operation_id,
        "append-session-nodes",
    );
    let stamp = RuntimeTurnCommitStamp::append_session_nodes(
        operation.clone(),
        requested_ancestor_node_id,
        nodes,
    )?;
    let draft_namespace = operation.storage_key()?;
    crate::runtime::state::append_session_nodes_to_state_with_clock(
        state,
        nodes,
        &draft_namespace,
        clock,
    );
    let mut graph = state.pending_graph_commit();
    graph.derive_node_ids(&state.session_id, &operation)?;
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
    )?;
    commit.turn_commit = stamp;
    commit.debug_assert_append_envelope_scope();
    Ok(commit)
}
