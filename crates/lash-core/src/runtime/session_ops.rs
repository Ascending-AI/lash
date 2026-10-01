//! `LashRuntime` session-graph and execution-state operations.
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`;
//! no types live here and no public API is changed.

use crate::SessionId;
use std::sync::Arc;

use crate::{PluginOperationInvokeError, SessionError};

use super::LashRuntime;
use super::state::{append_session_nodes_to_state_with_clock, boundary_operation};

impl LashRuntime {
    /// The fleet-format generation this runtime's durable writers emit — the
    /// `F` the bound session's store recorded (ADR 0106 §1, FIG-3796).
    ///
    /// A runtime holding no store writes nothing durable, so the build's own
    /// generation is the only honest answer it can give.
    pub(super) fn fleet_format(&self) -> crate::FleetFormat {
        self.session
            .as_ref()
            .and_then(|session| session.history_store())
            .map(|store| store.fleet_format())
            .unwrap_or_else(crate::FleetFormat::current)
    }

    /// Replace the host-owned state envelope without durable publication.
    /// Reachable only through the test surface (`apply_persistence_state`).
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn set_persisted_state(
        &mut self,
        state: super::state::RuntimeSessionState,
    ) -> Result<(), SessionError> {
        let mut installed_tool_restore = None;
        if let Some(session) = self.session.as_ref() {
            if let Some(snapshot) = state.plugin_state() {
                session.plugins().hydrate_state(snapshot)?;
            } else if let Some(reference) = state.plugin_state_ref()
                && self.state.plugin_state_ref() != Some(reference)
                && !session.plugins().matches_state_ref(reference)
            {
                return Err(SessionError::Protocol(
                    "bodyless plugin state must match the live session's hydrated checkpoint"
                        .into(),
                ));
            }
            session.invalidate_runtime_caches();
            // Restore the persisted tool catalog so the live registry matches the
            // state being installed (mirrors `from_host_state`). Without this the
            // registry keeps its prior generation/tools and silently diverges from
            // `state`. `restore_state` accepts the snapshot's generation, so a
            // surface that reached generation >= 2 restores cleanly; live
            // changes bump once so the next commit captures them. A
            // `PreservePersisted` open skips this reconcile entirely
            // (FIG-3353): the snapshot stays durable truth untouched.
            if !self.preserves_persisted_tool_state()
                && let Some(tool_state) = state.tool_state_snapshot().cloned()
            {
                let registry = session.plugins().tool_registry();
                let report = crate::runtime::tool_restore::install_persisted_tool_state(
                    registry.as_ref(),
                    tool_state,
                    // A live runtime: the install tolerates and reports,
                    // never refuses (FIG-3367).
                    crate::runtime::tool_restore::ToolRestoreContext::for_live_install(
                        &state.session_id,
                        crate::runtime::ToolRestoreSite::PersistedStateInstall,
                        &self.host.core.tracing,
                        self.host.core.clock.as_ref(),
                    ),
                )?;
                installed_tool_restore = Some(report);
            }
        }
        if installed_tool_restore.is_some() {
            self.tool_restore_report = installed_tool_restore;
        }
        // Whole-state adoption rebuilds the marker field; the install
        // reasserts the per-open `PreservePersisted` claim from host
        // configuration (FIG-3353).
        self.install_resident_state(state)?;
        Ok(())
    }

    /// Append `request`'s nodes to a storeless runtime's session graph.
    ///
    /// A storeless runtime has no durable head and no drive: its `&mut self`
    /// serializes the append with every turn it runs, so the append applies
    /// at once. A store-backed session's head is owned by its bound turn, so
    /// its host appends are
    /// [`SessionCommand::AppendSessionNodes`](crate::SessionCommand::AppendSessionNodes)
    /// commands its drive applies at a turn boundary (FIG-4202); calling this
    /// on one is refused with [`RuntimeErrorCode::SessionCommandRequired`](crate::RuntimeErrorCode::SessionCommandRequired).
    pub async fn append_storeless_session_nodes(
        &mut self,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, SessionError> {
        self.refuse_store_backed_host_write("append_session_nodes")
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        if request.operation_id.trim().is_empty() {
            return Err(SessionError::Protocol(
                "session graph append requires a non-empty stable operation_id".to_string(),
            ));
        }
        if let Some(required_node_id) = request.requires_ancestor_node_id.as_ref()
            && !self
                .state
                .session_graph
                .active_path_contains(required_node_id)
        {
            return Ok(crate::AppendSessionNodesOutcome::StaleBranch {
                required_node_id: required_node_id.clone(),
            });
        }
        let operation = boundary_operation(
            &self.state.session_id,
            &request.operation_id,
            "append-session-nodes",
        );
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
                    crate::plugin::ProtocolSessionContext::new(&session_id, session.fleet_format()),
                    &request.nodes,
                )
                .await?;
        }
        self.stamp_live_plugin_state();
        Ok(crate::AppendSessionNodesOutcome::Appended {
            node_ids,
            leaf_node_id: self
                .state
                .session_graph
                .leaf_node_id
                .clone()
                .unwrap_or_else(|| crate::NodeId::new(String::new())),
        })
    }

    /// Refuse a direct host head write on a store-backed runtime (FIG-4202):
    /// its bound turn owns the head, so the write is a session command.
    pub(super) fn refuse_store_backed_host_write(
        &self,
        operation: &'static str,
    ) -> Result<(), crate::RuntimeError> {
        if self.is_store_backed() {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandRequired,
                format!(
                    "a store-backed session's head is owned by its bound turn: submit \
                     `{operation}` as a session command, which its drive applies at a turn \
                     boundary"
                ),
            ));
        }
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
                &session_id,
                session.fleet_format(),
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
                crate::plugin::ProtocolSessionContext::new(&session_id, session.fleet_format()),
                snapshot,
            )
            .await?;
        self.state
            .set_execution_state_components(crate::plugin::ExecutionStateCapture::from_hydrated(
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
        let store = self.host.core.trigger_store();
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
        let store = self.host.core.trigger_store();
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
        session_id: Option<SessionId>,
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

    /// Run plugin command `name` on a storeless runtime.
    ///
    /// A storeless runtime has no durable head and no drive, so the command
    /// runs at once, under `&mut self`. A store-backed session runs host
    /// plugin commands as
    /// [`SessionCommand::RunPluginCommand`](crate::SessionCommand::RunPluginCommand)
    /// at a turn boundary (FIG-4202); calling this on one is refused with
    /// [`RuntimeErrorCode::SessionCommandRequired`](crate::RuntimeErrorCode::SessionCommandRequired).
    pub async fn run_storeless_plugin_command(
        &mut self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
    ) -> Result<crate::PluginOperationReceipt<serde_json::Value>, PluginOperationInvokeError> {
        self.refuse_store_backed_host_write("run_plugin_command")
            .map_err(|err| PluginOperationInvokeError::Failed(err.to_string()))?;
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
        let events = self.apply_storeless_plugin_operation_effects(
            &plugin_id,
            outcome.events,
            &outcome.directives,
        )?;
        Ok(crate::PluginOperationReceipt {
            output: outcome.output,
            events,
            pending_turn_inputs: Vec::new(),
        })
    }

    /// Run plugin task `name` on a storeless runtime, under
    /// `scoped_effect_controller`; see [`Self::run_storeless_plugin_command`].
    /// A store-backed session runs it as
    /// [`SessionCommand::RunPluginTask`](crate::SessionCommand::RunPluginTask).
    pub async fn run_storeless_plugin_task(
        &mut self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
        scoped_effect_controller: crate::ScopedEffectController<'static>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<crate::PluginOperationReceipt<serde_json::Value>, PluginOperationInvokeError> {
        self.refuse_store_backed_host_write("run_plugin_task")
            .map_err(|err| PluginOperationInvokeError::Failed(err.to_string()))?;
        let manager = self.runtime_session_services()?;
        let Some(session) = self.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
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
        let events = self.apply_storeless_plugin_operation_effects(
            &plugin_id,
            outcome.events,
            &outcome.directives,
        )?;
        Ok(crate::PluginOperationReceipt {
            output: outcome.output,
            events,
            pending_turn_inputs: Vec::new(),
        })
    }

    /// Fold a storeless plugin operation's runtime events into the session
    /// graph. A storeless runtime has no durable queue, so an operation that
    /// queues turns is refused.
    fn apply_storeless_plugin_operation_effects(
        &mut self,
        plugin_id: &str,
        events: Vec<crate::PluginRuntimeEvent>,
        directives: &[crate::PluginRuntimeDirective],
    ) -> Result<Vec<crate::PluginOwned<crate::PluginRuntimeEvent>>, PluginOperationInvokeError>
    {
        if !directives.is_empty() {
            return Err(PluginOperationInvokeError::Failed(
                "a storeless runtime has no durable queue to queue a plugin's turn on".to_string(),
            ));
        }
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
            let operation = boundary_operation(
                &self.state.session_id,
                &uuid::Uuid::new_v4().to_string(),
                "append-plugin-runtime-events",
            );
            let draft_namespace = operation.storage_key().map_err(|err| {
                PluginOperationInvokeError::Failed(format!(
                    "failed to encode plugin runtime event identity: {err}"
                ))
            })?;
            append_session_nodes_to_state_with_clock(
                &mut self.state,
                &nodes,
                &draft_namespace,
                self.host.core.clock.as_ref(),
            );
        }
        self.stamp_live_plugin_state();
        Ok(owned_events)
    }
}
