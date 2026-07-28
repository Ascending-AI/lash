pub(crate) use crate::session::SessionConfigPatch;
use crate::support::*;
pub use lash_core::{AcceptedInjectedTurnInput, PluginCommand, PluginQuery, PluginTask};

#[derive(Clone)]
pub struct Completions {
    pub(crate) core: LashCore,
}

impl Completions {
    pub async fn resolve(
        &self,
        key: lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome> {
        self.core
            .env
            .core
            .control
            .effect_host
            .resolve_await_event(&key, resolution)
            .await
            .map_err(|err| EmbedError::Plugin(lash_core::PluginError::Session(err.to_string())))
    }
}

#[derive(Clone)]
pub struct CoreTriggerAdmin {
    pub(crate) core: LashCore,
}

impl CoreTriggerAdmin {
    pub async fn emit(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::TriggerEmitReport> {
        let store = self.core.env.trigger_store.as_ref().ok_or_else(|| {
            EmbedError::Plugin(lash_core::PluginError::Session(
                "trigger store is unavailable in this runtime".to_string(),
            ))
        })?;
        let drivers = self.core.work_driver.drivers().await;
        let router = lash_core::TriggerRouter::new(
            Arc::clone(store),
            self.core.env.process_registry.clone(),
            drivers.process,
        );
        router
            .emit(request, scoped_effect_controller.controller())
            .await
            .map_err(Into::into)
    }

    pub async fn subscriptions(
        &self,
        filter: lash_core::TriggerSubscriptionFilter,
    ) -> Result<Vec<lash_core::TriggerRegistration>> {
        let store = self.core.env.trigger_store.as_ref().ok_or_else(|| {
            EmbedError::Plugin(lash_core::PluginError::Session(
                "trigger store is unavailable in this runtime".to_string(),
            ))
        })?;
        let records = store.list_subscriptions(filter).await?;
        Ok(records
            .iter()
            .map(lash_core::TriggerRegistration::from)
            .collect())
    }
}

#[derive(Clone)]
pub struct SessionAdmin {
    pub(crate) runtime: RuntimeHandle,
}

impl SessionAdmin {
    pub fn config(&self) -> SessionConfigAdmin {
        SessionConfigAdmin {
            control: self.clone(),
        }
    }

    pub fn tools(&self) -> ToolAdmin {
        ToolAdmin {
            control: self.clone(),
        }
    }

    pub fn commands(&self) -> SessionCommandAdmin {
        SessionCommandAdmin {
            control: self.clone(),
        }
    }

    pub fn triggers(&self) -> SessionTriggerAdmin {
        SessionTriggerAdmin {
            control: self.clone(),
        }
    }

    pub fn state(&self) -> SessionStateAdmin {
        SessionStateAdmin {
            control: self.clone(),
        }
    }

    pub fn children(&self) -> ChildSessionAdmin {
        ChildSessionAdmin {
            control: self.clone(),
        }
    }

    pub fn injection(&self) -> InjectionAdmin {
        InjectionAdmin {
            control: self.clone(),
        }
    }

    pub fn protocol(&self) -> ProtocolAdmin {
        ProtocolAdmin {
            control: self.clone(),
        }
    }

    pub fn processes(&self) -> SessionProcessAdmin {
        SessionProcessAdmin {
            control: self.clone(),
        }
    }

    /// Run `f` against the locked runtime writer, then publish the resulting
    /// observation. The body is the canonical `lock → call → publish_from`
    /// stamp shared by nearly every mutating control method; publish happens
    /// unconditionally once the closure returns.
    async fn with_writer<F, T>(&self, f: F) -> T
    where
        F: AsyncFnOnce(&mut LashRuntime) -> T,
    {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let value = f(&mut runtime).await;
        self.runtime.publish_from(&runtime);
        value
    }

