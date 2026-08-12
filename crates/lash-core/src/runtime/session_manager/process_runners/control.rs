use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct ProcessCommandRunner<'scope> {
    current: &'scope CurrentSessionCapability,
    registry: Arc<dyn crate::ProcessRegistry>,
    parent_invocation: Option<crate::RuntimeInvocation>,
    effect_controller: &'scope dyn crate::RuntimeEffectController,
    turn_cancellation: Option<crate::ProcessTurnCancellation>,
}

pub(crate) fn guard_process_command_in_recorded_body(
    parent_invocation: Option<&crate::RuntimeInvocation>,
    effect_controller: &dyn crate::RuntimeEffectController,
    command: &crate::ProcessCommand,
) -> Result<(), crate::PluginError> {
    if parent_invocation.is_some_and(|invocation| {
        invocation.effect_kind() == Some(crate::RuntimeEffectKind::ToolAttempt)
    }) && effect_controller.journal_addressing()
        == crate::EffectJournalAddressing::OrdinalAddressed
        && !matches!(command, crate::ProcessCommand::List { .. })
    {
        let route = match command {
            crate::ProcessCommand::Start { .. } => "processes().start()",
            crate::ProcessCommand::Await { .. } => "processes().await_process()",
            crate::ProcessCommand::Cancel { .. } => "processes().cancel()",
            crate::ProcessCommand::Signal { .. } => "processes().signal()",
            crate::ProcessCommand::EmitEvent { .. } => "process_events().emit()",
            crate::ProcessCommand::Transfer { .. } => "processes().transfer()",
            crate::ProcessCommand::DeleteSession { .. } => "processes().delete_session()",
            crate::ProcessCommand::List { .. } => unreachable!("list is journal-neutral"),
        };
        return Err(crate::PluginError::Session(format!(
            "ToolContext::{route} is unavailable inside a recorded tool attempt; return a ToolIntent for coordinator execution after the final attempt is committed"
        )));
    }
    Ok(())
}

impl<'scope> ProcessCommandRunner<'scope> {
    fn new(
        current: &'scope CurrentSessionCapability,
        scope: &'scope crate::ProcessOpScope<'scope>,
        unavailable_message: &'static str,
    ) -> Result<Self, crate::PluginError> {
        let Some(registry) = current.host.process_registry.as_ref() else {
            return Err(crate::PluginError::Session(unavailable_message.to_string()));
        };
        let effect_controller = scope.controller();
        Ok(Self {
            current,
            registry: Arc::clone(registry),
            parent_invocation: scope.parent_invocation.clone(),
            effect_controller,
            turn_cancellation: scope.turn_cancellation.clone(),
        })
    }

    fn registry(&self) -> &Arc<dyn crate::ProcessRegistry> {
        &self.registry
    }

