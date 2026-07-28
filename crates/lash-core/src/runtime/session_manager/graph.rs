use super::*;
use std::sync::atomic::Ordering;

impl CurrentSessionCapability {
    pub(in crate::runtime::session_manager) async fn append_session_nodes(
        &self,
        managed: &ManagedSessionCapability,
        usage: &UsageCapability,
        background: &ProcessCapability,
        session_id: &str,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesResult, crate::PluginError> {
        if request.operation_id.trim().is_empty() {
            return Err(crate::PluginError::Session(
                "session graph append requires a non-empty stable operation_id".to_string(),
            ));
        }
        if let Some(runtime) = {
            let registry = managed.registry.lock().await;
            registry.get(session_id).cloned()
        } {
            let mut writer = runtime.runtime.lock().await;
            let result = writer
                .append_session_nodes(request)
                .await
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
            runtime.publish_from(&writer);
            return Ok(result);
        }

        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }

        let Some(store) = &self.store else {
            return Err(crate::PluginError::Session(
                "session graph mutation requires a runtime store".to_string(),
            ));
        };

        let mut state = if usage.persist_to_store {
            self.current_snapshot_for_store_write().await?
        } else {
            self.snapshot.to_runtime_state()
        };
        let usage_deltas = if usage.persist_to_store {
            usage.merge_drained_token_ledger(&mut state)
        } else {
            Vec::new()
        };
        if let Some(required) = request.requires_ancestor_node_id.as_deref()
            && !state.session_graph.active_path_contains(required)
        {
            return Ok(crate::AppendSessionNodesResult::StaleBranch {
                current_leaf_node_id: state.session_graph.leaf_node_id.clone(),
            });
        }
        let operation = super::super::state::boundary_operation(
            &state.session_id,
            &request.operation_id,
            "append-session-nodes",
        );
        let draft_namespace = operation
            .storage_key()
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        let node_ids = append_session_nodes_to_state_with_clock(
            &mut state,
            &request.nodes,
            &draft_namespace,
            self.host.core.clock.as_ref(),
        );
        let requested_node_count = node_ids.len();
        let mut graph = state.pending_graph_commit();
        let persisted_node_ids =
            super::super::state::derive_graph_commit_node_ids(&mut state, &mut graph, &operation)
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        let node_ids = persisted_node_ids[persisted_node_ids
            .len()
            .saturating_sub(requested_node_count)..]
            .to_vec();
        let leaf_node_id = state.session_graph.leaf_node_id.clone().unwrap_or_default();
        let mut commit = crate::store::RuntimeCommit::persisted_state_with_graph_commit(
            &state,
            graph,
            &usage_deltas,
        );
        let hash = commit
            .turn_commit_hash()
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        commit.turn_commit = Some(crate::RuntimeTurnCommitStamp::new(
            state.session_id.clone(),
            operation,
            hash,
        ));
        let result = commit_runtime_state_with_fresh_session_execution_lease(
            Arc::clone(store),
            commit,
            &self.runtime_lease_owner,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        state.apply_persisted_commit_result(result);
        background.sync_needed.store(true, Ordering::Release);
        Ok(crate::AppendSessionNodesResult::Appended {
            node_ids,
            leaf_node_id,
        })
    }
}