    async fn update_config(&self, patch: SessionConfigPatch) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .update_session_config(patch.provider, patch.model, patch.prompt)
                .await;
        })
        .await;
        Ok(())
    }

    async fn export_state(&self) -> lash_core::SessionSnapshot {
        self.runtime.observe().read_view.to_snapshot()
    }

    async fn append_messages(&self, messages: Vec<PluginMessage>) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .append_session_nodes(lash_core::AppendSessionNodesRequest {
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    nodes: messages
                        .into_iter()
                        .map(lash_core::SessionAppendNode::message)
                        .collect(),
                    requires_ancestor_node_id: None,
                })
                .await
                .map(|_| ())
                .map_err(Into::into)
        })
        .await
    }

    async fn append_plugin_body(
        &self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
    ) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .append_session_nodes(lash_core::AppendSessionNodesRequest {
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    nodes: vec![lash_core::SessionAppendNode::plugin(plugin_type, body)],
                    requires_ancestor_node_id: None,
                })
                .await
                .map(|_| ())
                .map_err(Into::into)
        })
        .await
    }

    async fn set_persisted_state(&self, state: RuntimeSessionState) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.set_persisted_state(state).map_err(Into::into)
        })
        .await
    }

    async fn set_prompt_template(&self, template: PromptTemplate) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.set_prompt_template(template).await;
        })
        .await;
        Ok(())
    }

    async fn clear_prompt_template(&self) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.clear_prompt_template().await;
        })
        .await;
        Ok(())
    }

    async fn add_prompt_contribution(&self, contribution: PromptContribution) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.add_prompt_contribution(contribution).await;
        })
        .await;
        Ok(())
    }

    async fn replace_prompt_slot(
        &self,
        slot: PromptSlot,
        contributions: impl IntoIterator<Item = PromptContribution>,
    ) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.replace_prompt_slot(slot, contributions).await;
        })
        .await;
        Ok(())
    }

    async fn clear_prompt_slot(&self, slot: PromptSlot) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.clear_prompt_slot(slot).await;
        })
        .await;
        Ok(())
    }

    async fn apply_protocol_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtensionHandle,
    ) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .apply_protocol_session_extension(extension)
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn branch_to_node(
        &self,
        target_leaf: Option<String>,
    ) -> Result<lash_core::SessionSnapshot> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .branch_to_node(target_leaf)
                .await
                .map_err(Into::into)
        })
        .await
    }

    /// Refresh the session graph from any background process that signalled it
    /// changed. This is the honest name for what the core `await_background_work`
    /// call does — a session-graph resync, **not** a terminal wait on background
    /// work (that lives on the process admin's `await_output`). Renamed off the
    /// old `SessionProcessAdmin::await_all` misnomer per the ADR 0019 grill.
    pub(crate) async fn refresh_background_graph(&self) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.await_background_work().await.map_err(Into::into)
        })
        .await
    }

    fn process_registry(&self) -> Result<Arc<dyn lash_core::ProcessRegistry>> {
        self.runtime
            .observe()
            .process_registry
            .clone()
            .ok_or_else(|| {
                EmbedError::Plugin(lash_core::PluginError::Session(
                    "process registry is unavailable in this runtime".to_string(),
                ))
            })
    }

    fn process_observer(&self) -> Result<lash_core::ProcessWorkObserver> {
        Ok(lash_core::ProcessWorkObserver::new(
            self.process_registry()?,
        ))
    }

    /// An observer over the session's process registry, or `None` when this
    /// runtime has no registry. Session-scoped reads use this so a registry-less
    /// runtime observes an empty process set rather than erroring, matching the
    /// pre-unification `list_process_handles` behavior.
    fn process_observer_opt(&self) -> Option<lash_core::ProcessWorkObserver> {
        self.runtime
            .observe()
            .process_registry
            .clone()
            .map(lash_core::ProcessWorkObserver::new)
    }

    /// The session scopes this session may address grants under: its root scope
    /// plus its current agent frame, deduplicated. Mirrors the scope set the
    /// runtime observation lists handles across, so the grant-scoped session
    /// view sees exactly what the session can address.
    fn process_visible_scopes(&self) -> Vec<lash_core::SessionScope> {
        let observation = self.runtime.observe();
        let root = observation.process_scope();
        let mut scopes = vec![root.clone()];
        let frame_id = observation.persisted_state.current_agent_frame_id.as_str();
        if !frame_id.is_empty() {
            let frame_scope =
                lash_core::SessionScope::for_agent_frame(observation.session_id(), frame_id);
            if frame_scope.id() != root.id() {
                scopes.push(frame_scope);
            }
        }
        scopes
    }

    async fn signal_process(
        &self,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEvent> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        let session_id = runtime.session_id().to_string();
        let processes = runtime.process_service()?;
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        processes
            .signal(
                &session_id,
                process_id,
                signal_name,
                signal_id,
                payload,
                scope,
            )
            .await
            .map_err(EmbedError::Plugin)
    }

    async fn transfer_process_handles(
        &self,
        to_session_id: &str,
        process_ids: Vec<String>,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<()> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        let session_id = runtime.session_id().to_string();
        let processes = runtime.process_service()?;
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        processes
            .transfer(&session_id, to_session_id, process_ids, scope)
            .await
            .map_err(EmbedError::Plugin)
    }

    async fn await_process_output(
        &self,
        process_id: &str,
    ) -> Result<lash_core::ProcessAwaitOutput> {
        lash_core::ProcessAwaiter::polling(self.process_registry()?)
            .await_terminal(process_id)
            .await
            .map_err(Into::into)
    }

    async fn request_process_abandon(
        &self,
        process_id: &str,
        reason: Option<String>,
    ) -> Result<lash_core::ObservedProcess> {
        let session_id = self.runtime.observe().session_id().to_string();
        let request = lash_core::AbandonRequest {
            requested_by: format!("session:{session_id}"),
            requested_at_ms: crate::process_admin::now_epoch_ms(),
            reason,
        };
        self.process_registry()?
            .request_process_abandon(process_id, request)
            .await?;
        self.process_observer()?
            .process(process_id)
            .await
            .ok_or_else(|| {
                EmbedError::Plugin(lash_core::PluginError::Session(format!(
                    "process `{process_id}` vanished after recording its abandon request"
                )))
            })
    }

    async fn refresh_tool_catalog(&self) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .refresh_session_tool_catalog()
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn submit_session_command(
        &self,
        command: lash_core::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::SessionCommandReceipt> {
        let idempotency_key = idempotency_key.into();
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .submit_session_command(command, idempotency_key)
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn list_trigger_registrations(&self) -> Result<Vec<lash_core::TriggerRegistration>> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .list_trigger_registrations()
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn trigger_registrations_by_source_type(
        &self,
        source_type: impl Into<lash_core::TriggerEventType>,
    ) -> Result<Vec<lash_core::TriggerRegistration>> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .trigger_registrations_by_source_type(source_type)
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn query_plugin_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(String, serde_json::Value)> {
        let observation = self.runtime.observe();
        let session_id = observation.session_id().to_string();
        observation
            .query_plugin(name, args, Some(session_id))
            .await
            .map_err(Into::into)
    }

    async fn run_plugin_command_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<lash_core::PluginCommandReceipt<serde_json::Value>> {
        let session_id = self.runtime.observe().session_id().to_string();
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let operation_scope = lash_core::ExecutionScope::runtime_operation(format!(
            "{session_id}:plugin_command:{name}:{}",
            lash_core::TurnActivityId::fresh().0
        ));
        let receipt = runtime
            .run_plugin_command(name, args, Some(session_id), operation_scope)
            .await?;
        self.record_plugin_operation_observations(&receipt.events, &receipt.pending_turn_inputs);
        self.runtime.publish_from(&runtime);
        Ok(receipt)
    }

    async fn run_plugin_task_raw_with_cancel(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation_token: CancellationToken,
    ) -> Result<lash_core::PluginTaskReceipt<serde_json::Value>> {
        let session_id = self.runtime.observe().session_id().to_string();
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let scope_id = format!(
            "{session_id}:plugin_task:{name}:{}",
            lash_core::TurnActivityId::fresh().0
        );
        let scoped_effect_controller = runtime
            .effect_host()
            .scoped_static(lash_core::ExecutionScope::runtime_operation(scope_id))
            .map_err(EmbedError::Runtime)?
            .ok_or_else(|| {
                EmbedError::Plugin(lash_core::PluginError::Session(
                    "plugin task execution requires an effect host that can create a static runtime-operation scope".to_string(),
                ))
            })?;
        let receipt = runtime
            .run_plugin_task(
                name,
                args,
                Some(session_id),
                scoped_effect_controller,
                cancellation_token,
            )
            .await?;
        self.record_plugin_operation_observations(&receipt.events, &receipt.pending_turn_inputs);
        self.runtime.publish_from(&runtime);
        Ok(receipt)
    }

    fn record_plugin_operation_observations(
        &self,
        events: &[lash_core::PluginOwned<lash_core::PluginRuntimeEvent>],
        pending_turn_inputs: &[lash_core::PendingTurnInput],
    ) {
        for owned in events {
            self.runtime.record_turn_activity(
                None,
                lash_core::TurnActivity::independent(lash_core::TurnEvent::PluginRuntime {
                    plugin_id: owned.plugin_id.clone(),
                    event: owned.value.clone(),
                }),
            );
        }
        if !pending_turn_inputs.is_empty() {
            self.runtime.record_queue_changed(
                lash_core::SessionQueueEventKind::Enqueued,
                pending_turn_inputs
                    .iter()
                    .map(|input| input.input_id.clone())
                    .collect(),
            );
        }
    }

    async fn compact_context(
        &self,
        instructions: Option<String>,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<bool> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .compact_context(instructions, scoped_effect_controller)
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn persist_current_state(&self) -> Result<RuntimeSessionState> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.await_background_work().await?;
            Ok(runtime.export_persisted_state())
        })
        .await
    }

    async fn start_process(
        &self,
        request: lash_core::ProcessStartRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessHandleSummary> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        let session_id = runtime.session_id().to_string();
        let processes = runtime.process_service()?;
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        let summary = processes
            .start_from_request(&session_id, request, scope)
            .await
            .map_err(EmbedError::Plugin)?;
        self.runtime.record_process_changed(
            SessionProcessEventKind::Started,
            vec![summary.process_id.clone()],
        );
        Ok(summary)
    }

    async fn session_state_service(&self) -> Result<Arc<dyn SessionStateService>> {
        self.runtime
            .writer()
            .lock()
            .await
            .session_state_service()
            .map_err(Into::into)
    }

    async fn cancel_process(
        &self,
        process_id: &str,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessCancelSummary> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        let session_id = runtime.session_id().to_string();
        let processes = runtime.process_service()?;
        let cancel_ability = runtime.process_cancel_ability();
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        let summary = cancel_ability
            .cancel_summary(
                processes.as_ref(),
                lash_core::ProcessCancelRequest::new(
                    &session_id,
                    process_id,
                    scope,
                    lash_core::ProcessCancelSource::HostApi,
                )
                .with_reason("requested by host API"),
            )
            .await
            .map_err(EmbedError::Plugin)?;
        self.runtime.record_process_changed(
            SessionProcessEventKind::Cancelled,
            vec![summary.process_id.clone()],
        );
        Ok(summary)
    }

    async fn cancel_visible_processes(
        &self,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<Vec<lash_core::ProcessCancelSummary>> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        let session_id = runtime.session_id().to_string();
        let processes = runtime.process_service()?;
        let cancel_ability = runtime.process_cancel_ability();
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        let summaries = cancel_ability
            .cancel_all_visible(
                processes.as_ref(),
                lash_core::ProcessCancelAllRequest::new(
                    &session_id,
                    scope,
                    lash_core::ProcessCancelSource::HostApi,
                )
                .with_reason("requested by host API"),
            )
            .await
            .map_err(EmbedError::Plugin)?;
        self.runtime.record_process_changed(
            SessionProcessEventKind::Cancelled,
            summaries
                .iter()
                .map(|summary| summary.process_id.clone())
                .collect(),
        );
        Ok(summaries)
    }

    async fn snapshot_execution_state(&self) -> Result<Option<Vec<u8>>> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.snapshot_execution_state().await.map_err(Into::into)
        })
        .await
    }

    async fn restore_execution_state(&self, bytes: &[u8]) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .restore_execution_state(bytes)
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn tool_state(&self) -> Result<ToolState> {
        self.runtime.observe().tool_state.clone().ok_or_else(|| {
            EmbedError::Session(SessionError::Protocol(
                "runtime session not available".to_string(),
            ))
        })
    }

    async fn apply_tool_state(&self, state: ToolState) -> Result<u64> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .apply_tool_state(state)
                .await
                .map_err(EmbedError::from)
        })
        .await
    }

    async fn restore_tool_state(&self, state: ToolState) -> Result<ToolRestoreReport> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .restore_tool_state(state)
                .await
                .map_err(EmbedError::from)
        })
        .await
    }

    async fn set_tool_membership(&self, tool_id: lash_core::ToolId, present: bool) -> Result<u64> {
        self.set_tool_membership_many(&[(tool_id, present)]).await
    }

    async fn set_tool_membership_many(&self, updates: &[(lash_core::ToolId, bool)]) -> Result<u64> {
        let mut state = self.tool_state().await?;
        for (tool_id, present) in updates {
            state
                .set_membership(tool_id, *present)
                .map_err(|err| EmbedError::Session(SessionError::Protocol(err.to_string())))?;
        }
        self.apply_tool_state(state).await
    }

    async fn active_tool_manifests(&self) -> Result<Vec<ToolManifest>> {
        Ok(self.tool_state().await?.tool_manifests())
    }

    async fn add_tool_provider(&self, provider: Arc<dyn ToolProvider>) -> Result<ToolSourceHandle> {
        let tool_registry = self.tool_registry().await?;
        let handle = tool_registry
            .add_tool_provider(provider)
            .map_err(|err| EmbedError::Session(SessionError::Protocol(err.to_string())))?;
        self.refresh_tool_catalog().await?;
        Ok(handle)
    }

    async fn remove_tool_source(&self, handle: &ToolSourceHandle) -> Result<u64> {
        let tool_registry = self.tool_registry().await?;
        let generation = tool_registry
            .remove_source(handle)
            .map_err(|err| EmbedError::Session(SessionError::Protocol(err.to_string())))?;
        self.refresh_tool_catalog().await?;
        Ok(generation)
    }

    async fn create_child_session(&self, request: SessionCreateRequest) -> Result<SessionHandle> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        let lifecycle = runtime.session_lifecycle_service()?;
        lifecycle.create_session(request).await.map_err(Into::into)
    }

    async fn close_child_session(&self, session_id: &str) -> Result<()> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        let lifecycle = runtime.session_lifecycle_service()?;
        lifecycle
            .close_session(session_id)
            .await
            .map_err(Into::into)
    }

    async fn activate_managed_session(&self, session_id: &str) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .activate_managed_session(session_id)
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn inject_turn_input(
        &self,
        turn_id: &str,
        id: Option<String>,
        message: PluginMessage,
    ) -> Result<()> {
        self.inject_turn_inputs_for_turn(
            turn_id,
            vec![lash_core::InjectedTurnInput { id, message }],
        )
        .await
    }

    async fn inject_turn_inputs_for_turn(
        &self,
        turn_id: &str,
        messages: Vec<lash_core::InjectedTurnInput>,
    ) -> Result<()> {
        for input in messages {
            let source_key = input.id.map(|id| format!("injection:{id}"));
            let turn_input = turn_input_from_plugin_message(input.message);
            self.runtime
                .enqueue_turn_input(
                    turn_input,
                    lash_core::TurnInputIngress::active_turn(
                        turn_id,
                        lash_core::TurnInputCheckpointBoundary::AfterWork,
                    ),
                    source_key,
                )
                .await
                .map(|_| ())
                .map_err(EmbedError::Runtime)?;
        }
        Ok(())
    }

    async fn tool_registry(&self) -> Result<Arc<lash_core::ToolRegistry>> {
        self.runtime
            .writer()
            .lock()
            .await
            .plugin_session()
            .map(|session| session.tool_registry())
            .ok_or_else(|| {
                EmbedError::Session(SessionError::Protocol(
                    "tool registry is unavailable in this runtime session".to_string(),
                ))
            })
    }
}

