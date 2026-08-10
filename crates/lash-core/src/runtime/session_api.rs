use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::sync::MutexExt;

impl LashRuntime {
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    pub(super) fn stamp_live_plugin_state(&mut self) {
        if let Some(session) = self.session.as_ref() {
            let snapshot = session.plugins().tool_registry().export_state();
            self.state.set_tool_state_snapshot(Some(snapshot));
            let captured = session.plugins().snapshot();
            crate::runtime::state::store_plugin_snapshot(&mut self.state, captured);
            self.state.plugin_snapshot_revision =
                Some(session.plugins().snapshot_revision_fingerprint());
        } else {
            self.state.set_tool_state_snapshot(None);
            self.state.set_plugin_snapshot(None);
            self.state.plugin_snapshot_revision = None;
        }
    }
    pub(super) fn active_tool_catalog_shared(
        &self,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        if !self.resident_session_state_valid {
            self.trace_synchronous_resident_state_refusal("active_tool_catalog_shared");
            return Err(crate::PluginError::Session(
                "resident session state is invalidated; durable reload is required".to_string(),
            ));
        }
        self.session
            .as_ref()
            .map(|session| session.shared_tool_catalog(&self.state.session_id))
            .unwrap_or_else(|| Ok(Arc::new(Vec::new())))
    }

    pub fn tool_state(&self) -> Result<crate::ToolState, SessionError> {
        if !self.resident_session_state_valid {
            self.trace_synchronous_resident_state_refusal("tool_state");
            return Err(SessionError::Protocol(
                "resident session state is invalidated; durable reload is required".to_string(),
            ));
        }
        let Some(session) = self.session.as_ref() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        Ok(session.plugins().tool_registry().export_state())
    }
    /// Override protocol-owned turn options for this session.
    pub fn set_protocol_turn_options(&mut self, options: crate::ProtocolTurnOptions) {
        self.state.protocol_turn_options = options.clone();
        self.protocol_turn_options = options;
    }

