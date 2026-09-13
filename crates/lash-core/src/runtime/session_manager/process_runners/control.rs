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
    effect_controller_handle: crate::runtime::RuntimeEffectControllerHandle<'scope>,
    turn_cancellation: Option<crate::ProcessTurnCancellation>,
}

impl<'scope> ProcessCommandRunner<'scope> {
    fn new(
        current: &'scope CurrentSessionCapability,
        scope: &'scope crate::ProcessOpScope<'scope>,
        unavailable_message: &'static str,
    ) -> Result<Self, crate::PluginError> {
        let Some(registry) = current.host.process_registry() else {
            return Err(crate::PluginError::Session(unavailable_message.to_string()));
        };
        let effect_controller = scope.controller();
        Ok(Self {
            current,
            registry: Arc::clone(registry),
            parent_invocation: scope.parent_invocation.clone(),
            effect_controller,
            effect_controller_handle: scope.effect_controller.clone_scoped(),
            turn_cancellation: scope.turn_cancellation.clone(),
        })
    }

    fn registry(&self) -> &Arc<dyn crate::ProcessRegistry> {
        &self.registry
    }

    async fn start(
        &self,
        registration: crate::ProcessRegistration,
        observers: Vec<SessionId>,
        env_spec: Option<crate::ProcessExecutionEnvSpec>,
        execution_context: crate::ProcessExecutionContext,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Start {
                registration,
                observers,
                env_spec,
                execution_context: Box::new(execution_context),
            })
            .await?
        {
            crate::ProcessEffectOutcome::Start { record } => Ok(*record),
            _ => Err(wrong_process_outcome("start")),
        }
    }

    async fn await_process_ref(
        &self,
        process_ref: crate::ProcessRef,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Await { process_ref })
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

    async fn cancel_named(
        &self,
        process_id: &ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let command = match self.registry.resolve_process_ref(process_id).await {
            Ok(process_ref) => crate::ProcessCommand::Cancel {
                process_ref,
                origin,
                requester,
                attribution,
            },
            Err(refusal @ crate::PluginError::ProcessUnknown { .. })
            | Err(refusal @ crate::PluginError::ProcessNoLongerRetained { .. }) => {
                crate::ProcessCommand::CancelRefused {
                    process_id: process_id.clone(),
                    origin,
                    requester,
                    refusal,
                }
            }
            Err(error) => return Err(error),
        };
        match self.run(command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            crate::ProcessEffectOutcome::CancelRefused { refusal } => Err(refusal),
            _ => Err(wrong_process_outcome("cancel")),
        }
    }

    async fn parent_end(
        &self,
        identity: crate::ToolIntentIdentity,
        process_id: ProcessId,
        policy: crate::ProcessParentEndPolicy,
    ) -> Result<crate::ToolIntentParentEndOutcome, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::ParentEnd {
                identity,
                process_id,
                policy,
            })
            .await?
        {
            crate::ProcessEffectOutcome::ParentEnd { outcome } => Ok(*outcome),
            _ => Err(wrong_process_outcome("parent_end")),
        }
    }

    async fn signal(
        &self,
        process_ref: crate::ProcessRef,
        signal_name: String,
        signal_id: String,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Signal {
                process_ref,
                signal_name,
                signal_id,
                request,
            })
            .await?
        {
            crate::ProcessEffectOutcome::Signal { event } => Ok(*event),
            _ => Err(wrong_process_outcome("signal")),
        }
    }

    async fn signal_recorded(
        &self,
        process_ref: crate::ProcessRef,
        signal_name: String,
        signal_id: String,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.signal(process_ref, signal_name, signal_id, request)
            .await
    }

    async fn emit_event(
        &self,
        process_id: &ProcessId,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::EmitEvent {
                process_id: ProcessId::from(process_id.to_string()),
                request,
            })
            .await?
        {
            crate::ProcessEffectOutcome::EmitEvent {
                event,
                wake_delivery,
            } => {
                crate::tool_provider::process_events::enqueue_wake_delivery(
                    Arc::clone(&self.registry),
                    self.current.store.clone(),
                    self.current.host.session_store_factory.as_ref(),
                    wake_delivery.map(|delivery| *delivery),
                    None,
                    Arc::clone(self.current.host.queued_work()),
                    self.current.host.core.control.process_wake_delivery_policy,
                    Arc::clone(&self.current.host.core.clock),
                )
                .await?;
                Ok(*event)
            }
            _ => Err(wrong_process_outcome("emit_event")),
        }
    }

    async fn transfer(
        &self,
        from_scope: crate::SessionScope,
        to_scope: crate::SessionScope,
        process_ids: Vec<ProcessId>,
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
        let effect_id = command.effect_id();
        let scoped = self.effect_controller_handle.scoped();
        let attribution = self
            .parent_invocation
            .as_ref()
            .map(|parent| parent.attribution.clone())
            .unwrap_or_else(|| {
                scoped
                    .execution_scope()
                    .session_id()
                    .map(crate::RuntimeAttribution::for_session)
                    .unwrap_or_else(crate::RuntimeAttribution::none)
            });
        let invocation = crate::runtime::causal::process_effect_invocation(
            scoped.execution_scope(),
            attribution,
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
        let (owned_controller, task_requests): (
            Arc<dyn crate::RuntimeEffectController>,
            Option<
                tokio::sync::mpsc::UnboundedReceiver<
                    crate::runtime::effect::EffectControllerTaskRequest,
                >,
            >,
        ) = if let Some(owned) = scoped.owned_controller() {
            (owned, None)
        } else {
            let (proxy, requests) = crate::runtime::effect::EffectTaskController::scoped(
                self.effect_controller,
                scoped.execution_scope().clone(),
            )
            .map_err(crate::RuntimeEffectControllerError::from)?;
            (
                proxy
                    .owned_controller()
                    .expect("effect-task proxy owns its controller"),
                Some(requests),
            )
        };
        let mut local_executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&self.registry),
            self.current
                .host
                .process_work()
                .cloned()
                .expect("process service requires process-work wiring"),
        )
        .with_process_env_store(Arc::clone(
            &self.current.host.core.durability.process_env_store,
        ))
        .with_process_effect_controller(owned_controller);
        if let Some(turn_cancellation) = self.turn_cancellation.clone() {
            local_executor = local_executor.with_process_turn_cancellation(turn_cancellation);
        }
        let outcome = if let Some(task_requests) = task_requests {
            crate::runtime::effect::drive_effect_controller_task(
                self.effect_controller,
                scoped.execution_scope().clone(),
                envelope,
                local_executor,
                task_requests,
            )
            .await?
        } else {
            scoped.execute_effect(envelope, local_executor).await?
        };
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
        session_id: &SessionId,
        agent_frame_id: Option<&crate::FrameNodeId>,
    ) -> crate::SessionScope {
        agent_frame_id
            .map(|frame_id| crate::SessionScope::for_agent_frame(session_id, frame_id.clone()))
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
        session_id: &SessionId,
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
            .start(
                registration,
                options.initial_observers.into_iter().collect(),
                None,
                execution_context,
            )
            .await
    }

    /// Builds a start command only from the recorded intent payload and its
    /// structural parent invocation, then crosses the journal immediately.
    pub(in crate::runtime::session_manager) async fn start_process_from_recorded_intent(
        &self,
        current: &CurrentSessionCapability,
        session_id: &SessionId,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.mark_current_process_sync_needed(current, session_id);
        let caused_by = scope
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::causal_ref);
        let env_spec = request.env_spec.clone();
        let observers = request.observers.clone();
        let originator = request.originator.clone();
        let registration = request.into_registration(None).with_process_provenance(
            crate::ProcessProvenance::new(originator).with_caused_by(caused_by),
        );
        // A recorded intent declares its own execution env, so the engine gate
        // runs against the recorded spec instead of a stored env ref. It must
        // run here: once the start command crosses the journal the entry is
        // committed and replays forever.
        let registration =
            self.admit_and_stamp_engine_start(current, registration, env_spec.as_ref())?;
        let options = crate::ProcessStartOptions::new().with_initial_observers(observers);
        let execution_context = options.execution_context(&scope);
        self.command_runner(current, &scope)?
            .start(
                registration,
                options.initial_observers.into_iter().collect(),
                env_spec,
                execution_context,
            )
            .await
    }

    async fn prepare_process_environment(
        &self,
        current: &CurrentSessionCapability,
        _session_id: &SessionId,
        registration: crate::ProcessRegistration,
    ) -> Result<crate::ProcessRegistration, crate::PluginError> {
        if !matches!(
            registration.input.as_ref(),
            crate::ProcessInput::Engine { .. }
        ) {
            return Ok(registration);
        }
        let env_spec = match registration.env_ref.as_ref() {
            Some(env_ref) => Some(
                crate::load_process_execution_env(
                    current.host.core.durability.process_env_store.as_ref(),
                    env_ref,
                )
                .await?,
            ),
            None => None,
        };
        self.admit_and_stamp_engine_start(current, registration, env_spec.as_ref())
    }

    /// Admit immutable recorded inputs and stamp the sole engine identity.
    fn admit_and_stamp_engine_start(
        &self,
        current: &CurrentSessionCapability,
        registration: crate::ProcessRegistration,
        env_spec: Option<&crate::ProcessExecutionEnvSpec>,
    ) -> Result<crate::ProcessRegistration, crate::PluginError> {
        let crate::ProcessInput::Engine { kind, payload } = registration.input.as_ref() else {
            return Ok(registration);
        };
        // Deliberate asymmetry between the two routes, and not a new refusal.
        // A request-shaped start captures the live session env before it reaches
        // this gate (`capture_execution_env_ref`), so its env is never absent. A
        // recorded intent must be validated against the env its own record
        // carries — substituting the live session env would make the admitted
        // start depend on when it was realized, which a journaled command may
        // never do. With no recorded env there is nothing to validate against,
        // and such a start was already refused with this exact error downstream
        // in `validate_process_registration`; the gate only moves the same
        // refusal ahead of the journal.
        let Some(env_spec) = env_spec else {
            return Err(crate::PluginError::Session(format!(
                "process `{}` requires a captured execution env",
                registration.id
            )));
        };
        let identity = current
            .host
            .core
            .process_engines
            .admit(kind, payload, Some(env_spec))?;
        Ok(registration.with_identity(identity))
    }

    pub(in crate::runtime::session_manager) async fn await_process(
        &self,
        current: &CurrentSessionCapability,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        let process_ref = match current
            .host
            .process_registry()
            .ok_or_else(|| crate::PluginError::Session("process registry unavailable".to_string()))?
            .resolve_process_ref(process_id)
            .await
        {
            Ok(process_ref) => process_ref,
            Err(crate::PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }) => {
                return Ok(crate::ProcessAwaitOutput::NoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                });
            }
            Err(error) => return Err(error),
        };
        self.await_process_ref(current, process_ref, scope).await
    }

    pub(in crate::runtime::session_manager) async fn await_process_ref(
        &self,
        current: &CurrentSessionCapability,
        process_ref: crate::ProcessRef,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        self.command_runner(current, &scope)?
            .await_process_ref(process_ref)
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
        session_id: &SessionId,
        process_id: &ProcessId,
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

    /// Record the caller departure of an Externally-Owned row this session
    /// observes (FIG-1383).
    ///
    /// Deliberately scope-free, mirroring
    /// [`list_model_tool_process_handles_for_attempt`](Self::list_model_tool_process_handles_for_attempt):
    /// the effect controller this write would otherwise ride is exactly the
    /// thing that has gone away. Visibility is still enforced — only a session
    /// that observes the row may report its caller gone — and the registry
    /// enforces the rest of the state machine.
    pub(in crate::runtime::session_manager) async fn report_process_caller_departure(
        &self,
        current: &CurrentSessionCapability,
        session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let registry = current.host.process_registry().ok_or_else(|| {
            crate::PluginError::Session(
                "process registry is unavailable in this runtime".to_string(),
            )
        })?;
        if !registry.is_observer(session_id, process_id).await? {
            return Err(crate::PluginError::Session(format!(
                "process handle `{process_id}` is not visible in this session"
            )));
        }
        registry.record_caller_departure(process_id).await
    }

    pub(in crate::runtime::session_manager) async fn list_process_handles(
        &self,
        current: &CurrentSessionCapability,
        session_id: &SessionId,
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
        session_id: &SessionId,
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
        session_id: &SessionId,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let records = self
            .list_process_handles_for_attempt(current, session_id, mode)
            .await?;
        Ok(Self::narrow_tool_visible_records(
            current, session_id, records,
        ))
    }

    pub(in crate::runtime::session_manager) async fn list_process_handles_for_attempt(
        &self,
        current: &CurrentSessionCapability,
        session_id: &SessionId,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let registry = current.host.process_registry().ok_or_else(|| {
            crate::PluginError::Session(
                "process registry is unavailable in this runtime".to_string(),
            )
        })?;
        match mode {
            crate::ProcessListMode::Live => registry.list_live_observed_by(session_id).await,
            crate::ProcessListMode::All => {
                registry
                    .list_observed_by(
                        session_id,
                        &crate::ProcessListFilter {
                            status: crate::ProcessStatusFilter::Any,
                            ..Default::default()
                        },
                    )
                    .await
            }
        }
    }

    pub(in crate::runtime::session_manager) async fn cancel_process(
        &self,
        current: &CurrentSessionCapability,
        managed: &ManagedSessionCapability,
        session_id: &SessionId,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        let _ = (managed, session_id);
        runner
            .cancel_named(
                process_id,
                crate::CancelOrigin::OperatorRequested,
                serde_json::to_string(runner.effect_controller_handle.scoped().execution_scope())
                    .expect("execution scopes contain only serializable identities"),
                None,
            )
            .await
    }

    pub(in crate::runtime::session_manager) async fn cancel_recorded_intent(
        &self,
        current: &CurrentSessionCapability,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        runner
            .cancel_named(
                process_id,
                crate::CancelOrigin::ModelRequested,
                identity.replay_key.clone(),
                Some(crate::RuntimeReplayAttribution::ToolIntent(identity)),
            )
            .await
    }

    pub(in crate::runtime::session_manager) async fn finish_recorded_intent_parent(
        &self,
        current: &CurrentSessionCapability,
        identity: crate::ToolIntentIdentity,
        process_id: ProcessId,
        policy: crate::ProcessParentEndPolicy,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ToolIntentParentEndOutcome, crate::PluginError> {
        self.command_runner(current, &scope)?
            .parent_end(identity, process_id, policy)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn emit_process_event(
        &self,
        current: &CurrentSessionCapability,
        session_id: &SessionId,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.validate_model_tool_process_handles(
            current,
            session_id,
            std::slice::from_ref(process_id),
        )
        .await?;
        let request =
            crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(replay_key);
        self.command_runner(current, &scope)?
            .emit_event(process_id, request)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn signal_possessed_process(
        &self,
        current: &CurrentSessionCapability,
        _session_id: &SessionId,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        let record = runner
            .registry()
            .get_process(process_id)
            .await?
            .ok_or_else(|| crate::runtime::registry_transitions::unknown_process(process_id))?;
        if record.is_terminal() {
            return Err(crate::PluginError::ProcessAlreadyTerminal {
                process_id: ProcessId::from(process_id.to_string()),
                status: record.status,
            });
        }
        let event_type = crate::process_signal_event_type(&signal_name)?;
        let request = crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
            format!("process:{process_id}:signal.{signal_name}:{signal_id}"),
        );
        runner
            .signal(
                crate::ProcessRef::from_record(&record),
                signal_name,
                signal_id,
                request,
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn signal_recorded_intent(
        &self,
        current: &CurrentSessionCapability,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let event_type = crate::process_signal_event_type(&signal_name)?;
        let request = crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
            format!("process:{process_id}:signal.{signal_name}:{signal_id}"),
        );
        let runner = self.command_runner(current, &scope)?;
        let process_ref = runner.registry().resolve_process_ref(process_id).await?;
        runner
            .signal_recorded(process_ref, signal_name, signal_id, request)
            .await
    }

    pub(in crate::runtime::session_manager) async fn emit_event_recorded_intent(
        &self,
        current: &CurrentSessionCapability,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let request =
            crate::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(replay_key);
        self.command_runner(current, &scope)?
            .emit_event(process_id, request)
            .await
    }

    pub(in crate::runtime::session_manager) async fn validate_process_handles_observed(
        &self,
        current: &CurrentSessionCapability,
        _managed: &ManagedSessionCapability,
        session_id: &SessionId,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        let _ = scope;
        self.validate_process_handles_observed_inner(current, session_id, handle_ids)
            .await
    }

    pub(in crate::runtime::session_manager) async fn validate_model_tool_process_handles(
        &self,
        current: &CurrentSessionCapability,
        session_id: &SessionId,
        handle_ids: &[ProcessId],
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
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
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
        session_id: &SessionId,
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
        session_id: &SessionId,
    ) {
        if session_id == current.session_id {
            self.sync_needed.store(true, Ordering::Release);
        }
    }

    fn narrow_tool_visible_records(
        current: &CurrentSessionCapability,
        session_id: &SessionId,
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
                    .narrow(
                        &SessionId::from(session_id.to_string()),
                        std::slice::from_ref(process_id),
                    )
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

    /// FIG-653: this gate enforces observer subscription relationships, not authorization.
    async fn validate_process_handles_observed_inner(
        &self,
        current: &CurrentSessionCapability,
        session_id: &SessionId,
        process_ids: &[ProcessId],
    ) -> Result<(), crate::PluginError> {
        if process_ids.is_empty() {
            return Ok(());
        }
        let registry = current.host.process_registry().ok_or_else(|| {
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
        session_id: &SessionId,
        process_ids: &[ProcessId],
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
            let returned = filter.narrow(session_id, std::slice::from_ref(process_id));
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

fn process_visibility_miss(process_id: &ProcessId) -> crate::PluginError {
    crate::PluginError::ProcessNotVisible {
        process_id: ProcessId::from(process_id.to_string()),
    }
}
