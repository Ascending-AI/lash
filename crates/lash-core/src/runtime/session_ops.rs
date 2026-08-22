//! `LashRuntime` session-graph and execution-state operations.
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`;
//! no types live here and no public API is changed.

use crate::facade_support::ScopedEffectControllerFacadeOps;
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
            if let Some(tool_state) = state.tool_state_snapshot().cloned() {
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
            let snapshot = state.plugin_snapshot().cloned().unwrap_or_default();
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

    /// Runs a lane-less host append between turn drivers, never concurrently with a running turn.
    pub async fn append_session_nodes(
        &mut self,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        if request.operation_id.trim().is_empty() {
            return Err(SessionError::Protocol(
                "session graph append requires a non-empty stable operation_id".to_string(),
            ));
        }
        self.refresh_session_graph_from_store().await?;
        let history_store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store());
        if history_store.is_none()
            && let Some(required_node_id) = request.requires_ancestor_node_id.as_deref()
            && !self
                .state
                .session_graph
                .active_path_contains(required_node_id)
        {
            return Ok(crate::AppendSessionNodesOutcome::StaleBranch {
                required_node_id: required_node_id.to_string(),
            });
        }
        let operation = boundary_operation(
            &self.state.session_id,
            &request.operation_id,
            "append-session-nodes",
        );
        let append_stamp = crate::RuntimeTurnCommitStamp::append_session_nodes(
            operation.clone(),
            request.requires_ancestor_node_id.as_deref(),
            &request.nodes,
        )
        .map_err(|err| SessionError::Protocol(err.to_string()))?;
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
        if let Some(store) = history_store {
            let requested_node_count = node_ids.len();
            let mut graph = self.state.pending_graph_commit();
            let node_id_mapping = match graph.derive_node_ids(&self.state.session_id, &operation) {
                Ok(mapping) => mapping,
                Err(source) => {
                    let mut context =
                        "failed to derive persisted session graph node identities".to_string();
                    if let Err(rollback_err) = self
                        .restore_protocol_session_from_state(state_before_append)
                        .await
                    {
                        context.push_str(&format!(
                            "; failed to restore protocol session: {rollback_err}"
                        ));
                    }
                    return Err(SessionError::Store { context, source });
                }
            };
            let persisted_node_ids = node_id_mapping
                .iter()
                .map(|(_, derived)| derived.clone())
                .collect::<Vec<_>>();
            let locally_derived_node_ids = persisted_node_ids[persisted_node_ids
                .len()
                .saturating_sub(requested_node_count)..]
                .to_vec();
            let locally_derived_leaf_node_id = graph.leaf_node_id.clone().unwrap_or_default();
            let mut commit =
                crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
                    &self.state,
                    graph,
                    &[],
                    operation,
                    self.host.core.durability.commit_budget,
                )
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            commit.turn_commit = append_stamp;
            commit.debug_assert_append_envelope_scope();
            let _pre_commit_phase = super::RuntimeNamedPhase::begin(
                self.turn_phase_probe.clone(),
                "session_graph_append.pre_commit",
            );
            // Lane-less public runtime operation: callers append between turn
            // drivers, so this handle owns no retained execution guard.
            //
            // Structurally excluded from `state::commit_in_lane_context`: this site
            // is strictly lane-less (never carries a `BorrowedLaneAuthority`) and
            // interleaves in-memory protocol session rollback
            // (`restore_protocol_session_from_state`) on commit failure or
            // `AppendAncestorNotActive` stale-branch response.
            let result = match super::commit_runtime_state_with_fresh_session_execution_lease(
                Arc::clone(&store),
                commit,
                &self.runtime_lease_owner,
                &self.runtime_lease_executor_id,
                self.host.core.control.lease_timings,
                Arc::clone(&self.host.core.clock),
            )
            .await
            {
                Ok(result) => result,
                Err(crate::StoreError::AppendAncestorNotActive { required_node_id }) => {
                    if let Err(rollback_err) = self
                        .restore_protocol_session_from_state(state_before_append)
                        .await
                    {
                        return Err(SessionError::Protocol(format!(
                            "append requires inactive ancestor `{required_node_id}`; failed to restore pre-append protocol session: {rollback_err}"
                        )));
                    }
                    return Ok(crate::AppendSessionNodesOutcome::StaleBranch { required_node_id });
                }
                Err(err) => {
                    if let Err(rollback_err) = self
                        .restore_protocol_session_from_state(state_before_append)
                        .await
                    {
                        let context = format!(
                            "failed to persist runtime state; failed to restore protocol session: \
                             {rollback_err}"
                        );
                        return Err(super::session_commit_error(&context, err));
                    }
                    return Err(super::session_commit_error(
                        "failed to persist runtime state",
                        err,
                    ));
                }
            };
            let receipt_replayed = result.receipt_replayed;
            let committed_leaf_node_id = result.committed_leaf_node_id.clone();
            let node_ids = if receipt_replayed {
                match super::state::receipt_append_node_ids(&result, requested_node_count) {
                    Ok(node_ids) => node_ids,
                    Err(source) => {
                        let mut context =
                            "append receipt contains an invalid stored node-id result".to_string();
                        if let Err(rollback_err) = self
                            .restore_protocol_session_from_state(state_before_append.clone())
                            .await
                        {
                            context.push_str(&format!(
                                "; failed to restore pre-append protocol session: {rollback_err}"
                            ));
                        }
                        return Err(SessionError::Store { context, source });
                    }
                }
            } else {
                locally_derived_node_ids
            };
            if receipt_replayed {
                let mut durable_state = state_before_append.clone();
                if let Err(source) = crate::store::refresh_persisted_session_state(
                    store.as_ref(),
                    &mut durable_state,
                )
                .await
                {
                    let mut context =
                        "failed to refresh resident state after append receipt replay".to_string();
                    if let Err(rollback_err) = self
                        .restore_protocol_session_from_state(state_before_append)
                        .await
                    {
                        context.push_str(&format!(
                            "; failed to restore pre-append protocol session: {rollback_err}"
                        ));
                    }
                    return Err(SessionError::Store { context, source });
                }
                self.restore_protocol_session_from_state(durable_state)
                    .await?;
            } else {
                super::state::apply_graph_commit_node_id_mapping(&mut self.state, &node_id_mapping)
                    .map_err(|source| SessionError::Store {
                        context: "failed to apply persisted session graph node identities"
                            .to_string(),
                        source,
                    })?;
                self.state.apply_persisted_commit_result(result);
                self.state.mark_node_ids_persisted(persisted_node_ids);
            }
            return Ok(crate::AppendSessionNodesOutcome::Appended {
                node_ids,
                leaf_node_id: committed_leaf_node_id.unwrap_or(locally_derived_leaf_node_id),
            });
        }
        Ok(crate::AppendSessionNodesOutcome::Appended {
            node_ids,
            leaf_node_id: self
                .state
                .session_graph
                .leaf_node_id
                .clone()
                .unwrap_or_default(),
        })
    }

    async fn restore_protocol_session_from_state(
        &mut self,
        state_before_append: RuntimeSessionState,
    ) -> Result<(), SessionError> {
        self.state = state_before_append;
        self.policy = self.state.policy.clone();
        self.protocol_turn_options = self.state.protocol_turn_options.clone();
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
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
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
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
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
        // Extraction is transactional: the registry entry is only surrendered
        // once the handle has actually yielded its runtime. `try_into_runtime`
        // hands the intact handle back in `Err`, so the still-in-use case
        // restores it under the same lock — a failed activation must stay
        // retryable instead of ghosting the child until a cold reopen.
        let child = {
            let mut registry = self.managed_sessions.lock().await;
            let registered = registry.len();
            let Some(handle) = registry.remove(session_id) else {
                tracing::debug!(
                    session_id,
                    managed_sessions = registered,
                    consulted = "managed_session_registry",
                    outcome = "unknown_session",
                    event = "managed_session.activation",
                    "managed session activation denied: not registered"
                );
                return Err(SessionError::Protocol(format!(
                    "unknown managed session `{session_id}`"
                )));
            };
            // Reference count observed while the registry lock is held: the
            // extraction below can only succeed at 1, so this is the input that
            // decides the outcome.
            let runtime_references = handle.runtime_reference_count();
            match handle.try_into_runtime() {
                Ok(child) => {
                    tracing::debug!(
                        session_id,
                        managed_sessions = registered,
                        runtime_references,
                        consulted = "managed_session_handle_references",
                        outcome = "activated",
                        event = "managed_session.activation",
                        "managed session activated"
                    );
                    child
                }
                Err(handle) => {
                    let runtime_references_on_refusal = handle.runtime_reference_count();
                    registry.insert(session_id.to_string(), handle);
                    tracing::debug!(
                        session_id,
                        managed_sessions = registered,
                        runtime_references,
                        runtime_references_on_refusal,
                        consulted = "managed_session_handle_references",
                        outcome = "in_use_handle_restored",
                        event = "managed_session.activation",
                        "managed session activation denied: the runtime is still referenced \
                         elsewhere; its registration was restored and activation stays retryable"
                    );
                    return Err(SessionError::Protocol(format!(
                        "managed session `{session_id}` is still in use"
                    )));
                }
            }
        };
        *self = child;
        Ok(())
    }

    /// Explicitly snapshot protocol-local execution state, including leaf bodies, if any.
    ///
    /// This reads the executor's complete live state and stages nothing. A turn's
    /// capture is a checkpoint delta whose unchanged leaves ride as body-free
    /// refs, and the runtime releases their resident bodies once the durable refs
    /// are authoritative — so reassembling a portable snapshot out of resident
    /// checkpoint state would be both incomplete and a capture this path cannot
    /// honestly acknowledge, because it writes nothing durable.
    pub async fn snapshot_execution_state(
        &mut self,
    ) -> Result<Option<crate::plugin::HydratedExecutionState>, SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
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
            .hydrated_execution_state(crate::plugin::ProtocolSessionContext::new(
                session,
                &session_id,
            ))
            .await
    }

    /// Explicitly restore protocol-local execution state from a hydrated snapshot.
    pub async fn restore_execution_state(
        &mut self,
        snapshot: &crate::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
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
        self.state
            .set_execution_state_components(crate::plugin::ExecutionStateSnapshot::from_hydrated(
                snapshot.clone(),
            ))
            .map_err(|source| SessionError::Store {
                context: "failed to stage restored execution-state components".to_string(),
                source,
            })?;
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
        &mut self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
    ) -> Result<(String, serde_json::Value), PluginOperationInvokeError> {
        self.reload_invalidated_resident_session_state()
            .await
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
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

    /// Runs a lane-less plugin command between turn drivers, never concurrently with a running turn.
    pub async fn run_plugin_command(
        &mut self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        operation_scope: crate::ExecutionScope,
    ) -> Result<crate::PluginCommandReceipt<serde_json::Value>, PluginOperationInvokeError> {
        self.reload_invalidated_resident_session_state()
            .await
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
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

    /// Runs a lane-less plugin task between turn drivers, never concurrently with a running turn.
    pub async fn run_plugin_task(
        &mut self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        scoped_effect_controller: crate::ScopedEffectController<'static>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<crate::PluginTaskReceipt<serde_json::Value>, PluginOperationInvokeError> {
        self.reload_invalidated_resident_session_state()
            .await
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
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
                match derive_graph_commit_node_ids(&mut self.state, &mut graph, &operation) {
                    Ok(node_ids) => node_ids,
                    Err(err) => {
                        let mut context =
                            format!("failed to derive plugin runtime event identity: {err}");
                        if let Err(rollback_err) = self
                            .restore_protocol_session_from_state(state_before_append.clone())
                            .await
                        {
                            context.push_str(&format!(
                                "; failed to restore protocol session: {rollback_err}"
                            ));
                        }
                        return Err(PluginOperationInvokeError::Failed(context));
                    }
                };
            let commit =
                crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
                    &self.state,
                    graph,
                    &[],
                    operation,
                    self.host.core.durability.commit_budget,
                )
                .map_err(|err| {
                    PluginOperationInvokeError::Failed(format!(
                        "failed to hash plugin runtime events: {err}"
                    ))
                })?;
            // Lane-less host plugin-operation boundary. In-turn lifecycle
            // graph appends use `session_manager::graph` and carry an explicit
            // borrowed guard instead of reaching this runtime-owned path.
            let result = match super::commit_runtime_state_with_fresh_session_execution_lease(
                store,
                commit,
                &self.runtime_lease_owner,
                &self.runtime_lease_executor_id,
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
                        .restore_protocol_session_from_state(state_before_append)
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
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                &[],
                operation,
                self.host.core.durability.commit_budget,
            )
            .map_err(|err| {
                PluginOperationInvokeError::Failed(format!(
                    "failed to identify plugin operation state: {err}"
                ))
            })?;
        // Lane-less host plugin-operation snapshot. Turn-scoped service calls
        // are classified at the session-manager call sites instead.
        let result = super::commit_runtime_state_with_fresh_session_execution_lease(
            store,
            commit,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
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
