use super::*;
use crate::ActorContext;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct ProcessCommandRunner<'scope> {
    current: &'scope CurrentOwnerCapability,
    registry: Arc<dyn crate::ProcessRegistry>,
    parent_invocation: Option<crate::RuntimeInvocation>,
    scoped_effect_controller: crate::ActorContext,
    turn_cancellation: Option<crate::ProcessTurnCancellation>,
}

impl<'scope> ProcessCommandRunner<'scope> {
    fn new(
        current: &'scope CurrentOwnerCapability,
        scope: &'scope crate::ProcessOpScope<'scope>,
        unavailable_message: &'static str,
    ) -> Result<Self, crate::PluginError> {
        let Some(registry) = current.host.process_registry() else {
            return Err(crate::PluginError::Session(unavailable_message.to_string()));
        };
        Ok(Self {
            current,
            registry: Arc::clone(registry),
            parent_invocation: scope.parent_invocation.clone(),
            scoped_effect_controller: scope.effect_controller.clone(),
            turn_cancellation: scope.turn_cancellation.clone(),
        })
    }

    async fn start(
        &self,
        registration: crate::ProcessStartRegistration,
        observers: Vec<SessionId>,
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
            crate::ProcessEffectOutcome::Start { record, .. } => Ok(*record),
            _ => Err(wrong_process_outcome("start")),
        }
    }

    /// StartLaunched records this body. Only its local registry and relay
    /// work belongs here; nested SDK commands cannot replay an unfinished body.
    async fn start_in_run(
        &self,
        registration: crate::ProcessStartRegistration,
        observers: Vec<SessionId>,
        execution_context: crate::ProcessExecutionContext,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        // The owner admitted its launch/prepare record before running this body.
        // This executor only writes the registry and outbox; asking to admit
        // another journal command here rejects that owner step (FIG-5009).
        let execution = self
            .local_executor(Some(self.scoped_effect_controller.clone()))
            .into_process()?;
        match execution
            .execute(
                self.scoped_effect_controller.execution_scope(),
                crate::ProcessCommand::Start {
                    registration,
                    observers,
                    execution_context: Box::new(execution_context),
                },
            )
            .await?
        {
            crate::ProcessEffectOutcome::Start { record, .. } => Ok(*record),
            _ => Err(wrong_process_outcome("start")),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "the process service requires its host's process-work wiring"
    )]
    fn local_executor(
        &self,
        owned_controller: Option<ActorContext>,
    ) -> crate::RuntimeEffectLocalExecutor<'static> {
        let mut local_executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&self.registry),
            self.current
                .host
                .process_work()
                .cloned()
                .expect("process service requires process-work wiring"),
            self.current.host.core.process_engines.clone(),
            // A start executed in its Run body is issued from no journal
            // frontier, so the host's runtime proposes its process scope.
            crate::runtime::HostStartAdmission {
                tracing: Some(self.current.host.core.tracing.clone()),
                ..Default::default()
            },
        )
        .with_process_starts(
            self.current
                .host
                .core
                .backend()
                .obligation_ledger(crate::store::ObligationKind::ProcessStart),
            Arc::clone(&self.current.host.core.clock),
            self.current.host.core.control.relay_policy(),
            self.current.host.core.tracing.metrics().clone(),
        )
        .with_process_env_store(Arc::clone(
            &self.current.host.core.durability.process_env_store,
        ))
        .with_process_attachments(Arc::clone(
            self.current
                .host
                .core
                .durability
                .attachment_store
                .referrers(),
        ));
        if let Some(owned_controller) = owned_controller {
            local_executor = local_executor.with_process_effect_controller(owned_controller);
        }
        if let Some(turn_cancellation) = self.turn_cancellation.clone() {
            local_executor = local_executor.with_process_turn_cancellation(turn_cancellation);
        }
        local_executor
    }

    async fn await_process_ref(
        &self,
        process_id: crate::ProcessId,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::Await { process_id })
            .await?
        {
            crate::ProcessEffectOutcome::Await { output } => Ok(*output),
            _ => Err(wrong_process_outcome("await")),
        }
    }

    async fn attach_process_terminal(
        &self,
        process_id: crate::ProcessId,
        key: crate::AwaitEventKey,
    ) -> Result<Option<crate::ProcessAwaitOutput>, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::AttachTerminal { process_id, key })
            .await?
        {
            crate::ProcessEffectOutcome::AttachTerminal => Ok(None),
            crate::ProcessEffectOutcome::Await { output } => Ok(Some(*output)),
            _ => Err(wrong_process_outcome("attach-terminal")),
        }
    }

    async fn list(
        &self,
        session_scope: crate::SessionScope,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::List {
                selection: crate::ProcessListSelection::Observed {
                    session_scope,
                    mode,
                },
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
        // The recorded cancel admission checks the process is retained and
        // records its answer: a replay after the process was pruned reads
        // that answer, never a registry that has moved on (ADR 0105 §1).
        let command = crate::ProcessCommand::Cancel {
            process_id: process_id.clone(),
            origin,
            requester,
            attribution,
        };
        match self.run(command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => Err(wrong_process_outcome("cancel")),
        }
    }

    async fn signal(
        &self,
        signal: crate::ProcessSignal,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        match self.run(crate::ProcessCommand::Signal { signal }).await? {
            crate::ProcessEffectOutcome::Signal { event } => Ok(*event),
            _ => Err(wrong_process_outcome("signal")),
        }
    }

    async fn emit_event(
        &self,
        process_id: &ProcessId,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        match self
            .run(crate::ProcessCommand::EmitEvent {
                process_id: process_id.clone(),
                request,
            })
            .await?
        {
            crate::ProcessEffectOutcome::EmitEvent { event, .. } => Ok(*event),
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
        let scoped = self.scoped_effect_controller.clone();
        scoped
            .admit_journal_write()
            .map_err(crate::PluginError::RuntimeEffectController)?;
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
        let local_executor = self.local_executor(Some(scoped.clone()));
        let outcome = scoped.process_effect(envelope, local_executor).await?;
        outcome.into_process().map_err(crate::PluginError::from)
    }
}

fn wrong_process_outcome(op: &str) -> crate::PluginError {
    crate::PluginError::Session(format!("process {op} returned the wrong outcome"))
}

impl ProcessCapability {
    fn command_runner<'scope>(
        &self,
        current: &'scope CurrentOwnerCapability,
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

    async fn capture_execution_env(
        &self,
        current: &CurrentOwnerCapability,
        registration: &crate::ProcessStartRegistration,
        scope: &crate::ProcessOpScope<'_>,
    ) -> Result<
        (
            Option<crate::ProcessExecutionEnvRef>,
            Option<crate::ProcessExecutionEnvSpec>,
        ),
        crate::PluginError,
    > {
        let claim = crate::ReferrerClaim::guarded(crate::ReferrerGuard::Journal(
            scope
                .effect_controller
                .execution_scope()
                .journal_identity()
                .map_err(|error| crate::PluginError::Session(error.to_string()))?,
        ));
        if let Some(env_ref) = registration.env_ref.clone() {
            current
                .host
                .core
                .durability
                .process_env_store
                .acquire_process_execution_env(&claim, &env_ref)
                .await
                .map_err(crate::PluginError::from)?;
            let spec = crate::load_process_execution_env(
                current.host.core.durability.process_env_store.as_ref(),
                &env_ref,
            )
            .await?;
            return Ok((Some(env_ref), Some(spec)));
        }
        let spec = current.execution_env_spec()?;
        let env_ref = crate::publish_process_execution_env(
            current.host.core.durability.process_env_store.as_ref(),
            &claim,
            &spec,
        )
        .await?;
        Ok((Some(env_ref), Some(spec)))
    }

    /// K5 launches the Run's admitted registration through the same process
    /// command executor as recorded intents. No live tool policy is consulted.
    pub(in crate::runtime::session_manager) async fn start_bound_process(
        &self,
        current: &CurrentOwnerCapability,
        registration: crate::ProcessStartRegistration,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let observers = match &registration.provenance.originator {
            crate::ProcessOriginator::Session { session_id, .. } => {
                self.mark_current_process_sync_needed(current, session_id);
                vec![session_id.clone()]
            }
            crate::ProcessOriginator::Host { .. } => Vec::new(),
        };
        let registration = with_admitted_start_cx(current, registration, &scope).await?;
        let env_spec = if matches!(
            registration.input.as_ref(),
            crate::ProcessStartTarget::Input(crate::ProcessInput::SessionTurn { .. })
        ) {
            match registration.env_ref.as_ref() {
                Some(reference) => Some(
                    crate::load_process_execution_env(
                        current.host.core.durability.process_env_store.as_ref(),
                        reference,
                    )
                    .await?,
                ),
                None => None,
            }
        } else {
            None
        };
        let registration = self
            .admit_session_turn_start(current, registration, env_spec.as_ref())
            .await?;
        let options = crate::ProcessStartOptions::new().with_initial_observers(observers);
        let execution_context = options.execution_context(&scope);
        let record = self
            .command_runner(current, &scope)?
            .start_in_run(
                registration,
                options.initial_observers.into_iter().collect(),
                execution_context,
            )
            .await?;
        scope.observe_process_started(&current.host.core.tracing, &record);
        Ok(record)
    }

    pub(in crate::runtime::session_manager) async fn start_process(
        &self,
        current: &CurrentOwnerCapability,
        session_id: &SessionId,
        registration: crate::ProcessStartRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.ensure_known_process_session(current, session_id)
            .await?;
        self.mark_current_process_sync_needed(current, session_id);
        let creator_scope = self.process_scope_for_op(session_id, scope.agent_frame_id());
        let caused_by = scope
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::causal_ref);
        let (env_ref, validation_env_spec) = self
            .capture_execution_env(current, &registration, &scope)
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
        let registration = with_admitted_start_cx(current, registration, &scope).await?;
        let registration = self
            .admit_session_turn_start(current, registration, validation_env_spec.as_ref())
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
                execution_context,
            )
            .await
    }

    /// Builds a start command only from the recorded intent payload and its
    /// structural parent invocation, then crosses the journal immediately.
    pub(in crate::runtime::session_manager) async fn start_process_from_recorded_intent(
        &self,
        current: &CurrentOwnerCapability,
        owner: &crate::RuntimeOwner,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        if let Some(session_id) = owner.session_id() {
            self.mark_current_process_sync_needed(current, session_id);
        }
        let caused_by = scope
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::causal_ref);
        let env_spec = if matches!(
            &request.input,
            crate::ProcessStartTarget::Input(crate::ProcessInput::SessionTurn { .. })
        ) {
            match request.env_ref.as_ref() {
                Some(env_ref) => Some(
                    crate::load_process_execution_env(
                        current.host.core.durability.process_env_store.as_ref(),
                        env_ref,
                    )
                    .await?,
                ),
                None => None,
            }
        } else {
            None
        };
        // A leaf start declares no observer edge (#1534 gives the *run* its own
        // possession, which is what makes `await handle` reachable). Observation
        // is the other half: without an edge the declaring session cannot see
        // the child it started, so `processes.list` from the very cell that
        // started it — and `session.admin().processes().list()` — come back
        // empty. The in-session start path has always seeded the declaring
        // session (`ExecutionContext::child_process_observers`); this seeds the
        // same edge for the recorded-intent route. FIG-653: the edge is the
        // subscription relationship, not authorization. An explicit observer set
        // on the request still wins.
        //
        // The edge belongs to the session that *owns* the child, which is the
        // child's own originator — the declaring session for an in-session
        // start, and the chain's session for a child a running process
        // declared. Seeding the calling session instead would put the ephemeral
        // process execution scope on a record, which no route may do, and would
        // hide a process's children from the session that started the chain. A
        // host-originated chain mints no edge at all, exactly as the in-session
        // path's `child_process_observers` does.
        let observers = if request.observers.is_empty() {
            match &request.originator {
                crate::ProcessOriginator::Host { .. } => Vec::new(),
                crate::ProcessOriginator::Session {
                    session_id: originator_session_id,
                    ..
                } => vec![originator_session_id.clone()],
            }
        } else {
            request.observers.clone()
        };
        let originator = request.originator.clone();
        // The host-facing label the start declared (FIG-3122). It is read off
        // the declaration here, before the registration exists, because that is
        // the only place "the host declared this label" is still distinguishable
        // from a label some derivation route produced; admission overwrites the
        // row's label with the engine's, and this restores the declared one
        // afterwards.
        let declared_label = request
            .identity
            .as_ref()
            .and_then(|identity| identity.label.clone());
        let registration = request
            .into_registration()
            .with_process_provenance(
                crate::ProcessProvenance::new(originator).with_caused_by(caused_by),
            )
            .with_consumer_hold(scope.consumer_hold.clone());
        let registration = with_admitted_start_cx(current, registration, &scope).await?;
        let registration = self
            .admit_session_turn_start(current, registration, env_spec.as_ref())
            .await?
            .with_host_facing_label(declared_label);
        let options = crate::ProcessStartOptions::new().with_initial_observers(observers);
        let execution_context = options.execution_context(&scope);
        self.command_runner(current, &scope)?
            .start(
                registration,
                options.initial_observers.into_iter().collect(),
                execution_context,
            )
            .await
    }

    /// Check the session child's creation facts against the parent's plugin set.
    async fn admit_session_turn_start(
        &self,
        current: &CurrentOwnerCapability,
        registration: crate::ProcessStartRegistration,
        env_spec: Option<&crate::ProcessExecutionEnvSpec>,
    ) -> Result<crate::ProcessStartRegistration, crate::PluginError> {
        if let crate::ProcessStartTarget::Input(crate::ProcessInput::SessionTurn {
            create_request,
            ..
        }) = registration.input.as_ref()
        {
            let Some(env_spec) = env_spec else {
                return Err(crate::PluginError::Session(format!(
                    "process `{}` requires a captured execution env",
                    registration.refusal_name()
                )));
            };
            super::super::session_init::admit_session_turn_child(
                current,
                create_request,
                env_spec,
                &registration.refusal_name(),
            )?;
        }
        Ok(registration)
    }

    pub(in crate::runtime::session_manager) async fn await_process(
        &self,
        current: &CurrentOwnerCapability,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        // The await's recorded existence guard answers an unknown or pruned
        // process, so a replay after a prune reads what the first run saw.
        self.await_process_ref(current, process_id.clone(), scope)
            .await
    }

    pub(in crate::runtime::session_manager) async fn await_process_ref(
        &self,
        current: &CurrentOwnerCapability,
        process_id: crate::ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        self.command_runner(current, &scope)?
            .await_process_ref(process_id)
            .await
    }

    /// Observe a terminal through the journaled process seam, or arm it as
    /// the resolver of `key` and return `None` without waiting.
    pub(in crate::runtime::session_manager) async fn attach_process_terminal(
        &self,
        current: &CurrentOwnerCapability,
        process_id: crate::ProcessId,
        key: crate::AwaitEventKey,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Option<crate::ProcessAwaitOutput>, crate::PluginError> {
        self.command_runner(current, &scope)?
            .attach_process_terminal(process_id, key)
            .await
    }

    pub(in crate::runtime::session_manager) async fn list_process_handles(
        &self,
        current: &CurrentOwnerCapability,
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
        current: &CurrentOwnerCapability,
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

    /// The host's tool-visibility filter narrows what a session sees; it is
    /// keyed by session, so a process owner's list is its started children
    /// unnarrowed.
    pub(in crate::runtime::session_manager) async fn list_model_tool_process_handles_for_attempt(
        &self,
        current: &CurrentOwnerCapability,
        owner: &crate::RuntimeOwner,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let records = self
            .list_process_handles_for_attempt(current, owner, mode)
            .await?;
        Ok(match owner {
            crate::RuntimeOwner::Session(session_id) => {
                Self::narrow_tool_visible_records(current, session_id, records)
            }
            crate::RuntimeOwner::Process(_) => records,
        })
    }

    /// What `owner` sees: the processes a session observes, or the children
    /// a process started — those whose recorded ancestry names the process as
    /// its starter and whose lifetime ends with it.
    pub(in crate::runtime::session_manager) async fn list_process_handles_for_attempt(
        &self,
        current: &CurrentOwnerCapability,
        owner: &crate::RuntimeOwner,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        let registry = current.host.process_registry().ok_or_else(|| {
            crate::PluginError::Session(
                "process registry is unavailable in this runtime".to_string(),
            )
        })?;
        match owner {
            crate::RuntimeOwner::Session(session_id) => match mode {
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
            },
            crate::RuntimeOwner::Process(process_id) => {
                let starter = crate::ScopeId::process(process_id.clone());
                Ok(registry
                    .list_processes(&crate::ProcessListFilter {
                        status: crate::ProcessStatusFilter::Any,
                        until: Some(starter.clone()),
                        ..Default::default()
                    })
                    .await?
                    .into_iter()
                    .filter(|record| record.ancestry.starter() == Some(&starter))
                    .filter(|record| match mode {
                        crate::ProcessListMode::Live => !record.status().is_retired(),
                        crate::ProcessListMode::All => true,
                    })
                    .collect())
            }
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "execution scopes are plain string identities"
    )]
    pub(in crate::runtime::session_manager) async fn cancel_process(
        &self,
        current: &CurrentOwnerCapability,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        let _ = owner;
        runner
            .cancel_named(
                process_id,
                crate::CancelOrigin::OperatorRequested,
                serde_json::to_string(runner.scoped_effect_controller.execution_scope())
                    .expect("execution scopes contain only serializable identities"),
                None,
            )
            .await
    }

    /// A Run's declared-start discharge (FIG-5080). It runs inside the owner
    /// step whose `StartDischarged` record carries it, so like
    /// [`ProcessCommandRunner::start_in_run`] it writes only the registry and
    /// the engine's keyed delivery, never a journal command of its own: an
    /// SDK command nested in that step cannot replay an unfinished body.
    #[expect(
        clippy::expect_used,
        reason = "execution scopes are plain string identities"
    )]
    pub(in crate::runtime::session_manager) async fn cancel_bound_process(
        &self,
        current: &CurrentOwnerCapability,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        let (Some(registry), Some(delivery)) =
            (current.host.process_registry(), current.host.process_work())
        else {
            return Err(crate::PluginError::Session(
                "processes are unavailable in this runtime".to_owned(),
            ));
        };
        let requester = serde_json::to_string(scope.effect_controller.execution_scope())
            .expect("execution scopes contain only serializable identities");
        let record = match registry
            .request_process_cancel(
                process_id,
                crate::CancelOrigin::OperatorRequested,
                requester,
                None,
            )
            .await
        {
            Ok(record) => record,
            // A process that ended, took another requester's cancel, or was
            // pruned once ended needs nothing more: the first request
            // stands (ADR 0094).
            Err(
                crate::PluginError::ProcessAlreadyTerminal { .. }
                | crate::PluginError::ProcessCancelConflict { .. }
                | crate::PluginError::ProcessNoLongerRetained { .. },
            ) => return Ok(()),
            Err(error) => return Err(error),
        };
        let request = record.cancel_request.ok_or_else(|| {
            crate::PluginError::Session(format!(
                "process `{process_id}` accepted a cancel without recording it"
            ))
        })?;
        delivery
            .deliver_cancel(
                process_id,
                &request,
                &format!("run-start-discharge:{process_id}"),
            )
            .await
    }

    pub(in crate::runtime::session_manager) async fn cancel_recorded_intent(
        &self,
        current: &CurrentOwnerCapability,
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

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn emit_process_event(
        &self,
        current: &CurrentOwnerCapability,
        session_id: &SessionId,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        self.validate_model_tool_process_handles(
            current,
            &crate::RuntimeOwner::Session(session_id.clone()),
            std::slice::from_ref(process_id),
            scope.clone(),
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
        current: &CurrentOwnerCapability,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        // The recorded append admission refuses an unknown, pruned or ended
        // target and records the refusal, so a replay after the target moved
        // on reads the first run's answer (ADR 0105 §1).
        runner
            .signal(crate::ProcessSignal::new(
                crate::ProcessSignalIdentity::new(process_id.clone(), signal_name, signal_id)?,
                payload,
            ))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime::session_manager) async fn signal_recorded_intent(
        &self,
        current: &CurrentOwnerCapability,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let runner = self.command_runner(current, &scope)?;
        runner
            .signal(crate::ProcessSignal::new(
                crate::ProcessSignalIdentity::new(process_id.clone(), signal_name, signal_id)?,
                payload,
            ))
            .await
    }

    pub(in crate::runtime::session_manager) async fn emit_event_recorded_intent(
        &self,
        current: &CurrentOwnerCapability,
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
        current: &CurrentOwnerCapability,
        owner: &crate::RuntimeOwner,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        if handle_ids.is_empty() {
            return Ok(());
        }
        match Box::pin(self.command_runner(current, &scope)?.run(
            crate::ProcessCommand::ValidateVisible {
                owner: owner.clone(),
                process_ids: handle_ids.to_vec(),
            },
        ))
        .await?
        {
            crate::ProcessEffectOutcome::ValidateVisible { not_visible: None } => Ok(()),
            crate::ProcessEffectOutcome::ValidateVisible {
                not_visible: Some(process_id),
            } => Err(process_visibility_miss(&process_id)),
            _ => Err(wrong_process_outcome("validate-visible")),
        }
    }

    pub(in crate::runtime::session_manager) async fn validate_model_tool_process_handles(
        &self,
        current: &CurrentOwnerCapability,
        owner: &crate::RuntimeOwner,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        self.validate_process_handles_observed(current, owner, handle_ids, scope)
            .await?;
        match owner {
            crate::RuntimeOwner::Session(session_id) => {
                self.validate_tool_filter(current, session_id, handle_ids)
                    .await
            }
            crate::RuntimeOwner::Process(_) => Ok(()),
        }
    }

    pub(in crate::runtime::session_manager) async fn transfer_process_handles(
        &self,
        current: &CurrentOwnerCapability,
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
        current: &CurrentOwnerCapability,
        session_id: &SessionId,
    ) -> Result<(), crate::PluginError> {
        if current.is_current_session(session_id) {
            return Ok(());
        }
        Err(crate::PluginError::Session(format!(
            "unknown session `{session_id}`"
        )))
    }

    fn mark_current_process_sync_needed(
        &self,
        current: &CurrentOwnerCapability,
        session_id: &SessionId,
    ) {
        if current.is_current_session(session_id) {
            self.sync_needed.store(true, Ordering::Release);
        }
    }

    fn narrow_tool_visible_records(
        current: &CurrentOwnerCapability,
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
                    .narrow(&session_id.clone(), std::slice::from_ref(process_id))
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

    async fn validate_tool_filter(
        &self,
        current: &CurrentOwnerCapability,
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
        process_id: process_id.clone(),
    }
}

/// A runtime start records the start context its operation was admitted
/// under: its ancestry and inherited session capability (FIG-3607 R1, R2).
/// Registration then checks the recorded lifetime against that ancestry (R3),
/// so a declaration can name only a scope it was admitted under. An
/// administrative operation has no start context and registers a root, which
/// may only be `Detached`.
///
/// A process scope whose context does not carry the process's lineage reads
/// it back from the enclosing process's own row: the
/// lineage is a recorded, immutable fact of that row, never re-derived. A
/// process with no row is refused rather than recorded as a root.
///
/// A turn or drain of a session a process runs as its own — a subagent's
/// session, executed by the host or the session's engine rather than lent the
/// live body's lineage — reads its owner from the session's metadata
/// (`owning_process_id`, recorded when the owner's start created the session)
/// and that owner's lineage from its row, so the start records the owner
/// above the session (R1).
async fn with_admitted_start_cx(
    current: &CurrentOwnerCapability,
    registration: crate::ProcessStartRegistration,
    scope: &crate::ProcessOpScope<'_>,
) -> Result<crate::ProcessStartRegistration, crate::PluginError> {
    let process_id = match scope.start_cx() {
        Ok(Some(cx)) => {
            if scope.process_lineage.is_none()
                && let Some(session_id) = cx.session_capability()
                && let Some(owner) = owning_process_lineage(current, &session_id).await?
            {
                let cx = scope.start_cx_under(&owner).map_err(|error| {
                    crate::PluginError::Session(format!("process start refused: {error}"))
                })?;
                return Ok(registration.with_start_cx(&cx));
            }
            return Ok(registration.with_start_cx(&cx));
        }
        Ok(None) => return Ok(registration),
        Err(crate::StartCxError::MissingLineage { process_id }) => process_id,
        Err(error) => {
            return Err(crate::PluginError::Session(format!(
                "process start refused: {error}"
            )));
        }
    };
    let registry = current.host.process_registry().ok_or_else(|| {
        crate::PluginError::Session(format!(
            "process start refused: no registry holds the lineage of enclosing process \
             `{process_id}`"
        ))
    })?;
    let enclosing = registry.get_process(&process_id).await?.ok_or_else(|| {
        crate::PluginError::Session(format!(
            "process start refused: enclosing process `{process_id}` has no row to read its \
             lineage from"
        ))
    })?;
    let cx = scope
        .start_cx_under(&enclosing.lineage())
        .map_err(|error| crate::PluginError::Session(format!("process start refused: {error}")))?;
    Ok(registration.with_start_cx(&cx))
}

/// The lineage of the process that runs `session_id` as its own, read from
/// the session's recorded `owning_process_id` and that process's row (FIG-3607
/// R1). `None` for a session no process owns, and for one whose owner's row
/// retention already pruned: a pruned owner ended long ago, its scope is
/// closed, and it can bound nothing a start made now would name.
async fn owning_process_lineage(
    current: &CurrentOwnerCapability,
    session_id: &SessionId,
) -> Result<Option<crate::ProcessLineage>, crate::PluginError> {
    let store = match current.session().and_then(|session| session.store.as_ref()) {
        Some(store) if current.is_current_session(session_id) => Some(store.clone()),
        _ => crate::runtime::live_session_view(
            &current.host.core.session_store_factory(),
            session_id,
        )
        .await
        .map_err(|error| {
            crate::PluginError::Session(format!(
                "process start refused: the owner of session `{session_id}` cannot be \
                     read: {error}"
            ))
        })?,
    };
    let Some(store) = store else {
        return Ok(None);
    };
    let owner = store
        .load_session_meta()
        .await
        .map_err(|error| {
            crate::PluginError::Session(format!(
                "process start refused: the owner of session `{session_id}` cannot be read: \
                 {error}"
            ))
        })?
        .and_then(|meta| meta.owning_process_id);
    let Some(owner) = owner else {
        return Ok(None);
    };
    let registry = current.host.process_registry().ok_or_else(|| {
        crate::PluginError::Session(format!(
            "process start refused: no registry holds the lineage of process `{owner}`, which \
             owns session `{session_id}`"
        ))
    })?;
    Ok(registry
        .get_process(&owner)
        .await?
        .map(|record| record.lineage()))
}