    /// The durable protocol turn options recorded on the session.
    pub fn protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        &self.state.protocol_turn_options
    }

    /// Override protocol-owned turn options during materialization.
    ///
    /// Existing `FrameOpen` nodes are immutable historical snapshots; the next
    /// opened frame captures this live value.
    pub fn set_protocol_turn_options_all_frames(&mut self, options: crate::ProtocolTurnOptions) {
        self.state.protocol_turn_options = options.clone();
        self.protocol_turn_options = options;
    }

    /// Run the protocol plugin's materialization hook against this runtime.
    ///
    /// Fires the
    /// [`ProtocolSessionPlugin::configure_runtime_on_materialize`](crate::plugin::ProtocolSessionPlugin::configure_runtime_on_materialize)
    /// hook, so both the child-create path and the root/builder-open path
    /// converge on one seam. `plugin_options` are the plugin-keyed options that
    /// reached this materialization (builder options for root opens, request
    /// options for child create); `is_root_session` distinguishes root from
    /// child.
    pub fn configure_protocol_on_materialize(
        &mut self,
        plugin_options: &crate::PluginOptions,
        is_root_session: bool,
    ) -> Result<(), crate::PluginError> {
        if !self.resident_session_state_valid {
            self.trace_synchronous_resident_state_refusal("configure_protocol_on_materialize");
            return Err(crate::PluginError::Session(
                "resident session state is invalidated; durable reload is required".to_string(),
            ));
        }
        let protocol_session = self
            .session
            .as_ref()
            .map(|session| Arc::clone(session.plugins().protocol_session()));
        if let Some(protocol_session) = protocol_session {
            let materialization = crate::plugin::ProtocolSessionMaterialization {
                plugin_options,
                is_root_session,
            };
            protocol_session
                .configure_runtime_on_materialize(
                    crate::plugin::ProtocolRuntimeContext::new(self),
                    materialization,
                )
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        }
        Ok(())
    }

    /// Export a snapshot of the current in-memory session state.
    /// This keeps persistence-heavy snapshots untouched; callers that need a
    /// fully persisted view should use `export_persisted_state`.
    pub fn export_state(&self) -> crate::SessionSnapshot {
        self.state.to_snapshot()
    }

    pub fn read_view(&self) -> crate::SessionReadView {
        crate::SessionReadView::from_runtime_state(
            &self.state,
            self.state.effective_policy().clone(),
            self.state.effective_protocol_turn_options().clone(),
        )
    }

    /// Export the narrow persistence snapshot used by stores and resume logic.
    pub fn export_persistence_state(&self) -> RuntimeSessionState {
        self.state.clone()
    }

    pub fn apply_persistence_state(
        &mut self,
        state: RuntimeSessionState,
    ) -> Result<(), SessionError> {
        self.set_persisted_state(state)
    }

    /// Export a persistence-ready state envelope with dynamic/plugin snapshots
    /// refreshed from the live session.
    pub async fn export_persisted_state(&mut self) -> Result<RuntimeSessionState, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        let mut state = self.state.clone();
        state.protocol_turn_options = self.protocol_turn_options.clone();
        if let Some(session) = self.session.as_ref() {
            let snapshot = session.plugins().tool_registry().export_state();
            state.set_tool_state_snapshot(Some(snapshot));
            let captured = session.plugins().snapshot();
            crate::runtime::state::store_plugin_snapshot(&mut state, captured);
            state.plugin_snapshot_revision =
                Some(session.plugins().snapshot_revision_fingerprint());
        }
        Ok(state)
    }

    pub fn usage_report(&self) -> SessionUsageReport {
        let mut entries = self.state.token_ledger.clone();
        let drained = self.shared_token_ledger.lock_recover();
        let mut saturated = false;
        for entry in drained.iter().cloned() {
            saturated |= merge_ledger_entry_saturating(&mut entries, entry.entry);
        }
        SessionUsageReport::from_entries_with_saturation(&entries, saturated)
    }

    pub async fn await_background_work(&mut self) -> Result<(), SessionError> {
        if self.process_sync_needed.swap(false, Ordering::AcqRel) {
            self.refresh_session_graph_from_store().await?;
        }
        Ok(())
    }

    pub(super) async fn refresh_session_graph_from_store(&mut self) -> Result<(), SessionError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            self.resident_graph_head_stale
                .store(false, Ordering::Release);
            return Ok(());
        };
        let read = store.load_session().await.map_err(|err| {
            SessionError::Protocol(format!("failed to refresh session graph from store: {err}"))
        })?;
        self.graph_loaded_from_store = true;
        let Some(read) = read else {
            self.resident_graph_head_stale
                .store(false, Ordering::Release);
            return Ok(());
        };
        let has_newer_graph = self.state.head_revision != read.head_revision
            || read.graph.leaf_node_id != self.state.session_graph.leaf_node_id
            || read.checkpoint_ref != self.state.checkpoint_ref;
        if !has_newer_graph {
            self.resident_graph_head_stale
                .store(false, Ordering::Release);
            return Ok(());
        }
        // Defend refreshes against third-party stores that return an unvalidated resident graph.
        read.graph
            .validate_resident_integrity()
            .map_err(|source| SessionError::Store {
                context: "failed to refresh session graph from store".to_string(),
                source,
            })?;
        let head = crate::store::SessionHead {
            session_id: read.session_id.clone(),
            head_revision: read.head_revision,
            current_frame_node_id: read.current_frame_node_id.clone(),
            graph: read.graph,
            config: read.config.clone(),
            checkpoint_ref: read.checkpoint_ref.clone(),
            token_ledger: read.token_ledger,
        };
        apply_session_head(&mut self.state, &head);
        apply_session_checkpoint(&mut self.state, read.checkpoint).map_err(|source| {
            SessionError::Store {
                context: "failed to restore session checkpoint".to_string(),
                source,
            }
        })?;
        self.policy = self.state.effective_policy().clone();
        self.protocol_turn_options = self.state.effective_protocol_turn_options().clone();
        self.resident_graph_head_stale
            .store(false, Ordering::Release);
        Ok(())
    }

    pub(super) fn runtime_session_services(
        &self,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        if !self.resident_session_state_valid {
            self.trace_synchronous_resident_state_refusal("runtime_session_services");
            return Err(PluginOperationInvokeError::Unknown(
                "resident session state is invalidated; durable reload is required".to_string(),
            ));
        }
        Ok(Arc::new(RuntimeSessionServices::new(
            self, true, None, None,
        )?))
    }

    pub(super) fn runtime_session_services_for_turn(
        &self,
        child_usage_event_relay: Option<ChildUsageEventRelay>,
        held_session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::new(
            self,
            false,
            child_usage_event_relay,
            held_session_execution_lease,
        )?))
    }

    pub(super) fn runtime_session_services_after_commit(
        &self,
        held_session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::new(
            self,
            true,
            None,
            held_session_execution_lease,
        )?))
    }

    pub fn session_state_service(
        &self,
    ) -> Result<Arc<dyn crate::plugin::SessionStateService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.state_service())
    }

    pub fn session_lifecycle_service(
        &self,
    ) -> Result<Arc<dyn crate::plugin::SessionLifecycleService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.lifecycle_service())
    }

    /// Returns a lane-less host service for calls between turn drivers, never concurrently with a running turn.
    pub fn session_graph_service(
        &self,
    ) -> Result<Arc<dyn crate::plugin::SessionGraphService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.graph_service())
    }

    pub fn process_service(
        &self,
    ) -> Result<Arc<dyn crate::ProcessService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.process_service())
    }

    pub fn effect_host(&self) -> Arc<dyn crate::EffectHost> {
        Arc::clone(&self.host.core.control.effect_host)
    }

    pub async fn enqueue_turn_input(
        &self,
        input: crate::TurnInput,
        ingress: crate::TurnInputIngress,
        source_key: Option<String>,
    ) -> Result<crate::PendingTurnInput, RuntimeError> {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(queued_turn_input_store_required)?;
        enqueue_turn_input_to_store(
            self.state.session_id.clone(),
            store,
            self.host.queued_work_driver.clone(),
            input,
            ingress,
            source_key,
        )
        .await
    }

    pub async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, RuntimeError> {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(queued_turn_input_store_required)?;
        store
            .cancel_queued_work_batch(session_id, batch_id)
            .await
            .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()))
    }

    /// The plugin session bound to the currently active runtime session, if any.
    pub fn plugin_session(&self) -> Option<Arc<crate::PluginSession>> {
        if !self.resident_session_state_valid {
            self.trace_synchronous_resident_state_refusal("plugin_session");
            return None;
        }
        self.session.as_ref().map(|s| Arc::clone(s.plugins()))
    }

    pub async fn open_agent_frame(
        &mut self,
        request: crate::OpenAgentFrameRequest,
    ) -> Result<crate::OpenAgentFrameResult, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        Ok(open_agent_frame_in_state_with_clock(
            &mut self.state,
            request,
            self.host.core.clock.as_ref(),
        ))
    }

    /// Run the registered compaction provider and commit the resulting
    /// seed nodes into a fresh Agent Frame.
    pub async fn compact_context(
        &mut self,
        instructions: Option<String>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
    ) -> Result<bool, PluginOperationInvokeError> {
        self.reload_invalidated_resident_session_state()
            .await
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        let services = self.runtime_session_services()?;
        let compaction_boundary = scoped_effect_controller.scope_id().to_string();
        let Some(plugin_session) = self.session.as_ref().map(|s| Arc::clone(s.plugins())) else {
            return Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
        let ctx = crate::CompactionContext {
            session_id: self.state.session_id.clone(),
            state: self.read_view(),
            instructions,
            sessions: services.state_service(),
            session_lifecycle: services.lifecycle_service(),
            session_graph: services.graph_service(),
            scoped_effect_controller,
        };
        let Some(compaction) = plugin_session.compact_context(&ctx).await.map_err(|err| {
            PluginOperationInvokeError::Unknown(format!("context compaction failed: {err}"))
        })?
        else {
            return Ok(false);
        };
        let frame_id = compaction_frame_id(
            &self.state.session_id,
            &compaction_boundary,
            self.state
                .current_frame_node_id
                .as_deref()
                .unwrap_or_default(),
        );
        let result = self
            .open_agent_frame(
                crate::OpenAgentFrameRequest::new(frame_id, crate::AgentFrameReason::compaction())
                    .with_initial_nodes(compaction.initial_nodes),
            )
            .await
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        if result.opened {
            self.stamp_live_plugin_state();
        }
        Ok(result.opened)
    }

    pub(super) fn session_policy(&self) -> SessionPolicy {
        self.policy.clone()
    }

    pub(super) async fn notify_session_config_changed(
        &self,
        previous: SessionPolicy,
    ) -> Result<(), crate::PluginError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        let current = self.session_policy();
        if current == previous {
            return Ok(());
        }
        let Ok(services) = self.runtime_session_services() else {
            return Ok(());
        };
        session
            .plugins()
            .emit_runtime_event(crate::PluginLifecycleEvent::SessionConfigChanged(Box::new(
                SessionConfigChangedContext {
                    session_id: self.state.session_id.clone(),
                    previous,
                    current,
                    sessions: services.state_service(),
                },
            )))
            .await
    }

    pub(super) async fn apply_session_config_mutations(&mut self, previous: SessionPolicy) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let current = self.session_policy();
        if current == previous {
            return;
        }
        let Ok(services) = self.runtime_session_services() else {
            return;
        };
        self.policy = session
            .plugins()
            .mutate_session_config(
                SessionConfigChangedContext {
                    session_id: self.state.session_id.clone(),
                    previous,
                    current,
                    sessions: services.state_service(),
                },
                self.policy.clone(),
            )
            .await;
        self.state.policy = self.policy.clone();
    }
}

