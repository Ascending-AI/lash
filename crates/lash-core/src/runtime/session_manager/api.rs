use super::*;

#[async_trait::async_trait]
impl crate::plugin::SessionReadService for RuntimeSessionStateService {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, crate::PluginError> {
        self.services.current.snapshot_current().await
    }

    async fn snapshot_session(
        &self,
        session_id: &str,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        self.services
            .current
            .snapshot_session(&self.services.managed, session_id)
            .await
    }

    async fn tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        self.services
            .current
            .tool_catalog(&self.services.managed, session_id)
            .await
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        self.services
            .current
            .shared_tool_catalog(&self.services.managed, session_id)
            .await
    }

    async fn tool_state(&self, session_id: &str) -> Result<crate::ToolState, crate::PluginError> {
        self.services
            .current
            .tool_state(&self.services.managed, session_id)
            .await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionStateService for RuntimeSessionStateService {
    async fn turn_scope(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<crate::ExecutionScope, crate::PluginError> {
        self.services
            .current
            .turn_scope_by_id(&self.services.managed, session_id, turn_id)
            .await
    }

    async fn snapshot_current(&self) -> Result<SessionSnapshot, crate::PluginError> {
        self.services.current.snapshot_current().await
    }

    async fn snapshot_session(
        &self,
        session_id: &str,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        self.services
            .current
            .snapshot_session(&self.services.managed, session_id)
            .await
    }

    async fn tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        self.services
            .current
            .tool_catalog(&self.services.managed, session_id)
            .await
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        self.services
            .current
            .shared_tool_catalog(&self.services.managed, session_id)
            .await
    }

    async fn tool_state(&self, session_id: &str) -> Result<crate::ToolState, crate::PluginError> {
        self.services
            .current
            .tool_state(&self.services.managed, session_id)
            .await
    }

    async fn apply_tool_state(
        &self,
        session_id: &str,
        snapshot: crate::ToolState,
    ) -> Result<u64, crate::PluginError> {
        self.services
            .current
            .apply_tool_state(&self.services.managed, session_id, snapshot)
            .await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionLifecycleService for RuntimeSessionLifecycleService {
    async fn create_session(
        &self,
        request: SessionCreateRequest,
    ) -> Result<SessionHandle, crate::PluginError> {
        Box::pin(self.services.managed.create_session(
            &self.services.current,
            &self.services.usage,
            request,
        ))
        .await
    }

    async fn close_session(&self, session_id: &str) -> Result<(), crate::PluginError> {
        self.services
            .managed
            .close_session(&self.services.current, &self.services.usage, session_id)
            .await
    }

    async fn start_turn(
        &self,
        request: crate::SessionTurnRequest<'_>,
    ) -> Result<AssembledTurn, crate::PluginError> {
        self.services
            .managed
            .start_turn(&self.services.current, &self.services.usage, request)
            .await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionGraphService for RuntimeSessionGraphService {
    async fn append_session_nodes(
        &self,
        session_id: &str,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesResult, crate::PluginError> {
        self.services
            .current
            .append_session_nodes(
                &self.services.managed,
                &self.services.usage,
                &self.services.processes,
                session_id,
                request,
            )
            .await
    }
    async fn emit_trace_event(
        &self,
        context: lash_trace::TraceContext,
        event: lash_trace::TraceEvent,
    ) -> Result<(), crate::PluginError> {
        self.services.current.emit_trace_event(context, event).await
    }
}

#[async_trait::async_trait]
impl crate::plugin::ProcessReadService for RuntimeSessionProcessService {
    async fn list_visible(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.services
            .processes
            .list_process_handles(&self.services.current, session_id, mode, scope)
            .await
    }
}

#[async_trait::async_trait]
impl crate::ProcessService for RuntimeSessionProcessService {
    async fn list_visible_for_attempt(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.services
            .processes
            .list_model_tool_process_handles_for_attempt(&self.services.current, session_id, mode)
            .await
    }

    async fn start_from_request(
        &self,
        session_id: &str,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleSummary, crate::PluginError> {
        let env_ref = match request.env_spec.as_ref() {
            Some(env_spec) => Some(
                crate::persist_process_execution_env(
                    self.services
                        .current
                        .host
                        .core
                        .durability
                        .process_env_store
                        .as_ref(),
                    env_spec,
                )
                .await?,
            ),
            None => None,
        };
        let observers = request.observers.clone();
        let registration = request.into_registration(env_ref);
        let record = self
            .start(
                session_id,
                registration,
                crate::ProcessStartOptions::new().with_initial_observers(observers),
                scope,
            )
            .await?;
        Ok(crate::ProcessHandleSummary::from_record(record))
    }

    async fn start(
        &self,
        session_id: &str,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .start_process(
                &self.services.current,
                &self.services.managed,
                session_id,
                registration,
                options,
                scope,
            )
            .await
    }

    async fn complete_external(
        &self,
        session_id: &str,
        process_id: &str,
        await_output: crate::ProcessAwaitOutput,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessCompletionOutcome, crate::PluginError> {
        self.services
            .processes
            .complete_external_process(
                &self.services.current,
                session_id,
                process_id,
                await_output,
                scope,
            )
            .await
    }

    async fn await_process(
        &self,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        self.services
            .processes
            .await_process(&self.services.current, process_id, scope)
            .await
    }

    async fn list_visible(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.services
            .processes
            .list_process_handles(&self.services.current, session_id, mode, scope)
            .await
    }

    async fn validate_visible(
        &self,
        session_id: &str,
        handle_ids: &[String],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        self.services
            .processes
            .validate_process_handles_observed(
                &self.services.current,
                &self.services.managed,
                session_id,
                handle_ids,
                scope,
            )
            .await
    }

    async fn cancel(
        &self,
        session_id: &str,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .cancel_process(
                &self.services.current,
                &self.services.managed,
                session_id,
                process_id,
                scope,
            )
            .await
    }

    async fn cancel_with_reason(
        &self,
        session_id: &str,
        process_id: &str,
        reason: Option<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .cancel_process_with_reason(
                &self.services.current,
                &self.services.managed,
                session_id,
                process_id,
                reason,
                scope,
            )
            .await
    }

    async fn signal(
        &self,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .signal_process(
                &self.services.current,
                session_id,
                process_id,
                signal_name,
                signal_id,
                payload,
                scope,
            )
            .await
    }

    async fn emit_event(
        &self,
        session_id: &str,
        process_id: &str,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .emit_process_event(
                &self.services.current,
                session_id,
                process_id,
                event_type,
                replay_key,
                payload,
                scope,
            )
            .await
    }

    async fn signal_possessed(
        &self,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .signal_possessed_process(
                &self.services.current,
                session_id,
                process_id,
                signal_name,
                signal_id,
                payload,
                scope,
            )
            .await
    }

    async fn transfer(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: Vec<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        self.services
            .processes
            .transfer_process_handles(
                &self.services.current,
                &self.services.managed,
                from_session_id,
                to_session_id,
                process_ids,
                scope,
            )
            .await
    }
}

#[async_trait::async_trait]
impl crate::ProcessService for ModelToolSessionProcessService {
    async fn list_visible_for_attempt(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.services
            .processes
            .list_model_tool_process_handles_for_attempt(&self.services.current, session_id, mode)
            .await
    }

    async fn start_from_request(
        &self,
        session_id: &str,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleSummary, crate::PluginError> {
        let host = RuntimeSessionProcessService {
            services: Arc::clone(&self.services),
        };
        crate::ProcessService::start_from_request(&host, session_id, request, scope).await
    }

    async fn start(
        &self,
        session_id: &str,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let host = RuntimeSessionProcessService {
            services: Arc::clone(&self.services),
        };
        crate::ProcessService::start(&host, session_id, registration, options, scope).await
    }

    async fn complete_external(
        &self,
        session_id: &str,
        process_id: &str,
        await_output: crate::ProcessAwaitOutput,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessCompletionOutcome, crate::PluginError> {
        let host = RuntimeSessionProcessService {
            services: Arc::clone(&self.services),
        };
        crate::ProcessService::complete_external(&host, session_id, process_id, await_output, scope)
            .await
    }

    async fn await_process(
        &self,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        self.services
            .processes
            .await_process(&self.services.current, process_id, scope)
            .await
    }

    async fn list_visible(
        &self,
        session_id: &str,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.services
            .processes
            .list_model_tool_process_handles(&self.services.current, session_id, mode, scope)
            .await
    }

    async fn validate_visible(
        &self,
        session_id: &str,
        process_ids: &[String],
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        self.services
            .processes
            .validate_model_tool_process_handles(&self.services.current, session_id, process_ids)
            .await
    }

    async fn cancel(
        &self,
        session_id: &str,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .cancel_process(
                &self.services.current,
                &self.services.managed,
                session_id,
                process_id,
                scope,
            )
            .await
    }

    async fn cancel_with_reason(
        &self,
        session_id: &str,
        process_id: &str,
        reason: Option<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .cancel_process_with_reason(
                &self.services.current,
                &self.services.managed,
                session_id,
                process_id,
                reason,
                scope,
            )
            .await
    }

    async fn signal(
        &self,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .validate_model_tool_process_handles(
                &self.services.current,
                session_id,
                &[process_id.to_string()],
            )
            .await?;
        self.services
            .processes
            .signal_possessed_process(
                &self.services.current,
                session_id,
                process_id,
                signal_name,
                signal_id,
                payload,
                scope,
            )
            .await
    }

    async fn emit_event(
        &self,
        session_id: &str,
        process_id: &str,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .emit_process_event(
                &self.services.current,
                session_id,
                process_id,
                event_type,
                replay_key,
                payload,
                scope,
            )
            .await
    }

    async fn signal_possessed(
        &self,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .signal_possessed_process(
                &self.services.current,
                session_id,
                process_id,
                signal_name,
                signal_id,
                payload,
                scope,
            )
            .await
    }

    async fn transfer(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: Vec<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        self.services
            .processes
            .transfer_process_handles(
                &self.services.current,
                &self.services.managed,
                from_session_id,
                to_session_id,
                process_ids,
                scope,
            )
            .await
    }
}