fn turn_input_from_plugin_message(message: PluginMessage) -> TurnInput {
    let mut input = TurnInput::empty();
    if !message.content.is_empty() {
        input.items.push(InputItem::Text {
            text: message.content,
        });
    }
    for source in message.attachments {
        input.items.push(InputItem::attachment(source));
    }
    input
}

#[derive(Clone)]
pub struct SessionConfigAdmin {
    control: SessionAdmin,
}

impl SessionConfigAdmin {
    pub async fn update(&self, patch: SessionConfigPatch) -> Result<()> {
        self.control.update_config(patch).await
    }

    pub async fn set_prompt_template(&self, template: PromptTemplate) -> Result<()> {
        self.control.set_prompt_template(template).await
    }

    pub async fn clear_prompt_template(&self) -> Result<()> {
        self.control.clear_prompt_template().await
    }

    pub async fn add_prompt_contribution(&self, contribution: PromptContribution) -> Result<()> {
        self.control.add_prompt_contribution(contribution).await
    }

    pub async fn replace_prompt_slot(
        &self,
        slot: PromptSlot,
        contributions: impl IntoIterator<Item = PromptContribution>,
    ) -> Result<()> {
        self.control.replace_prompt_slot(slot, contributions).await
    }

    pub async fn clear_prompt_slot(&self, slot: PromptSlot) -> Result<()> {
        self.control.clear_prompt_slot(slot).await
    }
}

