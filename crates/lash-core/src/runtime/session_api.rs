use super::*;

impl LashRuntime {
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    pub(super) fn stamp_live_plugin_state(&mut self) {
        if let Some(session) = self.session.as_ref() {
            let snapshot = session.plugins().tool_registry().export_state();
            self.state.tool_state_generation = Some(snapshot.generation());
            self.state.tool_state_snapshot = Some(snapshot);
            let captured = session.plugins().snapshot();
            crate::runtime::state::store_plugin_snapshot(&mut self.state.plugin_snapshot, captured);
            self.state.plugin_snapshot_revision =
                Some(session.plugins().snapshot_revision_fingerprint());
        } else {
            self.state.tool_state_generation = None;
            self.state.tool_state_snapshot = None;
            self.state.plugin_snapshot = None;
            self.state.plugin_snapshot_revision = None;
        }
    }
    pub(super) fn active_tool_catalog_shared(
        &self,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        self.session
            .as_ref()
            .map(|session| session.shared_tool_catalog(&self.state.session_id))
            .unwrap_or_else(|| Ok(Arc::new(Vec::new())))
    }

    pub fn tool_state(&self) -> Result<crate::ToolState, SessionError> {
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
        if let Some(frame) = self.state.current_agent_frame_mut() {
            frame.protocol_turn_options = options.clone();
        }
        self.protocol_turn_options = options;
    }

