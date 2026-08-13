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
                .map_err(plugin_error_from_session_append)?;
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
        let operation = super::super::state::boundary_operation(
            &state.session_id,
            &request.operation_id,
            "append-session-nodes",
        );
        let mut staged_usage = if usage.persist_to_store {
            Some(
                usage
                    .stage_token_ledger(&mut state, &operation)
                    .map_err(|err| crate::PluginError::Session(err.to_string()))?,
            )
        } else {
            None
        };
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
            crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_staged_usage_and_budget(
                &state,
                graph,
                usage_deltas,
                operation,
                self.host.core.durability.commit_budget,
            )
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        commit.turn_commit = append_stamp;
        commit.debug_assert_append_envelope_scope();
        // Dual-context site: TurnPersisted observers run under the parent
        // turn's held lane, while host-created graph services are lane-less.
        // Keep the distinction explicit so timing can never select authority.
        let commit_result = if let Some(lease) = &self.held_session_execution_lease {
            let result = commit_runtime_state_with_borrowed_lease(
                lease,
                Arc::clone(store),
                commit,
                &self.runtime_lease_owner,
            )
            .await;
            if result.is_ok() {
                // The guard remains current, but this service committed from a
                // snapshot outside the owning runtime. Force a deliberate head
                // reload before its next physical turn; planner CAS is not the
                // graph-freshness discovery mechanism.
                self.resident_graph_head_stale
                    .store(true, Ordering::Release);
            }
            result
        } else {
            commit_runtime_state_with_fresh_session_execution_lease(
                Arc::clone(store),
                commit,
                &self.runtime_lease_owner,
                &self.runtime_lease_executor_id,
                self.host.core.control.lease_timings,
                Arc::clone(&self.host.core.clock),
            )
            .await
        };
        let result = match commit_result {
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

fn plugin_error_from_session_append(error: crate::SessionError) -> crate::PluginError {
    match error {
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
        crate::SessionError::Store {
            source:
                crate::StoreError::AppendReceiptRequestedNodeCountCorrupt {
                    session_id,
                    operation_key,
                    stored,
                    attempted,
                },
            ..
        } => crate::PluginError::AppendReceiptRequestedNodeCountCorrupt {
            session_id,
            operation_key,
            stored,
            attempted,
        },
        error => crate::PluginError::Session(error.to_string()),
    }
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;

    #[test]
    fn append_receipt_count_corruption_remains_typed_at_plugin_boundary() {
        let error = plugin_error_from_session_append(crate::SessionError::Store {
            context: "append receipt".to_string(),
            source: crate::StoreError::AppendReceiptRequestedNodeCountCorrupt {
                session_id: "root".to_string(),
                operation_key: "append-operation".to_string(),
                stored: Some(1),
                attempted: Some(2),
            },
        });
        assert!(matches!(
            error,
            crate::PluginError::AppendReceiptRequestedNodeCountCorrupt {
                ref session_id,
                ref operation_key,
                stored: Some(1),
                attempted: Some(2),
            } if session_id == "root" && operation_key == "append-operation"
        ));
    }
}