    async fn start(
        &self,
        registration: crate::ProcessRegistration,
        observers: Vec<String>,
        execution_context: crate::ProcessExecutionContext,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Start {
                registration,
                observers,
                execution_context: Box::new(execution_context),
            })
            .await?
        {
            crate::ProcessEffectOutcome::Start { record } => Ok(*record),
            _ => Err(wrong_process_outcome("start")),
        }
    }

    async fn await_process(
        &self,
        process_id: &str,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Await {
                process_id: process_id.to_string(),
            })
            .await?
        {
            crate::ProcessEffectOutcome::Await { output } => Ok(*output),
            _ => Err(wrong_process_outcome("await")),
        }
    }

    async fn list(
        &self,
        session_scope: crate::SessionScope,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::List {
                session_scope,
                mode,
            })
            .await?
        {
            crate::ProcessEffectOutcome::List { entries } => Ok(entries),
            _ => Err(wrong_process_outcome("list")),
        }
    }

    async fn cancel(
        &self,
        process_id: &str,
        reason: Option<String>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Cancel {
                process_id: process_id.to_string(),
                reason,
            })
            .await?
        {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => Err(wrong_process_outcome("cancel")),
        }
    }

    async fn signal(
        &self,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let event_type = request.event_type.clone();
        let payload = request.payload.clone();
        let event = match self
            .run(crate::ProcessCommand::Signal {
                process_id: process_id.to_string(),
                signal_name: signal_name.clone(),
                signal_id,
                request,
            })
            .await?
        {
            crate::ProcessEffectOutcome::Signal { event } => *event,
            _ => return Err(wrong_process_outcome("signal")),
        };
        let waiting_ordinal = self
            .registry
            .get_process(process_id)
            .await?
            .and_then(|record| match record.wait {
                Some(crate::WaitState {
                    kind:
                        crate::WaitKind::Signal {
                            name,
                            event_type: wait_event_type,
                            ordinal,
                            ..
                        },
                    ..
                }) if name == signal_name && wait_event_type == event_type => Some(ordinal),
                _ => None,
            });
        let ordinal = match waiting_ordinal {
            Some(ordinal) => ordinal,
            None => {
                self.registry
                    .count_events_through(process_id, &event_type, event.sequence)
                    .await?
            }
        };
        if ordinal > 0 {
            let key = self
                .effect_controller
                .await_event_key(
                    &crate::ExecutionScope::process(process_id),
                    crate::AwaitEventWaitIdentity::process_signal(
                        process_id,
                        &signal_name,
                        ordinal,
                    ),
                )
                .await
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
            let _ = self
                .effect_controller
                .resolve_await_event(&key, crate::Resolution::Ok(payload))
                .await
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        }
        Ok(event)
    }

    async fn emit_event(
        &self,
        process_id: &str,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::EmitEvent {
                process_id: process_id.to_string(),
                request,
            })
            .await?
        {
            crate::ProcessEffectOutcome::EmitEvent { event } => Ok(*event),
            _ => Err(wrong_process_outcome("emit_event")),
        }
    }

    async fn transfer(
        &self,
        from_scope: crate::SessionScope,
        to_scope: crate::SessionScope,
        process_ids: Vec<String>,
    ) -> Result<(), crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Transfer {
                from_scope,
                to_scope,
                process_ids,
            })
            .await?
        {
            crate::ProcessEffectOutcome::Transfer => Ok(()),
            _ => Err(wrong_process_outcome("transfer")),
        }
    }

    async fn run(
        &self,
        command: crate::ProcessCommand,
    ) -> Result<crate::ProcessEffectOutcome, crate::PluginError> {
        guard_process_command_in_recorded_body(
            self.parent_invocation.as_ref(),
            self.effect_controller,
            &command,
        )?;
        let effect_id = command.effect_id();
        let invocation = crate::runtime::causal::process_effect_invocation(
            &self.current.session_id,
            self.parent_invocation.clone(),
            &effect_id,
        );
        let envelope = crate::RuntimeEffectEnvelope::new(
            invocation,
            crate::RuntimeEffectCommand::process(command),
        );
        // Route through the controller explicitly selected by the process
        // operation scope: host-configured for host/API paths, scoped for
        // in-turn paths.
        let mut local_executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&self.registry),
            self.current.host.process_work_driver.clone(),
        );
        if let Some(turn_cancellation) = self.turn_cancellation.clone() {
            local_executor = local_executor.with_process_turn_cancellation(turn_cancellation);
        }
        let outcome = self
            .effect_controller
            .execute_effect(envelope, local_executor)
            .await?;
        outcome.into_process().map_err(crate::PluginError::from)
    }
}

fn wrong_process_outcome(op: &str) -> crate::PluginError {
    crate::PluginError::Session(format!("process {op} returned the wrong outcome"))
}

