use super::*;
use lash_vocabulary::identity::TurnId;

impl crate::core_internal::RuntimeService for RuntimeSessionStateService {}

#[async_trait::async_trait]
impl crate::plugin::SessionReadService for RuntimeSessionStateService {
    async fn committed_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<
        Option<crate::transcript::CommittedTurnWithCalls>,
        crate::plugin::CommittedTurnReadError,
    > {
        let current = &self.services.current;
        let decoders = current.plugins.plugin_host().transcript_decoders()?;
        let store = if let Some(store) = current
            .session()
            .filter(|bound| bound.session_id == *session_id)
            .and_then(|bound| bound.store.as_ref())
        {
            store.clone()
        } else {
            crate::store::SessionStore::new(
                current.host.core.session_store_factory(),
                session_id.clone(),
            )?
        };
        crate::runtime::durable::services::RuntimeTurnServices::committed_turn(
            &current.host.core.control.effect_host,
            &store,
            turn_id,
            &decoders,
        )
        .await
    }

    async fn snapshot_current(
        &self,
    ) -> Result<SessionSnapshot, lash_execution_contracts::plugin_error::PluginError> {
        self.services.current.snapshot_current().await
    }

    async fn snapshot_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot, lash_execution_contracts::plugin_error::PluginError> {
        self.services.current.snapshot_session(session_id).await
    }

    async fn tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, lash_execution_contracts::plugin_error::PluginError> {
        self.services.current.tool_catalog(session_id).await
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<Vec<serde_json::Value>>, lash_execution_contracts::plugin_error::PluginError>
    {
        self.services.current.shared_tool_catalog(session_id).await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionStateService for RuntimeSessionStateService {
    async fn turn_scope(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, lash_execution_contracts::plugin_error::PluginError> {
        self.services
            .current
            .turn_scope_by_id(session_id, turn_id)
            .await
    }
}

impl crate::core_internal::RuntimeService for RuntimeSessionLifecycleService {}

#[async_trait::async_trait]
impl crate::plugin::SessionLifecycleService for RuntimeSessionLifecycleService {
    async fn create_session(
        &self,
        request: SessionCreateRequest,
    ) -> Result<SessionHandle, lash_execution_contracts::plugin_error::PluginError> {
        Box::pin(super::session_init::create_session(
            &self.services.current,
            request,
        ))
        .await
    }
}

impl crate::core_internal::RuntimeService for RuntimeSessionGraphService {}

#[async_trait::async_trait]
impl crate::plugin::SessionGraphService for RuntimeSessionGraphService {
    async fn append_session_nodes(
        &self,
        session_id: &SessionId,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, lash_execution_contracts::plugin_error::PluginError>
    {
        Box::pin(self.services.current.append_session_nodes(
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
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        self.services.current.emit_trace_event(context, event).await
    }
}

#[async_trait::async_trait]
impl crate::plugin::ProcessReadService for RuntimeSessionProcessService {
    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: lash_execution_contracts::process::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        Vec<lash_execution_contracts::process::ObservedProcess>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
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
        owner: &lash_vocabulary::identity::RuntimeOwner,
        mode: lash_execution_contracts::process::ProcessListMode,
    ) -> Result<
        Vec<lash_execution_contracts::process::ObservedProcess>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        if self
            .visibility
            .consults_filter(ProcessVisibilityOperation::ListVisibleForAttempt)
        {
            self.services
                .processes
                .list_model_tool_process_handles_for_attempt(&self.services.current, owner, mode)
                .await
        } else {
            self.services
                .processes
                .list_process_handles_for_attempt(&self.services.current, owner, mode)
                .await
        }
    }

    async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: lash_execution_contracts::process::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        lash_execution_contracts::process::ProcessHandleView,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let observers = request.observers.clone();
        let registration = request.into_registration();
        let record = self
            .start(
                session_id,
                registration,
                lash_execution_contracts::process::ProcessStartOptions::new()
                    .with_initial_observers(observers),
                scope,
            )
            .await?;
        Ok(lash_execution_contracts::process::ProcessHandleView::from_observed(record))
    }

    async fn stage_recorded_start(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        request: lash_execution_contracts::process::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::StagedProcessStart, lash_execution_contracts::plugin_error::PluginError>
    {
        self.services
            .processes
            .stage_recorded_start_process(&self.services.current, owner, request, scope)
            .await
    }

    async fn stage_bound(
        &self,
        registration: lash_execution_contracts::process::ProcessStartRegistration,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::StagedProcessStart, lash_execution_contracts::plugin_error::PluginError>
    {
        self.services
            .processes
            .stage_bound_process(&self.services.current, registration, scope)
            .await
    }

    async fn cancel_bound(
        &self,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        self.services
            .processes
            .cancel_bound_process(&self.services.current, process_id, scope)
            .await
    }

    async fn start(
        &self,
        session_id: &SessionId,
        registration: lash_execution_contracts::process::ProcessStartRegistration,
        options: lash_execution_contracts::process::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        lash_execution_contracts::process::ObservedProcess,
        lash_execution_contracts::plugin_error::PluginError,
    > {
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

    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        lash_execution_contracts::process::ProcessAwaitOutput,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.services
            .processes
            .await_process(&self.services.current, process_id, scope)
            .await
    }

    async fn await_process_ref(
        &self,
        process_id: &lash_vocabulary::identity::ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        lash_execution_contracts::process::ProcessAwaitOutput,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.services
            .processes
            .await_process_ref(&self.services.current, process_id.clone(), scope)
            .await
    }

    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, lash_execution_contracts::plugin_error::PluginError> {
        match self.services.current.host.process_registry() {
            Some(registry) => registry.abandon_consumer_hold(key, owner).await,
            None => Ok(Vec::new()),
        }
    }

    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        let registry = self
            .services
            .current
            .host
            .process_registry()
            .ok_or_else(|| {
                lash_execution_contracts::plugin_error::PluginError::Session(
                    "process registry is unavailable in this runtime".to_string(),
                )
            })?;
        registry.release_consumer_hold(process_id, key).await
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: lash_execution_contracts::process::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        Vec<lash_execution_contracts::process::ObservedProcess>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
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
        owner: &lash_vocabulary::identity::RuntimeOwner,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        if self
            .visibility
            .consults_filter(ProcessVisibilityOperation::ValidateVisible)
        {
            self.services
                .processes
                .validate_model_tool_process_handles(
                    &self.services.current,
                    owner,
                    handle_ids,
                    scope,
                )
                .await
        } else {
            self.services
                .processes
                .validate_process_handles_observed(&self.services.current, owner, handle_ids, scope)
                .await
        }
    }

    async fn cancel(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        lash_execution_contracts::process::ObservedProcess,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.services
            .processes
            .cancel_process(&self.services.current, owner, process_id, scope)
            .await
    }

    async fn cancel_recorded_intent(
        &self,
        _owner: &lash_vocabulary::identity::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<
        lash_execution_contracts::process::ObservedProcess,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.services
            .processes
            .cancel_recorded_intent(&self.services.current, process_id, identity, scope)
            .await
    }

    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
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
