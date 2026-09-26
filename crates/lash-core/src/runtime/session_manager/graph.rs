use super::*;
use std::sync::atomic::Ordering;

impl CurrentSessionCapability {
    pub(in crate::runtime::session_manager) async fn append_session_nodes(
        &self,
        usage: &UsageCapability,
        background: &ProcessCapability,
        session_id: &SessionId,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, crate::PluginError> {
        if request.operation_id.trim().is_empty() {
            return Err(crate::PluginError::Session(
                "session graph append requires a non-empty stable operation_id".to_string(),
            ));
        }
        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }

        let mut state = match &self.snapshot {
            // A turn-scoped service never commits on its own: the append rides
            // the running turn's draft and lands with the turn's final commit.
            CurrentSnapshot::ReadModel { graph_appends, .. } => {
                return graph_appends.record(session_id, &request);
            }
            CurrentSnapshot::Owned(_) => self.current_snapshot_for_store_write().await?,
        };
        let Some(store) = &self.store else {
            return Err(crate::PluginError::Session(
                "session graph mutation requires a runtime store".to_string(),
            ));
        };
        let operation = super::super::state::boundary_operation(
            &state.session_id,
            &request.operation_id,
            "append-session-nodes",
        );
        // Host-scoped services persist the shared usage ledger with every
        // store write they make.
        debug_assert!(usage.persist_to_store);
        let mut staged_usage = Some(
            usage
                .stage_token_ledger(&mut state, &operation)
                .map_err(|err| crate::PluginError::Session(err.to_string()))?,
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
        let locally_derived_leaf_node_id = graph
            .leaf_node_id()
            .cloned()
            .unwrap_or_else(|| crate::NodeId::new(String::new()));
        let usage_deltas = staged_usage
            .as_ref()
            .map_or(&[][..], |staged| staged.deltas());
        state.capture_plugin_states(&self.plugins);
        let mut commit =
            crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_staged_usage_and_budget(
                &state,
                graph,
                usage_deltas,
                operation,
                self.host.core.durability.commit_budget,
                self.fleet_format(),
            )
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        commit.turn_commit = append_stamp;
        commit.debug_assert_append_envelope_scope();
        let commit_result = super::super::state::commit_in_lane_context(
            self.held_session_execution_lease.as_ref(),
            Arc::clone(store),
            commit,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
            &self.resident_graph_head_stale,
        )
        .await;
        let result = match commit_result {
            Ok(result) => result,
            Err(crate::StoreError::AppendAncestorNotActive { required_node_id }) => {
                return Ok(crate::AppendSessionNodesOutcome::StaleBranch { required_node_id });
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
            Err(crate::StoreError::AppendReceiptRequestedNodeCountCorrupt {
                session_id,
                operation_key,
                stored,
                attempted,
            }) => {
                return Err(crate::PluginError::AppendReceiptRequestedNodeCountCorrupt {
                    session_id,
                    operation_key,
                    stored,
                    attempted,
                });
            }
            Err(crate::StoreError::SessionExecutionLeaseExpired { session_id }) => {
                return Err(crate::PluginError::SessionExecutionLeaseLost { session_id });
            }
            Err(err) => return Err(crate::PluginError::Session(err.to_string())),
        };
        let receipt_replayed = result.receipt_replayed;
        let committed_leaf_node_id = result.committed_leaf_node_id.clone();
        if let Some(staged) = staged_usage.take() {
            staged
                .confirm_identities(&result.committed_usage_delta_identities)
                .map_err(super::usage::plugin_error_from_usage_confirmation)?;
        }
        let node_ids =
            super::super::state::resolve_append_node_ids(&result, locally_derived_node_ids)
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        if !receipt_replayed {
            super::super::state::apply_graph_commit_node_id_mapping(&mut state, &node_id_mapping)
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
            state.apply_persisted_commit_result(result);
        }
        background.sync_needed.store(true, Ordering::Release);
        Ok(crate::AppendSessionNodesOutcome::Appended {
            node_ids,
            leaf_node_id: committed_leaf_node_id.unwrap_or(locally_derived_leaf_node_id),
        })
    }
    pub(in crate::runtime::session_manager) async fn switch_agent_frame(
        &self,
        session_id: &SessionId,
        request: &crate::SwitchAgentFrameRequest,
    ) -> Result<crate::OpenAgentFrameResult, crate::PluginError> {
        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }
        match &self.snapshot {
            // A turn-scoped service never commits on its own: the switch rides the
            // running turn's draft and materializes with the turn's final commit.
            CurrentSnapshot::ReadModel {
                graph_appends,
                meta,
                ..
            } => graph_appends.record_frame_switch(
                session_id,
                meta.current_frame_node_id.as_deref(),
                request,
            ),
            _ => Err(crate::PluginError::Session(format!(
                "agent-frame switch requires the running session's turn scope; session `{session_id}` has no live turn draft"
            ))),
        }
    }
}