impl ProcessCapability {
    fn command_runner<'scope>(
        &self,
        current: &'scope CurrentSessionCapability,
        scope: &'scope crate::ProcessOpScope<'scope>,
    ) -> Result<ProcessCommandRunner<'scope>, crate::PluginError> {
        ProcessCommandRunner::new(
            current,
            scope,
            "process registry is unavailable in this runtime",
        )
    }

    fn process_scope_for_op(
        &self,
        session_id: &str,
        agent_frame_id: Option<&str>,
    ) -> crate::SessionScope {
        agent_frame_id
            .filter(|frame_id| !frame_id.is_empty())
            .map(|frame_id| crate::SessionScope::for_agent_frame(session_id, frame_id))
            .unwrap_or_else(|| crate::SessionScope::new(session_id))
    }

    fn current_execution_env_spec(
        &self,
        current: &CurrentSessionCapability,
    ) -> crate::ProcessExecutionEnvSpec {
        let state = current.snapshot.to_runtime_state();
        state.process_execution_env_spec(&current.policy)
    }

    async fn capture_execution_env_ref(
        &self,
        current: &CurrentSessionCapability,
        registration: &crate::ProcessRegistration,
    ) -> Result<Option<crate::ProcessExecutionEnvRef>, crate::PluginError> {
        if let Some(env_ref) = registration.env_ref.clone() {
            return Ok(Some(env_ref));
        }
        match registration.input.as_ref() {
            crate::ProcessInput::ToolCall { .. } | crate::ProcessInput::Engine { .. } => {
                let spec = self.current_execution_env_spec(current);
                crate::persist_process_execution_env(
                    current.host.core.durability.process_env_store.as_ref(),
                    &spec,
                )
                .await
                .map(Some)
            }
            crate::ProcessInput::External { .. } | crate::ProcessInput::SessionTurn { .. } => {
                Ok(None)
            }
        }
    }

    pub(in crate::runtime::session_manager) async fn start_process(
        &self,
        current: &CurrentSessionCapability,
        managed: &ManagedSessionCapability,
        session_id: &str,
        registration: crate::ProcessRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.ensure_known_process_session(current, managed, session_id)
            .await?;
        self.mark_current_process_sync_needed(current, session_id);
        let creator_scope = self.process_scope_for_op(session_id, scope.agent_frame_id());
        let caused_by = scope
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::causal_ref);
        let env_ref = self
            .capture_execution_env_ref(current, &registration)
            .await?;
        // Children started *by a process* inherit the chain's provenance (the
        // run context provides it); in-session starts stamp the creating
        // session. Wake routing and observer membership are independent: only
        // the explicit `options.initial_observers` set creates edges. The ephemeral
        // execution scope must never appear on a record.
        let (originator, wake_session_id) = match options.spawn_provenance.clone() {
            Some(spawn) => (spawn.originator, spawn.wake_session_id),
            None => (
                crate::ProcessOriginator::session(creator_scope.clone()),
                Some(creator_scope.session_id.clone()),
            ),
        };
        let registration = registration
            .with_process_provenance(
                crate::ProcessProvenance::new(originator).with_caused_by(caused_by),
            )
            .with_execution_env_ref(env_ref)
            .with_wake_session_id(wake_session_id);
        let registration = self
            .prepare_process_environment(current, session_id, registration)
            .await?;
        let execution_context = options.execution_context(&scope);
        let runner = ProcessCommandRunner::new(
            current,
            &scope,
            "processes are unavailable in this runtime",
        )?;
        runner
            .start(registration, options.initial_observers, execution_context)
            .await
    }

    async fn prepare_process_environment(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        registration: crate::ProcessRegistration,
    ) -> Result<crate::ProcessRegistration, crate::PluginError> {
        let crate::ProcessInput::Engine { kind, payload } = registration.input.as_ref() else {
            return Ok(registration);
        };
        let Some(env_ref) = registration.env_ref.as_ref() else {
            return Err(crate::PluginError::Session(format!(
                "process `{}` requires a captured execution env",
                registration.id
            )));
        };
        let env_spec = crate::load_process_execution_env(
            current.host.core.durability.process_env_store.as_ref(),
            env_ref,
        )
        .await?;
        let engine = current.host.core.process_engines.require(kind)?;
        let tool_catalog = current.plugins.resolved_tool_catalog(session_id)?;
        engine
            .validate_start(
                crate::ProcessEngineValidationContext::new(
                    current.plugins.host(),
                    tool_catalog,
                    current.host.process_registry.is_some(),
                ),
                payload,
                Some(&env_spec),
            )
            .await?;
        let identity = engine.identity(payload);
        Ok(registration.with_identity(identity))
    }

    pub(in crate::runtime::session_manager) async fn await_process(
        &self,
        current: &CurrentSessionCapability,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        self.command_runner(current, &scope)?
            .await_process(process_id)
            .await
    }

    /// Write the terminal outcome for an Externally-Owned process the session
    /// observes (ADR 0019). This is the "external actor calling
    /// `complete_process`" closure path: a `shell.start` detach records its
    /// immediately-terminal launch fact through here. Only Externally-Owned rows
    /// may be completed this way — an OwnerBound or Rerunnable row has a lash
    /// execution owner as its single terminal writer, so completing it out of
    /// band is rejected.
    pub(in crate::runtime::session_manager) async fn complete_external_process(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        process_id: &str,
        await_output: crate::ProcessAwaitOutput,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessCompletionOutcome, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        let session_scope = self.process_scope_for_op(session_id, scope.agent_frame_id());
        // Session-visibility authorization: the caller must observe the row.
        if !runner
            .registry()
            .is_observer(&session_scope.session_id, process_id)
            .await?
        {
            return Err(crate::PluginError::Session(format!(
                "process handle `{process_id}` is not visible in this session"
            )));
        }
        // The disposition check (only ExternallyOwned rows may be completed out
        // of band) now lives inside the registry's completion operation, keyed on
        // this explicit authority, so it is enforced uniformly across backends
        // rather than only here (ADR 0027).
        self.mark_current_process_sync_needed(current, session_id);
        runner
            .registry()
            .complete_process(
                process_id,
                await_output,
                crate::ProcessCompletionAuthority::ExternalOwner,
            )
            .await
    }

    pub(in crate::runtime::session_manager) async fn list_process_handles(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.command_runner(current, &scope)?
            .list(
                self.process_scope_for_op(session_id, scope.agent_frame_id()),
                mode,
            )
            .await
    }

    pub(in crate::runtime::session_manager) async fn list_model_tool_process_handles(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let records = self
            .list_process_handles(current, session_id, mode, scope)
            .await?;
        Ok(Self::narrow_tool_visible_records(
            current, session_id, records,
        ))
    }

    pub(in crate::runtime::session_manager) async fn list_model_tool_process_handles_for_attempt(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let registry = current.host.process_registry.as_ref().ok_or_else(|| {
            crate::PluginError::Session(
                "process registry is unavailable in this runtime".to_string(),
            )
        })?;
        let records = match mode {
            crate::ProcessListMode::Live => registry.list_live_observed_by(session_id).await?,
            crate::ProcessListMode::All => registry.list_observed_by(session_id).await?,
        };
        Ok(Self::narrow_tool_visible_records(
            current, session_id, records,
        ))
    }

    pub(in crate::runtime::session_manager) async fn cancel_process(
        &self,
        current: &CurrentSessionCapability,
        managed: &ManagedSessionCapability,
        session_id: &str,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        if runner.registry().get_process(process_id).await?.is_none() {
            return Err(crate::PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let _ = (managed, session_id);
        runner
            .cancel(process_id, Some("requested by host".to_string()))
            .await
    }

    pub(in crate::runtime::session_manager) async fn cancel_process_with_reason(
        &self,
        current: &CurrentSessionCapability,
        managed: &ManagedSessionCapability,
        session_id: &str,
        process_id: &str,
        reason: Option<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        if runner.registry().get_process(process_id).await?.is_none() {
            return Err(crate::PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let _ = (managed, session_id);
        runner.cancel(process_id, reason).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn emit_process_event(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        process_id: &str,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.validate_model_tool_process_handles(current, session_id, &[process_id.to_string()])
            .await?;
        let request =
            crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(replay_key);
        self.command_runner(current, &scope)?
            .emit_event(process_id, request)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn signal_process(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.signal_process_with_visibility(
            current,
            session_id,
            process_id,
            signal_name,
            signal_id,
            payload,
            scope,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn signal_possessed_process(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.signal_process_with_visibility(
            current,
            session_id,
            process_id,
            signal_name,
            signal_id,
            payload,
            scope,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn signal_process_with_visibility(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
        require_session_visibility: bool,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        if require_session_visibility {
            self.validate_process_handles_observed_inner(
                current,
                session_id,
                &[process_id.to_string()],
            )
            .await?;
        }
        let record = runner
            .registry()
            .get_process(process_id)
            .await?
            .ok_or_else(|| {
                crate::PluginError::Session(format!("unknown process `{process_id}`"))
            })?;
        if record.is_terminal() {
            return Err(crate::PluginError::ProcessAlreadyTerminal {
                process_id: process_id.to_string(),
                status: record.status,
            });
        }
        let event_type = crate::process_signal_event_type(&signal_name)?;
        let request = crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
            format!("process:{process_id}:signal.{signal_name}:{signal_id}"),
        );
        runner
            .signal(process_id, signal_name, signal_id, request)
            .await
    }

    pub(in crate::runtime::session_manager) async fn validate_process_handles_observed(
        &self,
        current: &CurrentSessionCapability,
        _managed: &ManagedSessionCapability,
        session_id: &str,
        handle_ids: &[String],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        let _ = scope;
        self.validate_process_handles_observed_inner(current, session_id, handle_ids)
            .await
    }

    pub(in crate::runtime::session_manager) async fn validate_model_tool_process_handles(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        handle_ids: &[String],
    ) -> Result<(), crate::PluginError> {
        self.validate_process_handles_observed_inner(current, session_id, handle_ids)
            .await?;
        self.validate_tool_filter(current, session_id, handle_ids)
            .await
    }

    pub(in crate::runtime::session_manager) async fn transfer_process_handles(
        &self,
        current: &CurrentSessionCapability,
        _managed: &ManagedSessionCapability,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: Vec<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        if process_ids.is_empty() {
            return Ok(());
        }
        self.command_runner(current, &scope)?
            .transfer(
                self.process_scope_for_op(from_session_id, scope.agent_frame_id()),
                self.process_scope_for_op(to_session_id, None),
                process_ids,
            )
            .await
    }

    async fn ensure_known_process_session(
        &self,
        current: &CurrentSessionCapability,
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<(), crate::PluginError> {
        if session_id == current.session_id
            || managed.registry.lock().await.contains_key(session_id)
        {
            return Ok(());
        }
        Err(crate::PluginError::Session(format!(
            "unknown session `{session_id}`"
        )))
    }

    fn mark_current_process_sync_needed(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
    ) {
        if session_id == current.session_id {
            self.sync_needed.store(true, Ordering::Release);
        }
    }

    fn narrow_tool_visible_records(
        current: &CurrentSessionCapability,
        session_id: &str,
        records: Vec<crate::ProcessRecord>,
    ) -> Vec<crate::ProcessRecord> {
        let Some(filter) = current
            .host
            .core
            .control
            .process_tool_visibility_filter
            .as_ref()
        else {
            return records;
        };
        let candidates = records
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        let returned_candidates = candidates
            .iter()
            .filter(|process_id| {
                filter
                    .narrow(&session_id.to_string(), std::slice::from_ref(process_id))
                    .iter()
                    .any(|returned| returned == *process_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let returned = returned_candidates.iter().cloned().collect::<HashSet<_>>();
        let outcome = if returned_candidates.len() == candidates.len() {
            "unchanged"
        } else {
            "narrowed"
        };
        tracing::info!(
            target: "lash::process_tool_visibility",
            %session_id,
            operation = "list",
            candidates = ?candidates,
            returned = ?returned_candidates,
            policy = "host_filter",
            %outcome,
            "model process-tool visibility decision"
        );
        records
            .into_iter()
            .filter(|record| returned.contains(&record.id))
            .collect()
    }

    async fn validate_process_handles_observed_inner(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        process_ids: &[String],
    ) -> Result<(), crate::PluginError> {
        if process_ids.is_empty() {
            return Ok(());
        }
        let registry = current.host.process_registry.as_ref().ok_or_else(|| {
            crate::PluginError::Session("process registry is unavailable in this runtime".into())
        })?;
        for process_id in process_ids {
            match registry.is_observer(session_id, process_id).await {
                Ok(true) | Err(crate::PluginError::ProcessNoLongerRetained { .. }) => {}
                Ok(false) => return Err(process_visibility_miss(process_id)),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn validate_tool_filter(
        &self,
        current: &CurrentSessionCapability,
        session_id: &str,
        process_ids: &[String],
    ) -> Result<(), crate::PluginError> {
        let Some(filter) = current
            .host
            .core
            .control
            .process_tool_visibility_filter
            .as_ref()
        else {
            return Ok(());
        };
        for process_id in process_ids {
            let returned = filter.narrow(&session_id.to_string(), std::slice::from_ref(process_id));
            let allowed = returned.iter().any(|returned| returned == process_id);
            tracing::info!(
                target: "lash::process_tool_visibility",
                %session_id,
                operation = "target",
                candidate = %process_id,
                returned = ?returned,
                policy = "host_filter",
                outcome = if allowed { "allowed" } else { "denied" },
                "model process-tool visibility decision"
            );
            if !allowed {
                return Err(process_visibility_miss(process_id));
            }
        }
        Ok(())
    }
}

fn process_visibility_miss(process_id: &str) -> crate::PluginError {
    crate::PluginError::ProcessNotVisible {
        process_id: process_id.to_string(),
    }
}
