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
                .map_err(|err| match err {
                    crate::SessionError::Store {
                        source:
                            crate::StoreError::AppendOperationIdentityConflict {
                                session_id,
                                operation_key,
                            },
                        ..
                    } => crate::PluginError::AppendOperationIdentityConflict {
                        session_id,
                        operation_key,
                    },
                    err => crate::PluginError::Session(err.to_string()),
                })?;
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
        let mut staged_usage = if usage.persist_to_store {
            Some(usage.stage_token_ledger(&mut state))
        } else {
            None
        };
        let operation = super::super::state::boundary_operation(
            &state.session_id,
            &request.operation_id,
            "append-session-nodes",
        );
        let append_stamp = crate::RuntimeTurnCommitStamp::append_session_nodes(
            operation.clone(),
            request.requires_ancestor_node_id.as_deref(),
            &request.nodes,
        )
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
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
        let node_id_mapping = graph
            .derive_node_ids(&state.session_id, &operation)
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        let persisted_node_ids = node_id_mapping
            .iter()
            .map(|(_, derived)| derived.clone())
            .collect::<Vec<_>>();
        let locally_derived_node_ids = persisted_node_ids[persisted_node_ids
            .len()
            .saturating_sub(requested_node_count)..]
            .to_vec();
        let locally_derived_leaf_node_id = graph.leaf_node_id.clone().unwrap_or_default();
        let usage_deltas = staged_usage
            .as_ref()
            .map_or(&[][..], |staged| staged.deltas());
        let mut commit =
            crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
                &state,
                graph,
                usage_deltas,
                operation,
            )
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        commit.turn_commit = append_stamp;
        let result = match commit_runtime_state_with_fresh_session_execution_lease(
            Arc::clone(store),
            commit,
            &self.runtime_lease_owner,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        {
            Ok(result) => result,
            Err(crate::StoreError::AppendAncestorNotActive { required_node_id }) => {
                return Ok(crate::AppendSessionNodesResult::StaleBranch { required_node_id });
            }
            Err(crate::StoreError::AppendOperationIdentityConflict {
                session_id,
                operation_key,
            }) => {
                return Err(crate::PluginError::AppendOperationIdentityConflict {
                    session_id,
                    operation_key,
                });
            }
            Err(err) => return Err(crate::PluginError::Session(err.to_string())),
        };
        let receipt_replayed = result.receipt_replayed;
        let committed_leaf_node_id = result.committed_leaf_node_id.clone();
        if !receipt_replayed && let Some(staged) = staged_usage.take() {
            staged.commit();
        }
        let node_ids = if receipt_replayed {
            super::super::state::receipt_append_node_ids(&result, requested_node_count)
                .map_err(|err| crate::PluginError::Session(err.to_string()))?
        } else {
            locally_derived_node_ids
        };
        if !receipt_replayed {
            super::super::state::apply_graph_commit_node_id_mapping(&mut state, &node_id_mapping)
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
            state.apply_persisted_commit_result(result);
        }
        background.sync_needed.store(true, Ordering::Release);
        Ok(crate::AppendSessionNodesResult::Appended {
            node_ids,
            leaf_node_id: committed_leaf_node_id.unwrap_or(locally_derived_leaf_node_id),
        })
    }
}