#[derive(Clone)]
pub struct ToolAdmin {
    control: SessionAdmin,
}

impl ToolAdmin {
    pub(crate) fn new(control: SessionAdmin) -> Self {
        Self { control }
    }
}

impl ToolAdmin {
    pub async fn state(&self) -> Result<ToolState> {
        self.control.tool_state().await
    }

    pub fn advanced(&self) -> AdvancedToolAdmin {
        AdvancedToolAdmin {
            control: self.control.clone(),
        }
    }

    /// Toggle Tool Catalog membership for a tool. `present` adds it as a
    /// member; `!present` removes it. Membership is the execution gate.
    pub async fn set_membership(
        &self,
        tool_id: impl Into<lash_core::ToolId>,
        present: bool,
    ) -> Result<u64> {
        self.control
            .set_tool_membership(tool_id.into(), present)
            .await
    }

    pub async fn set_membership_many(&self, updates: &[(lash_core::ToolId, bool)]) -> Result<u64> {
        self.control.set_tool_membership_many(updates).await
    }

    pub async fn active_manifests(&self) -> Result<Vec<ToolManifest>> {
        self.control.active_tool_manifests().await
    }

    pub async fn add_provider(&self, provider: Arc<dyn ToolProvider>) -> Result<ToolSourceHandle> {
        self.control.add_tool_provider(provider).await
    }

