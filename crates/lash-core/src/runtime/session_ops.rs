//! `LashRuntime` session-graph and execution-state operations.
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`;
//! no types live here and no public API is changed.

use std::sync::Arc;

use crate::{PluginOperationInvokeError, SessionError};

use super::LashRuntime;
use super::state::{
    RuntimeSessionState, append_session_nodes_to_state_with_clock, boundary_operation,
    derive_graph_commit_node_ids,
};

impl LashRuntime {
    /// Replace the host-owned state envelope.
    pub fn set_persisted_state(&mut self, state: RuntimeSessionState) -> Result<(), SessionError> {
        let mut state = state;
        if let Some(session) = self.session.as_ref() {
            session.invalidate_runtime_caches();
            // Restore the persisted tool catalog so the live registry matches the
            // state being installed (mirrors `from_host_state`). Without this the
            // registry keeps its prior generation/tools and silently diverges from
            // `state`. `restore_state` accepts the snapshot's generation, so a
            // surface that reached generation >= 2 restores cleanly; live
            // changes bump once so the next commit captures them.
            if let Some(tool_state) = state.tool_state_snapshot.clone() {
                let report = session
                    .plugins()
                    .tool_registry()
                    .restore_state(tool_state)
                    .map_err(|err| SessionError::Protocol(err.to_string()))?;
                if !report.orphaned.is_empty() {
                    tracing::warn!(
                        session_id = %state.session_id,
                        orphaned = ?report.orphaned,
                        "persisted state installed with orphaned tools: no registered \
                         source resolves them; they remain non-members until their source returns"
                    );
                }
            }
            let snapshot = state.plugin_snapshot.clone().unwrap_or_default();
            session
                .plugins()
                .restore(&snapshot)
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            state.plugin_snapshot_revision =
                Some(session.plugins().snapshot_revision_fingerprint());
        }
        self.policy = state.policy.clone();
        self.protocol_turn_options = state.protocol_turn_options.clone();
        self.state = state;
        Ok(())
    }

    pub async fn append_session_nodes(
        &mut self,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesResult, SessionError> {
        if request.operation_id.trim().is_empty() {
            return Err(SessionError::Protocol(
                "session graph append requires a non-empty stable operation_id".to_string(),
            ));
        }
        self.refresh_session_graph_from_store().await?;
        if let Some(required) = request.requires_ancestor_node_id.as_deref()
            && !self.state.session_graph.active_path_contains(required)
        {
            return Ok(crate::AppendSessionNodesResult::StaleBranch {
                current_leaf_node_id: self.state.session_graph.leaf_node_id.clone(),
            });
        }
        let operation = boundary_operation(
            &self.state.session_id,
            &request.operation_id,
            "append-session-nodes",
        );
        let state_before_append = self.state.clone();
        let draft_namespace = operation
            .storage_key()
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        let node_ids = append_session_nodes_to_state_with_clock(
            &mut self.state,
            &request.nodes,
            &draft_namespace,
            self.host.core.clock.as_ref(),
        );
        if let Some(session) = self.session.as_mut() {
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = self.state.session_id.clone();
            protocol_session
                .append_session_nodes(
                    crate::plugin::ProtocolSessionContext::new(session, &session_id),
                    &request.nodes,
                )
                .await?;
        }
        self.stamp_live_plugin_state();
        if let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        {
            let requested_node_count = node_ids.len();
            let mut graph = self.state.pending_graph_commit();
            let persisted_node_ids =
                derive_graph_commit_node_ids(&mut self.state, &mut graph, &operation)
                    .map_err(|err| SessionError::Protocol(err.to_string()))?;
            let node_ids = persisted_node_ids[persisted_node_ids
                .len()
                .saturating_sub(requested_node_count)..]
                .to_vec();
            let commit =
                crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
                    &self.state,
                    graph,
                    &[],
                    operation,
                )
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            let result = match super::commit_runtime_state_with_fresh_session_execution_lease(
                store,
                commit,
                &self.runtime_lease_owner,
                self.host.core.control.lease_timings,
                Arc::clone(&self.host.core.clock),
            )
            .await
            {
                Ok(result) => result,
                Err(err) => {
                    let persistence_error = format!("failed to persist runtime state: {err}");
                    if let Err(rollback_err) = self
                        .restore_protocol_session_after_failed_append(state_before_append)
                        .await
                    {
                        return Err(SessionError::Protocol(format!(
                            "{persistence_error}; failed to restore protocol session: \
                             {rollback_err}"
                        )));
                    }
                    return Err(SessionError::Protocol(persistence_error));
                }
            };
            self.state.apply_persisted_commit_result(result);
            self.state.mark_node_ids_persisted(persisted_node_ids);
            return Ok(crate::AppendSessionNodesResult::Appended {
                node_ids,
                leaf_node_id: self
                    .state
                    .session_graph
                    .leaf_node_id
                    .clone()
                    .unwrap_or_default(),
            });
        }
        Ok(crate::AppendSessionNodesResult::Appended {
            node_ids,
            leaf_node_id: self
                .state
                .session_graph
                .leaf_node_id
                .clone()
                .unwrap_or_default(),
        })
    }

    async fn restore_protocol_session_after_failed_append(
        &mut self,
        state_before_append: RuntimeSessionState,
    ) -> Result<(), SessionError> {
        self.state = state_before_append;
        let state_for_restore = self.state.clone();
        if let Some(session) = self.session.as_mut() {
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = state_for_restore.session_id.clone();
            protocol_session
                .restore_session(
                    crate::plugin::ProtocolSessionContext::new(session, &session_id),
                    &state_for_restore,
                )
                .await?;
        }
        self.stamp_live_plugin_state();
        Ok(())
    }

    pub async fn apply_protocol_session_extension(
        &mut self,
        extension: crate::ProtocolSessionExtensionHandle,
    ) -> Result<(), SessionError> {
        let Some(session) = self.session.as_ref() else {
            return Err(SessionError::Protocol(
                "runtime session is not available".to_string(),
            ));
        };
        let protocol_session = Arc::clone(session.plugins().protocol_session());
        protocol_session.apply_session_extension(extension).await
    }

    pub async fn validate_protocol_turn_extension(
        &mut self,
        extension: &crate::ProtocolTurnExtensionHandle,
    ) -> Result<(), SessionError> {
        let Some(session) = self.session.as_ref() else {
            return Err(SessionError::Protocol(
                "runtime session is not available".to_string(),
            ));
        };
        let protocol_session = Arc::clone(session.plugins().protocol_session());
        protocol_session.validate_turn_extension(extension).await
    }

    /// Promote a managed child session into the foreground runtime.
    ///
    /// Child sessions created through `SessionLifecycleService::create_session` are real
    /// runtimes, not serialized placeholders. Foreground activation must therefore
    /// claim that runtime instead of reconstructing a new empty state in the UI.
    pub async fn activate_managed_session(&mut self, session_id: &str) -> Result<(), SessionError> {
        let child = {
            let mut registry = self.managed_sessions.lock().await;
            registry.remove(session_id).ok_or_else(|| {
                SessionError::Protocol(format!("unknown managed session `{session_id}`"))
            })?
        };
        let child = child.try_into_runtime().map_err(|_| {
            SessionError::Protocol(format!("managed session `{session_id}` is still in use"))
        })?;
        *self = child;
        Ok(())
    }

    /// Explicitly snapshot protocol-local execution state, if any.
    pub async fn snapshot_execution_state(&mut self) -> Result<Option<Vec<u8>>, SessionError> {
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        let code_executor = session
            .plugins()
            .code_executor()
            .ok_or(SessionError::CodeExecutionUnavailable)?;
        let session_id = self.state.session_id.clone();
        let blob = code_executor
            .snapshot_execution_state(crate::plugin::ProtocolSessionContext::new(
                session,
                &session_id,
            ))
            .await?;
        self.state.execution_state_snapshot = blob.clone();
        Ok(blob)
    }

    /// Explicitly restore protocol-local execution state from an opaque snapshot blob.
    pub async fn restore_execution_state(&mut self, snapshot: &[u8]) -> Result<(), SessionError> {
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        let code_executor = session
            .plugins()
            .code_executor()
            .ok_or(SessionError::CodeExecutionUnavailable)?;
        let session_id = self.state.session_id.clone();
        code_executor
            .restore_execution_state(
                crate::plugin::ProtocolSessionContext::new(session, &session_id),
                snapshot,
            )
            .await?;
        self.state.execution_state_snapshot = Some(snapshot.to_vec());
        Ok(())
    }

    pub async fn list_trigger_registrations(
        &self,
    ) -> Result<Vec<crate::TriggerRegistration>, SessionError> {
        let store = self.host.trigger_store.as_ref().ok_or_else(|| {
            SessionError::Protocol("trigger store is unavailable in this runtime".to_string())
        })?;
        let records = store
            .list_subscriptions(crate::TriggerSubscriptionFilter::for_session(
                self.state.session_id.clone(),
            ))
            .await
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        Ok(records
            .iter()
            .map(crate::TriggerRegistration::from)
            .collect())
    }

    pub async fn trigger_registrations_by_source_type(
        &self,
        source_type: impl Into<crate::TriggerEventType>,
    ) -> Result<Vec<crate::TriggerRegistration>, SessionError> {
        let store = self.host.trigger_store.as_ref().ok_or_else(|| {
            SessionError::Protocol("trigger store is unavailable in this runtime".to_string())
        })?;
        let mut filter =
            crate::TriggerSubscriptionFilter::for_session(self.state.session_id.clone());
        filter.source_type = Some(source_type.into().to_string());
        let records = store
            .list_subscriptions(filter)
            .await
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        Ok(records
            .iter()
            .map(crate::TriggerRegistration::from)
            .collect())
    }

    pub async fn query_plugin(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
    ) -> Result<(String, serde_json::Value), PluginOperationInvokeError> {
        let manager = self.runtime_session_services()?;
        let Some(session) = self.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
        session
            .plugins()
            .query_plugin(
                name,
                args,
                session_id,
                true,
                manager.read_service(),
                manager.process_read_service(),
            )
            .await
    }

    pub async fn run_plugin_command(
        &mut self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        operation_scope: crate::ExecutionScope,
    ) -> Result<crate::PluginCommandReceipt<serde_json::Value>, PluginOperationInvokeError> {
        let manager = self.runtime_session_services()?;
        let Some(session) = self.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
        let (plugin_id, outcome) = session
            .plugins()
            .run_plugin_command(
                name,
                args,
                session_id,
                true,
                manager.state_service(),
                manager.lifecycle_service(),
                manager.graph_service(),
                manager.process_service(),
            )
            .await?;
        let (events, pending_turn_inputs) = self
            .apply_plugin_operation_effects(
                &plugin_id,
                outcome.events,
                outcome.directives,
                operation_scope,
            )
            .await?;
        Ok(crate::PluginCommandReceipt {
            output: outcome.output,
            events,
            pending_turn_inputs,
        })
    }

    pub async fn run_plugin_task(
        &mut self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        scoped_effect_controller: crate::ScopedEffectController<'static>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<crate::PluginTaskReceipt<serde_json::Value>, PluginOperationInvokeError> {
        let manager = self.runtime_session_services()?;
        let Some(session) = self.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
        let operation_scope = scoped_effect_controller.execution_scope().clone();
        let (plugin_id, outcome) = session
            .plugins()
            .run_plugin_task(
                name,
                args,
                session_id,
                true,
                manager.state_service(),
                manager.lifecycle_service(),
                manager.graph_service(),
                manager.process_service(),
                scoped_effect_controller,
                cancellation_token,
            )
            .await?;
        let (events, pending_turn_inputs) = self
            .apply_plugin_operation_effects(
                &plugin_id,
                outcome.events,
                outcome.directives,
                operation_scope,
            )
            .await?;
        Ok(crate::PluginTaskReceipt {
            output: outcome.output,
            events,
            pending_turn_inputs,
        })
    }

    async fn apply_plugin_operation_effects(
        &mut self,
        plugin_id: &str,
        events: Vec<crate::PluginRuntimeEvent>,
        directives: Vec<crate::PluginRuntimeDirective>,
        operation_scope: crate::ExecutionScope,
    ) -> Result<
        (
            Vec<crate::PluginOwned<crate::PluginRuntimeEvent>>,
            Vec<crate::PendingTurnInput>,
        ),
        PluginOperationInvokeError,
    > {
        let owned_events = events
            .into_iter()
            .map(|event| crate::PluginOwned {
                plugin_id: plugin_id.to_string(),
                value: event,
            })
            .collect::<Vec<_>>();
        if !owned_events.is_empty() {
            let nodes = owned_events
                .iter()
                .map(|owned| {
                    crate::plugin_runtime_protocol_event(&owned.plugin_id, owned.value.clone())
                        .map(crate::SessionAppendNode::protocol_event)
                        .map_err(|err| {
                            PluginOperationInvokeError::Failed(format!(
                                "failed to encode plugin runtime event: {err}"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.append_plugin_runtime_event_nodes(&nodes, operation_scope.clone())
                .await?;
        }
        self.stamp_live_plugin_state();
        self.persist_plugin_operation_state_if_needed(operation_scope)
            .await?;

        let mut pending_turn_inputs = Vec::new();
        for directive in directives {
            match directive {
                crate::PluginRuntimeDirective::QueueTurn { input, source_key } => {
                    let pending = self
                        .enqueue_turn_input(input, crate::TurnInputIngress::NextTurn, source_key)
                        .await
                        .map_err(|err| {
                            PluginOperationInvokeError::Failed(format!(
                                "failed to queue plugin turn request: {err}"
                            ))
                        })?;
                    pending_turn_inputs.push(pending);
                }
            }
        }

        Ok((owned_events, pending_turn_inputs))
    }

    async fn append_plugin_runtime_event_nodes(
        &mut self,
        nodes: &[crate::SessionAppendNode],
        operation_scope: crate::ExecutionScope,
    ) -> Result<(), PluginOperationInvokeError> {
        let operation = crate::OperationId::new(operation_scope, "append-plugin-runtime-events");
        let state_before_append = self.state.clone();
        let draft_namespace = operation.storage_key().map_err(|err| {
            PluginOperationInvokeError::Failed(format!(
                "failed to encode plugin runtime event identity: {err}"
            ))
        })?;
        append_session_nodes_to_state_with_clock(
            &mut self.state,
            nodes,
            &draft_namespace,
            self.host.core.clock.as_ref(),
        );
        if let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        {
            let mut graph = self.state.pending_graph_commit();
            let persisted_node_ids =
                derive_graph_commit_node_ids(&mut self.state, &mut graph, &operation).map_err(
                    |err| {
                        PluginOperationInvokeError::Failed(format!(
                            "failed to derive plugin runtime event identity: {err}"
                        ))
                    },
                )?;
            let commit =
                crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
                    &self.state,
                    graph,
                    &[],
                    operation,
                )
                .map_err(|err| {
                    PluginOperationInvokeError::Failed(format!(
                        "failed to hash plugin runtime events: {err}"
                    ))
                })?;
            let result = match super::commit_runtime_state_with_fresh_session_execution_lease(
                store,
                commit,
                &self.runtime_lease_owner,
                self.host.core.control.lease_timings,
                Arc::clone(&self.host.core.clock),
            )
            .await
            {
                Ok(result) => result,
                Err(err) => {
                    let persistence_error =
                        format!("failed to persist plugin runtime events: {err}");
                    if let Err(rollback_err) = self
                        .restore_protocol_session_after_failed_append(state_before_append)
                        .await
                    {
                        return Err(PluginOperationInvokeError::Failed(format!(
                            "{persistence_error}; failed to restore protocol session: \
                             {rollback_err}"
                        )));
                    }
                    return Err(PluginOperationInvokeError::Failed(persistence_error));
                }
            };
            self.state.apply_persisted_commit_result(result);
            self.state.mark_node_ids_persisted(persisted_node_ids);
        }
        Ok(())
    }

    async fn persist_plugin_operation_state_if_needed(
        &mut self,
        operation_scope: crate::ExecutionScope,
    ) -> Result<(), PluginOperationInvokeError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(());
        };
        let operation = crate::OperationId::new(operation_scope, "plugin-operation-state");
        let (commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation(
                &mut self.state,
                &[],
                operation,
            )
            .map_err(|err| {
                PluginOperationInvokeError::Failed(format!(
                    "failed to identify plugin operation state: {err}"
                ))
            })?;
        let result = super::commit_runtime_state_with_fresh_session_execution_lease(
            store,
            commit,
            &self.runtime_lease_owner,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(|err| {
            PluginOperationInvokeError::Failed(format!(
                "failed to persist plugin operation state: {err}"
            ))
        })?;
        self.state.apply_persisted_commit_result(result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        Ok(())
    }
}
