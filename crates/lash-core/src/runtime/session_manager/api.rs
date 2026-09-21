use super::*;
use crate::TurnId;

#[async_trait::async_trait]
impl crate::plugin::SessionReadService for RuntimeSessionStateService {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, crate::PluginError> {
        self.services.current.snapshot_current().await
    }

    async fn snapshot_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        self.services.current.snapshot_session(session_id).await
    }

    async fn tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        self.services.current.tool_catalog(session_id).await
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        self.services.current.shared_tool_catalog(session_id).await
    }

    async fn tool_state(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::ToolState, crate::PluginError> {
        self.services.current.tool_state(session_id).await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionStateService for RuntimeSessionStateService {
    async fn turn_scope(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, crate::PluginError> {
        self.services
            .current
            .turn_scope_by_id(session_id, turn_id)
            .await
    }

    async fn snapshot_current(&self) -> Result<SessionSnapshot, crate::PluginError> {
        self.services.current.snapshot_current().await
    }

    async fn snapshot_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        self.services.current.snapshot_session(session_id).await
    }

    async fn tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        self.services.current.tool_catalog(session_id).await
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        self.services.current.shared_tool_catalog(session_id).await
    }

    async fn tool_state(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::ToolState, crate::PluginError> {
        self.services.current.tool_state(session_id).await
    }

    async fn apply_tool_state(
        &self,
        session_id: &SessionId,
        snapshot: crate::ToolState,
    ) -> Result<u64, crate::PluginError> {
        self.services
            .current
            .apply_tool_state(session_id, snapshot)
            .await
    }

    async fn session_plugin_init(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::SessionPluginInit, crate::PluginError> {
        self.services.current.plugin_init_by_id(session_id).await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionLifecycleService for RuntimeSessionLifecycleService {
    async fn create_session(
        &self,
        request: SessionCreateRequest,
    ) -> Result<SessionHandle, crate::PluginError> {
        Box::pin(super::session_init::create_session(
            &self.services.current,
            request,
        ))
        .await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionGraphService for RuntimeSessionGraphService {
    async fn append_session_nodes(
        &self,
        session_id: &SessionId,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, crate::PluginError> {
        Box::pin(self.services.current.append_session_nodes(
            &self.services.usage,
            &self.services.processes,
            session_id,
            request,
        ))
        .await
    }
    async fn emit_trace_event(
        &self,
        context: lash_trace::TraceContext,
        event: lash_trace::TraceEvent,
    ) -> Result<(), crate::PluginError> {
        self.services.current.emit_trace_event(context, event).await
    }

    async fn switch_agent_frame(
        &self,
        session_id: &SessionId,
        request: crate::SwitchAgentFrameRequest,
    ) -> Result<crate::OpenAgentFrameResult, crate::PluginError> {
        self.services
            .current
            .switch_agent_frame(session_id, &request)
            .await
    }
}

#[async_trait::async_trait]
impl crate::plugin::ProcessReadService for RuntimeSessionProcessService {
    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        if self
            .visibility
            .consults_filter(ProcessVisibilityOperation::ListVisible)
        {
            self.services
                .processes
                .list_model_tool_process_handles(&self.services.current, session_id, mode, scope)
                .await
        } else {
            self.services
                .processes
                .list_process_handles(&self.services.current, session_id, mode, scope)
                .await
        }
    }
}

#[async_trait::async_trait]
impl crate::ProcessService for RuntimeSessionProcessService {
    async fn list_visible_for_attempt(
        &self,
        session_id: &SessionId,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        if self
            .visibility
            .consults_filter(ProcessVisibilityOperation::ListVisibleForAttempt)
        {
            self.services
                .processes
                .list_model_tool_process_handles_for_attempt(
                    &self.services.current,
                    session_id,
                    mode,
                )
                .await
        } else {
            self.services
                .processes
                .list_process_handles_for_attempt(&self.services.current, session_id, mode)
                .await
        }
    }

    async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        let env_spec = request.env_spec.clone();
        let observers = request.observers.clone();
        let registration = request.into_registration(None);
        let record = self
            .start(
                session_id,
                registration,
                crate::ProcessStartOptions::new()
                    .with_initial_observers(observers)
                    .with_env_spec(env_spec),
                scope,
            )
            .await?;
        Ok(crate::ProcessHandleView::from_record(record))
    }

    async fn start_from_recorded_intent(
        &self,
        session_id: &SessionId,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        let record = self
            .services
            .processes
            .start_process_from_recorded_intent(&self.services.current, session_id, request, scope)
            .await?;
        Ok(crate::ProcessHandleView::from_record(record))
    }

    async fn recorded_max_attempts(
        &self,
        session_id: &SessionId,
        process_id: &crate::ProcessId,
    ) -> Result<Option<u32>, crate::PluginError> {
        let _ = session_id;
        self.services
            .processes
            .recorded_max_attempts(&self.services.current, process_id)
            .await
    }

    async fn start(
        &self,
        session_id: &SessionId,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .start_process(
                &self.services.current,
                session_id,
                registration,
                options,
                scope,
            )
            .await
    }

    async fn complete_external(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
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

    async fn report_caller_departure(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .report_process_caller_departure(&self.services.current, session_id, process_id)
            .await
    }

    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        self.services
            .processes
            .await_process(&self.services.current, process_id, scope)
            .await
    }

    async fn await_process_ref(
        &self,
        process_ref: &crate::ProcessRef,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        self.services
            .processes
            .await_process_ref(&self.services.current, process_ref.clone(), scope)
            .await
    }

    async fn attach_process_terminal(
        &self,
        process_ref: &crate::ProcessRef,
        key: &crate::AwaitEventKey,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        self.services
            .processes
            .attach_process_terminal(
                &self.services.current,
                process_ref.clone(),
                key.clone(),
                scope,
            )
            .await
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        if self
            .visibility
            .consults_filter(ProcessVisibilityOperation::ListVisible)
        {
            self.services
                .processes
                .list_model_tool_process_handles(&self.services.current, session_id, mode, scope)
                .await
        } else {
            self.services
                .processes
                .list_process_handles(&self.services.current, session_id, mode, scope)
                .await
        }
    }

    async fn validate_visible(
        &self,
        session_id: &SessionId,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        if self
            .visibility
            .consults_filter(ProcessVisibilityOperation::ValidateVisible)
        {
            self.services
                .processes
                .validate_model_tool_process_handles(&self.services.current, session_id, handle_ids)
                .await
        } else {
            self.services
                .processes
                .validate_process_handles_observed(
                    &self.services.current,
                    session_id,
                    handle_ids,
                    scope,
                )
                .await
        }
    }

    async fn validate_visible_refs(
        &self,
        session_id: &SessionId,
        process_refs: &[crate::ProcessRef],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        let process_ids = process_refs
            .iter()
            .map(|process_ref| process_ref.process_id.clone())
            .collect::<Vec<_>>();
        self.validate_visible(session_id, &process_ids, scope)
            .await?;
        let registry = self
            .services
            .current
            .host
            .process_registry()
            .ok_or_else(|| {
                crate::PluginError::Session("process registry unavailable".to_string())
            })?;
        for process_ref in process_refs {
            registry.get_process_ref(process_ref).await?;
        }
        Ok(())
    }

    async fn cancel(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .cancel_process(&self.services.current, session_id, process_id, scope)
            .await
    }

    async fn cancel_recorded_intent(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.services
            .processes
            .cancel_recorded_intent(&self.services.current, process_id, identity, scope)
            .await
    }

    async fn signal_recorded_intent(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .signal_recorded_intent(
                &self.services.current,
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
        session_id: &SessionId,
        process_id: &ProcessId,
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

    async fn emit_event_recorded_intent(
        &self,
        _session_id: &SessionId,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.services
            .processes
            .emit_event_recorded_intent(
                &self.services.current,
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
        session_id: &SessionId,
        process_id: &ProcessId,
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
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        self.services
            .processes
            .transfer_process_handles(
                &self.services.current,
                from_session_id,
                to_session_id,
                process_ids,
                scope,
            )
            .await
    }
}