    pub async fn remove_source(&self, handle: &ToolSourceHandle) -> Result<u64> {
        self.control.remove_tool_source(handle).await
    }
}

#[derive(Clone)]
pub struct AdvancedToolAdmin {
    control: SessionAdmin,
}

impl AdvancedToolAdmin {
    /// Replace the entire tool-state snapshot.
    ///
    /// This is a generation-checked escape hatch for hosts that intentionally
    /// edit the full snapshot. Prefer `ToolAdmin` membership methods for
    /// ordinary tool policy changes.
    pub async fn apply_state(&self, state: ToolState) -> Result<u64> {
        self.control.apply_tool_state(state).await
    }

    /// Restore a persisted tool-state snapshot, adopting its generation.
    ///
    /// Use this when re-applying a snapshot read from durable storage (session
    /// resume), not an edited delta: it reconstructs the exact persisted surface
    /// idempotently rather than requiring the snapshot to match the current
    /// generation. A cold resume of a session whose surface reached generation
    /// ≥ 2 needs this — [`apply_state`](Self::apply_state) would reject it.
    ///
    /// Persisted tools whose source is not currently registered (e.g. a
    /// detached MCP server) do not fail the restore: they are kept as orphaned
    /// non-members, listed in the returned [`ToolRestoreReport`], and rebind
    /// automatically when a source re-advertises the same tool.
    pub async fn restore_state(&self, state: ToolState) -> Result<ToolRestoreReport> {
        self.control.restore_tool_state(state).await
    }
}