    /// The durable protocol turn options recorded on the session.
    pub fn protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        &self.state.protocol_turn_options
    }

    /// Override protocol-owned turn options and mirror them to **every** agent
    /// frame. Used by the protocol materialization hook (apply-at-open): the
    /// last applied value is recorded on the session and on all frames.
    pub fn set_protocol_turn_options_all_frames(&mut self, options: crate::ProtocolTurnOptions) {
        self.state.protocol_turn_options = options.clone();
        for frame in &mut self.state.agent_frames {
            frame.protocol_turn_options = options.clone();
        }
        self.protocol_turn_options = options;
    }

    /// Run the protocol plugin's materialization hook against this runtime.
    ///
    /// Fires the [`ProtocolSessionPlugin::configure_runtime_on_materialize`]
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

    /// Export current session state for inspection/UI purposes.
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

    pub(crate) fn export_graph_first_state(&self) -> RuntimeSessionState {
        self.state.clone()
    }

    /// Export a persistence-ready state envelope with dynamic/plugin snapshots
    /// refreshed from the live session.
    pub fn export_persisted_state(&self) -> RuntimeSessionState {
        let mut state = self.state.clone();
        state.protocol_turn_options = self.protocol_turn_options.clone();
        if let Some(frame) = state.current_agent_frame_mut() {
            frame.protocol_turn_options = self.protocol_turn_options.clone();
        }
        if let Some(session) = self.session.as_ref() {
            let snapshot = session.plugins().tool_registry().export_state();
            state.tool_state_generation = Some(snapshot.generation());
            state.tool_state_snapshot = Some(snapshot);
            let captured = session.plugins().snapshot();
            crate::runtime::state::store_plugin_snapshot(&mut state.plugin_snapshot, captured);
            state.plugin_snapshot_revision =
                Some(session.plugins().snapshot_revision_fingerprint());
        }
        state
    }

    pub fn usage_report(&self) -> SessionUsageReport {
        let mut entries = self.state.token_ledger.clone();
        let drained = self.shared_token_ledger.lock().expect("token ledger lock");
        for entry in drained.iter().cloned() {
            merge_ledger_entry(&mut entries, entry);
        }
        SessionUsageReport::from_entries(&entries)
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
            return Ok(());
        };
        let scope = match self.residency {
            crate::Residency::KeepAll => crate::store::SessionReadScope::FullGraph,
            crate::Residency::ActivePathOnly => crate::store::SessionReadScope::ActivePath {
                leaf_node_id: self.state.session_graph.leaf_node_id.clone(),
            },
        };
        let read = store.load_session(scope).await.map_err(|err| {
            SessionError::Protocol(format!("failed to refresh session graph from store: {err}"))
        })?;
        self.graph_loaded_from_store = true;
        let Some(read) = read else {
            return Ok(());
        };
        let has_newer_graph = self.state.head_revision != Some(read.head_revision)
            || read.graph.leaf_node_id != self.state.session_graph.leaf_node_id
            || read.checkpoint_ref != self.state.checkpoint_ref;
        if !has_newer_graph {
            return Ok(());
        }
        let head = crate::store::SessionHead {
            session_id: read.session_id.clone(),
            head_revision: read.head_revision,
            current_frame_node_id: read.current_frame_node_id.clone(),
            graph: read.graph,
            config: read.config.clone(),
            checkpoint_ref: read.checkpoint_ref.clone(),
            token_ledger: merge_usage_delta_entries(read.token_ledger),
        };
        apply_session_head(&mut self.state, &head);
        apply_session_checkpoint(&mut self.state, read.checkpoint);
        self.policy = self.state.effective_policy().clone();
        self.protocol_turn_options = self.state.effective_protocol_turn_options().clone();
        Ok(())
    }

    pub(super) fn runtime_session_services(
        &self,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::new(self, true, None)?))
    }

    pub(super) fn runtime_session_services_for_turn(
        &self,
        child_usage_event_relay: Option<ChildUsageEventRelay>,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::new(
            self,
            false,
            child_usage_event_relay,
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

    pub fn process_cancel_ability(&self) -> Arc<dyn crate::ProcessCancelAbility> {
        Arc::clone(&self.host.core.control.process_cancel_ability)
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
        self.session.as_ref().map(|s| Arc::clone(s.plugins()))
    }

    pub fn open_agent_frame(
        &mut self,
        request: crate::OpenAgentFrameRequest,
    ) -> crate::OpenAgentFrameResult {
        open_agent_frame_in_state_with_clock(
            &mut self.state,
            request,
            self.host.core.clock.as_ref(),
        )
    }

    /// Run the registered compaction provider and commit the resulting
    /// seed nodes into a fresh Agent Frame.
    pub async fn compact_context(
        &mut self,
        instructions: Option<String>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
    ) -> Result<bool, PluginOperationInvokeError> {
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
        let result = self.open_agent_frame(
            crate::OpenAgentFrameRequest::new(frame_id, crate::AgentFrameReason::compaction())
                .with_initial_nodes(compaction.initial_nodes),
        );
        if result.opened {
            self.stamp_live_plugin_state();
        }
        Ok(result.opened)
    }

    pub(super) fn session_policy(&self) -> SessionPolicy {
        self.policy.clone()
    }

    pub(super) async fn notify_session_config_changed(&self, previous: SessionPolicy) {
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
            .await;
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
        drop(driver.wake_pending(Some(&enqueued.session_id), "queued_turn_input"));
    }
    Ok(enqueued)
}

impl LashRuntime {
    pub async fn submit_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::SessionCommandReceipt, RuntimeError> {
        let idempotency_key = idempotency_key.into();
        if idempotency_key.trim().is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::Other("session_command_idempotency_key".to_string()),
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
                .map_err(|err| {
                    RuntimeError::new(
                        RuntimeErrorCode::Other("queued_work".to_string()),
                        err.to_string(),
                    )
                })?;
            // An inline or external driver may have committed the command
            // before returning. Reconcile that authoritative head before this
            // resident runtime reaches another commit boundary; its lease is
            // advisory, so retaining the pre-drive head would manufacture a
            // stale CAS conflict on close.
            self.refresh_session_graph_from_store()
                .await
                .map_err(|err| {
                    RuntimeError::new("session_command_post_drive_refresh", err.to_string())
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
        session_execution_lease: &crate::SessionExecutionLeaseFence,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
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
                "session_command_claim",
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
        session_execution_lease: Option<&crate::SessionExecutionLeaseFence>,
    ) -> Result<(), RuntimeError> {
        self.refresh_session_graph_from_store()
            .await
            .map_err(|err| RuntimeError::new("session_command_refresh", err.to_string()))?;
        let crate::SessionCommand::RefreshToolCatalog { .. } = command;
        self.refresh_session_tool_catalog()
            .await
            .map_err(|err| RuntimeError::new("session_command_refresh_tools", err.to_string()))?;
        let graph = crate::store::GraphCommitDelta::Unchanged {
            leaf_node_id: self.state.session_graph.leaf_node_id.clone(),
        };
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
                crate::OperationId::new(
                    crate::ExecutionScope::queue_drain(&self.state.session_id, batch_id),
                    "session-command",
                )
            })
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "persisted session commands require a claimed queue boundary",
                )
            })?;
        let (mut commit, node_id_mapping) =
            crate::store::RuntimeCommit::persisted_state_with_graph_commit(&self.state, graph, &[])
                .with_operation(operation)
                .map_err(super::runtime_error_from_store_commit)?;
        debug_assert!(
            node_id_mapping.is_empty(),
            "session-command commits must not carry graph appends"
        );
        let Some(session_execution_lease) = session_execution_lease else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "session command commit requires a session execution lease",
            ));
        };
        commit = commit.with_session_execution_lease(session_execution_lease.clone());
        if let Some(completion) = completion {
            commit = commit.completing_queue_claim(completion);
        }
        let result = crate::store::commit_runtime_state_verified(store.as_ref(), commit)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        self.state.apply_persisted_commit_result(result);
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