pub(in crate::runtime) async fn enqueue_turn_input_to_store(
    session_id: String,
    store: Arc<dyn crate::RuntimePersistence>,
    queued_work_driver: Option<crate::QueuedWorkDriver>,
    input: crate::TurnInput,
    ingress: crate::TurnInputIngress,
    source_key: Option<String>,
) -> Result<crate::PendingTurnInput, RuntimeError> {
    super::turn_loop::ensure_durable_effect_input(&input)?;
    let is_next_turn = matches!(ingress, crate::TurnInputIngress::NextTurn);
    let mut draft = crate::PendingTurnInputDraft::new(session_id, ingress, input);
    draft.source_key = source_key;
    let enqueued = store
        .enqueue_pending_turn_input(draft)
        .await
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()))?;
    if is_next_turn && let Some(driver) = queued_work_driver.as_ref() {
        driver.notify_pending_work(Some(&enqueued.session_id), "queued_turn_input");
    }
    Ok(enqueued)
}

impl LashRuntime {
    pub async fn submit_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::SessionCommandReceipt, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        let idempotency_key = idempotency_key.into();
        if idempotency_key.trim().is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandIdempotencyKey,
                "session command idempotency key cannot be empty",
            ));
        }
        let source_key = command.source_key(&idempotency_key);
        let session_id = self.state.session_id.clone();
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            let batch_id = format!("inline-command:{}", uuid::Uuid::new_v4());
            self.apply_session_command(command, None, None).await?;
            return Ok(crate::SessionCommandReceipt {
                session_id,
                batch_id,
                source_key,
            });
        };
        let draft = crate::QueuedWorkBatchDraft::new(
            session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SlotPolicy::Exclusive,
            vec![crate::QueuedWorkPayload::session_command(command)],
        )
        .with_source_key(source_key.clone());
        let enqueued = store.enqueue_queued_work(draft).await.map_err(|err| {
            RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
        })?;
        if let Some(driver) = self.host.queued_work_driver.as_ref() {
            driver
                .claim_and_run_pending(Some(&session_id), "session_command")
                .await
                .map_err(|err| RuntimeError::new(RuntimeErrorCode::QueuedWork, err.to_string()))?;
            // An inline or external driver may have committed the command
            // before returning. Reconcile that authoritative head before this
            // resident runtime reaches another commit boundary; its lease is
            // advisory, so retaining the pre-drive head would manufacture a
            // stale CAS conflict on close.
            self.refresh_session_graph_from_store()
                .await
                .map_err(|err| {
                    RuntimeError::new(
                        crate::RuntimeErrorCode::SessionCommandPostDriveRefresh,
                        err.to_string(),
                    )
                })?;
        }
        Ok(crate::SessionCommandReceipt {
            session_id,
            batch_id: enqueued.batch_id,
            source_key,
        })
    }

    pub async fn drain_next_session_command(
        &mut self,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(None);
        };
        let claim = store
            .claim_leading_ready_session_command(
                &self.state.session_id,
                session_execution_lease,
                &self.runtime_lease_owner,
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(claim) = claim else {
            return Ok(None);
        };
        let Some((batch, command)) = claim.exclusive_session_command() else {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandClaim,
                format!(
                    "queued-work claim `{}` did not contain exactly one session command batch",
                    claim.claim_id
                ),
            ));
        };
        let batch_id = batch.batch_id.clone();
        let source_key = batch.source_key.clone().unwrap_or_else(|| batch_id.clone());
        let command = command.clone();
        self.apply_session_command(
            command,
            Some(claim.completion()),
            Some(session_execution_lease),
        )
        .await?;
        Ok(Some(crate::SessionCommandReceipt {
            session_id: self.state.session_id.clone(),
            batch_id,
            source_key,
        }))
    }

    async fn apply_session_command(
        &mut self,
        command: crate::SessionCommand,
        completion: Option<crate::QueuedWorkCompletion>,
        session_execution_lease: Option<&crate::SessionExecutionLeaseAuthority>,
    ) -> Result<(), RuntimeError> {
        self.refresh_session_graph_from_store()
            .await
            .map_err(|err| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::SessionCommandRefresh,
                    err.to_string(),
                )
            })?;
        let crate::SessionCommand::RefreshToolCatalog { .. } = command;
        self.refresh_session_tool_catalog().await.map_err(|err| {
            RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandRefreshTools,
                err.to_string(),
            )
        })?;
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(());
        };
        let operation = completion
            .as_ref()
            .and_then(|completion| completion.batch_ids.first())
            .map(|batch_id| {
                crate::OperationId::new(self.state.queue_drain_scope(batch_id), "session-command")
            })
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "persisted session commands require a claimed queue boundary",
                )
            })?;
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                &[],
                operation,
                self.host.core.durability.commit_budget,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        // Queue-claim settlement is generation-pinned per ADR 0029; presenting
        // the live execution fence on this commit is FIG-1072 territory.
        let Some(_session_execution_lease) = session_execution_lease else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "session command commit requires a session execution lease",
            ));
        };
        if let Some(completion) = completion {
            commit = commit.completing_queue_claim(completion);
        }
        let result = crate::store::commit_runtime_state_verified(store.as_ref(), commit)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        self.state.apply_persisted_commit_result(result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        Ok(())
    }
}

fn compaction_frame_id(
    session_id: &str,
    boundary_id: &str,
    previous_frame_node_id: &str,
) -> String {
    format!("{session_id}:frame:compaction:{boundary_id}:after:{previous_frame_node_id}")
}

pub(in crate::runtime) fn queued_turn_input_store_required() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::StoreCommitFailed,
        "queued turn input requires a persistent runtime store",
    )
}

#[cfg(test)]
mod tests {
    use super::compaction_frame_id;

    #[test]
    fn compaction_frame_identity_is_replay_stable() {
        let first = compaction_frame_id("session", "turn", "frame-before");
        let replay = compaction_frame_id("session", "turn", "frame-before");
        let next = compaction_frame_id("session", "turn", "frame-after");

        assert_eq!(first, replay);
        assert_ne!(first, next);
    }
}