#[derive(Clone)]
pub struct SessionCommandAdmin {
    control: SessionAdmin,
}

impl SessionCommandAdmin {
    /// Enqueue an unconditional tool-catalog refresh. The command drains
    /// asynchronously and recomputes the surface from live sources, so it
    /// takes no generation guard — any generation observed at enqueue time
    /// could legitimately have advanced by drain time.
    pub async fn refresh_tool_catalog(
        &self,
        reason: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::SessionCommandReceipt> {
        self.control
            .submit_session_command(
                lash_core::SessionCommand::RefreshToolCatalog {
                    reason: reason.into(),
                },
                idempotency_key,
            )
            .await
    }
}

/// Session-scoped read controls for Lashlang trigger registrations.
#[derive(Clone)]
pub struct SessionTriggerAdmin {
    control: SessionAdmin,
}

impl SessionTriggerAdmin {
    /// Return every trigger registration in the session.
    ///
    /// This is an admin/introspection view. Source owners should prefer
    /// [`Self::by_source_type`] so they only inspect registrations for the
    /// concrete source type they own.
    pub async fn list_all(&self) -> Result<Vec<lash_core::TriggerRegistration>> {
        self.control.list_trigger_registrations().await
    }

    /// Return registrations whose source value has the given host descriptor type.
    ///
    /// This is the source-owner API: a timer, UI, webhook, or other host-owned
    /// source uses it to inspect registrations for keys it may schedule and emit.
    pub async fn by_source_type(
        &self,
        source_type: impl Into<lash_core::TriggerEventType>,
    ) -> Result<Vec<lash_core::TriggerRegistration>> {
        self.control
            .trigger_registrations_by_source_type(source_type)
            .await
    }
}

#[derive(Clone)]
pub struct SessionProcessAdmin {
    control: SessionAdmin,
}

/// Session-scoped view of the global process surface ([`Processes`]).
///
/// This is thin sugar, not a parallel surface (ADR 0019 grill): every read is
/// the global observer pre-filtered by this session's **grant** scope (what the
/// session may address), and every mutation delegates to the same runtime
/// process path the global surface uses. It speaks the same [`ObservedProcess`]
/// vocabulary as [`Processes`]; [`start`](Self::start) returns the
/// grant-entry handle row ([`lash_core::ProcessHandleSummary`]), the one row
/// type retained for the model/handle contract.
impl SessionProcessAdmin {
    pub(crate) fn new(control: SessionAdmin) -> Self {
        Self { control }
    }

    /// Grant-scoped read: the global observer filtered to every scope this
    /// session may address (root + current frame), deduplicated. One home for
    /// the session's read logic — it calls the observer, never reimplements it.
    async fn list_granted(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::ObservedProcess>> {
        // A registry-less runtime has no processes to address; observe empty
        // rather than erroring, matching the pre-unification session read.
        let Some(observer) = self.control.process_observer_opt() else {
            return Ok(Vec::new());
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for scope in self.control.process_visible_scopes() {
            for process in observer.list_granted_to(&scope, filter).await? {
                if seen.insert(process.process_id.clone()) {
                    out.push(process);
                }
            }
        }
        Ok(out)
    }

    pub async fn start(
        &self,
        request: lash_core::ProcessStartRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessHandleSummary> {
        self.control
            .start_process(request, scoped_effect_controller)
            .await
    }

    /// Running processes this session may address.
    pub async fn list(&self) -> Result<Vec<lash_core::ObservedProcess>> {
        self.list_granted(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Running,
            ..lash_core::ProcessListFilter::default()
        })
        .await
    }

    /// Every process (any status) this session may address.
    pub async fn list_all(&self) -> Result<Vec<lash_core::ObservedProcess>> {
        self.list_granted(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
    }

    /// One process this session may address, if present.
    pub async fn get(&self, process_id: &str) -> Result<Option<lash_core::ObservedProcess>> {
        Ok(self
            .list_all()
            .await?
            .into_iter()
            .find(|process| process.process_id == process_id))
    }

    pub async fn events(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<lash_core::ObservedProcessEvent>> {
        let Some(observer) = self.control.process_observer_opt() else {
            return Ok(Vec::new());
        };
        observer
            .events_after(process_id, after_sequence)
            .await
            .map_err(Into::into)
    }

    pub async fn await_output(&self, process_id: &str) -> Result<lash_core::ProcessAwaitOutput> {
        self.control.await_process_output(process_id).await
    }

    pub async fn signal(
        &self,
        process_id: &str,
        signal_name: impl Into<String>,
        signal_id: impl Into<String>,
        payload: serde_json::Value,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEvent> {
        self.control
            .signal_process(
                process_id,
                signal_name.into(),
                signal_id.into(),
                payload,
                scoped_effect_controller,
            )
            .await
    }

    pub async fn cancel(
        &self,
        process_id: &str,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessCancelSummary> {
        self.control
            .cancel_process(process_id, scoped_effect_controller)
            .await
    }

    pub async fn cancel_all(
        &self,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<Vec<lash_core::ProcessCancelSummary>> {
        self.control
            .cancel_visible_processes(scoped_effect_controller)
            .await
    }

    /// Move this session's handle grants for `process_ids` to another session.
    /// Re-homes the addressing grant only; the process itself is global.
    pub async fn transfer(
        &self,
        to_session_id: &str,
        process_ids: Vec<String>,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<()> {
        self.control
            .transfer_process_handles(to_session_id, process_ids, scoped_effect_controller)
            .await
    }

    /// Record an Abandon Request (ADR 0019) against a process this session
    /// owns: an authorization the recovery sweep reconciles into `Abandoned`
    /// once the owner's lease lapses. Returns the process as observed after.
    pub async fn request_abandon(
        &self,
        process_id: &str,
        reason: Option<String>,
    ) -> Result<lash_core::ObservedProcess> {
        self.control
            .request_process_abandon(process_id, reason)
            .await
    }
}

#[derive(Clone)]
pub struct SessionStateAdmin {
    control: SessionAdmin,
}

impl SessionStateAdmin {
    pub async fn export(&self) -> lash_core::SessionSnapshot {
        self.control.export_state().await
    }

    pub async fn append_messages(&self, messages: Vec<PluginMessage>) -> Result<()> {
        self.control.append_messages(messages).await
    }

    pub async fn append_plugin_body(
        &self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
    ) -> Result<()> {
        self.control.append_plugin_body(plugin_type, body).await
    }

    pub async fn set_persisted(&self, state: RuntimeSessionState) -> Result<()> {
        self.control.set_persisted_state(state).await
    }

    pub async fn branch_to_node(
        &self,
        target_leaf: Option<String>,
    ) -> Result<lash_core::SessionSnapshot> {
        self.control.branch_to_node(target_leaf).await
    }

    pub async fn persist_current(&self) -> Result<RuntimeSessionState> {
        self.control.persist_current_state().await
    }

    pub async fn session_state_service(&self) -> Result<Arc<dyn SessionStateService>> {
        self.control.session_state_service().await
    }

    pub async fn snapshot_execution(&self) -> Result<Option<Vec<u8>>> {
        self.control.snapshot_execution_state().await
    }

    pub async fn restore_execution(&self, bytes: &[u8]) -> Result<()> {
        self.control.restore_execution_state(bytes).await
    }

    pub async fn compact_context(
        &self,
        instructions: Option<String>,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<bool> {
        self.control
            .compact_context(instructions, scoped_effect_controller)
            .await
    }
}

#[derive(Clone)]
pub struct PluginOperations {
    pub(crate) control: SessionAdmin,
}

impl PluginOperations {
    pub async fn query<Op: lash_core::PluginQuery>(&self, args: Op::Args) -> Result<Op::Output> {
        let (_plugin_id, output) = self
            .control
            .query_plugin_raw(Op::NAME, encode_plugin_args::<Op>(args)?)
            .await?;
        decode_plugin_output::<Op>(output)
    }

    pub async fn query_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(String, serde_json::Value)> {
        self.control.query_plugin_raw(name, args).await
    }

    pub async fn run_command<Op: lash_core::PluginCommand>(
        &self,
        args: Op::Args,
    ) -> Result<lash_core::PluginCommandReceipt<Op::Output>> {
        let receipt = self
            .control
            .run_plugin_command_raw(Op::NAME, encode_plugin_args::<Op>(args)?)
            .await?;
        Ok(lash_core::PluginCommandReceipt {
            output: decode_plugin_output::<Op>(receipt.output)?,
            events: receipt.events,
            pending_turn_inputs: receipt.pending_turn_inputs,
        })
    }

    pub async fn run_command_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<lash_core::PluginCommandReceipt<serde_json::Value>> {
        self.control.run_plugin_command_raw(name, args).await
    }

    pub async fn run_task<Op: lash_core::PluginTask>(
        &self,
        args: Op::Args,
    ) -> Result<lash_core::PluginTaskReceipt<Op::Output>> {
        self.run_task_with_cancel::<Op>(args, CancellationToken::new())
            .await
    }

    pub async fn run_task_with_cancel<Op: lash_core::PluginTask>(
        &self,
        args: Op::Args,
        cancellation_token: CancellationToken,
    ) -> Result<lash_core::PluginTaskReceipt<Op::Output>> {
        let receipt = self
            .control
            .run_plugin_task_raw_with_cancel(
                Op::NAME,
                encode_plugin_args::<Op>(args)?,
                cancellation_token,
            )
            .await?;
        Ok(lash_core::PluginTaskReceipt {
            output: decode_plugin_output::<Op>(receipt.output)?,
            events: receipt.events,
            pending_turn_inputs: receipt.pending_turn_inputs,
        })
    }

    pub async fn run_task_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<lash_core::PluginTaskReceipt<serde_json::Value>> {
        self.run_task_raw_with_cancel(name, args, CancellationToken::new())
            .await
    }

    pub async fn run_task_raw_with_cancel(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation_token: CancellationToken,
    ) -> Result<lash_core::PluginTaskReceipt<serde_json::Value>> {
        self.control
            .run_plugin_task_raw_with_cancel(name, args, cancellation_token)
            .await
    }
}

fn encode_plugin_args<Op: lash_core::PluginOperation>(args: Op::Args) -> Result<serde_json::Value> {
    serde_json::to_value(args).map_err(|err| {
        EmbedError::Plugin(lash_core::PluginError::Invoke(format!(
            "invalid {} args: {err}",
            Op::NAME
        )))
    })
}

fn decode_plugin_output<Op: lash_core::PluginOperation>(
    output: serde_json::Value,
) -> Result<Op::Output> {
    serde_json::from_value(output).map_err(|err| {
        EmbedError::Plugin(lash_core::PluginError::Invoke(format!(
            "invalid {} output: {err}",
            Op::NAME
        )))
    })
}

#[derive(Clone)]
pub struct ChildSessionAdmin {
    control: SessionAdmin,
}

impl ChildSessionAdmin {
    pub async fn create_session(&self, request: SessionCreateRequest) -> Result<SessionHandle> {
        self.control.create_child_session(request).await
    }

    pub async fn close_session(&self, session_id: &str) -> Result<()> {
        self.control.close_child_session(session_id).await
    }

    pub async fn activate_managed_session(&self, session_id: &str) -> Result<()> {
        self.control.activate_managed_session(session_id).await
    }
}

#[derive(Clone)]
pub struct InjectionAdmin {
    control: SessionAdmin,
}

impl InjectionAdmin {
    pub async fn inject_turn_input(
        &self,
        turn_id: &str,
        id: Option<String>,
        message: PluginMessage,
    ) -> Result<()> {
        self.control.inject_turn_input(turn_id, id, message).await
    }

    pub async fn inject_turn_inputs_for_turn(
        &self,
        turn_id: &str,
        messages: Vec<lash_core::InjectedTurnInput>,
    ) -> Result<()> {
        self.control
            .inject_turn_inputs_for_turn(turn_id, messages)
            .await
    }
}

#[derive(Clone)]
pub struct ProtocolAdmin {
    control: SessionAdmin,
}

impl ProtocolAdmin {
    pub async fn apply_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtensionHandle,
    ) -> Result<()> {
        self.control
            .apply_protocol_session_extension(extension)
            .await
    }
}
