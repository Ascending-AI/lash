use super::*;

#[test]
pub(super) fn restate_command_execution_plan_is_explicit_for_every_command() {
    let cases = vec![
        (RuntimeEffectCommand::Sleep { duration_ms: 1 }, "timer"),
        (
            RuntimeEffectCommand::process(ProcessCommand::List {
                session_scope: lash_core::SessionScope::new("session"),
                mode: lash_core::ProcessListMode::Live,
            }),
            "direct_process",
        ),
        (
            RuntimeEffectCommand::AwaitEvent {
                key: restate_await_event_key(
                    &durable_turn_scope("session", "turn"),
                    AwaitEventWaitIdentity::Custom {
                        key: "event".to_string(),
                    },
                )
                .expect("await-event key"),
            },
            "await_event",
        ),
        (
            RuntimeEffectCommand::PeekAwaitEvent {
                key: restate_await_event_key(
                    &durable_turn_scope("session", "turn"),
                    AwaitEventWaitIdentity::Custom {
                        key: "peek-event".to_string(),
                    },
                )
                .expect("peek-await-event key"),
            },
            "peek_await_event",
        ),
        (
            RuntimeEffectCommand::LlmCall {
                request: Box::new(llm_spec()),
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::Direct {
                request: Box::new(llm_spec()),
                usage_source: "test".to_string(),
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::ToolBatch {
                batch: lash_core::PreparedToolBatch::new("batch", vec![prepared_tool_call()]),
            },
            "durable_tool_batch",
        ),
        (
            RuntimeEffectCommand::ExecCode {
                language: "code".to_string(),
                code: "1 + 1".to_string(),
            },
            // The interpreter is composite: it can issue nested timers,
            // waits, tools, and model calls. Rebuild it on handler replay and
            // let those child effects use their own stable journal keys.
            "direct_local",
        ),
        (
            RuntimeEffectCommand::Checkpoint {
                checkpoint: lash_core::CheckpointKind::AfterWork,
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::SyncExecutionEnvironment {
                update_machine_config: true,
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::Trigger {
                command: Box::new(lash_core::TriggerCommand::List {
                    owner_scope: lash_core::TriggerOwnerScope::session("session"),
                    filter: lash_core::TriggerSubscriptionFilter::default(),
                }),
            },
            "journaled_run",
        ),
    ];

    for (command, expected) in cases {
        let kind = command.kind();
        // A grouped child on an arm that rebuilds the envelope into a target with
        // no membership slot must be refused, not silently stripped: those arms
        // record no canonical envelope, so the wake rule has no hash to fold
        // into and the group's identity fence would simply vanish.
        let grouped =
            RuntimeEffectEnvelope::new(runtime_invocation(kind, "classification"), command.clone())
                .in_effect_group(
                    "scope:group:batch:0",
                    0,
                    lash_core::GroupWakePolicy::First,
                    lash_core::LoserPolicy::RunToCompletion,
                );
        let grouped_result = restate_effect_execution(grouped);
        let carries_membership = matches!(
            expected,
            "direct_local" | "durable_tool_batch" | "journaled_run"
        );
        match grouped_result {
            Ok(_) => assert!(
                carries_membership,
                "the {expected} arm drops group membership silently; it must refuse instead"
            ),
            Err(error) => {
                assert!(
                    !carries_membership,
                    "the {expected} arm can carry membership and must not refuse it: {error}"
                );
                assert_eq!(
                    error.code,
                    lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
                    "an unhonored membership must be a typed group-shape refusal"
                );
            }
        }

        let execution = restate_effect_execution(RuntimeEffectEnvelope::new(
            runtime_invocation(kind, "classification"),
            command,
        ))
        .expect("an ungrouped effect classifies");
        let actual = match execution {
            RestateEffectExecution::DirectProcess { .. } => "direct_process",
            RestateEffectExecution::DurableProcessCommand { .. } => "durable_process_command",
            RestateEffectExecution::DirectLocal { .. } => "direct_local",
            RestateEffectExecution::DurableToolBatch { .. } => "durable_tool_batch",
            RestateEffectExecution::Timer { .. } => "timer",
            RestateEffectExecution::AwaitEvent { .. } => "await_event",
            RestateEffectExecution::PeekAwaitEvent { .. } => "peek_await_event",
            RestateEffectExecution::JournaledRun { .. } => "journaled_run",
        };
        assert_eq!(actual, expected);
    }
}

pub(super) type TestTurnCancelRaceFuture<'run, T> = Pin<
    Box<dyn Future<Output = Result<RestateTurnCancelRaceOutcome<T>, TerminalError>> + Send + 'run>,
>;

#[derive(Default)]
pub(super) struct TestTurnCancelGate {
    state: Mutex<TestTurnCancelGateState>,
}

#[derive(Default)]
pub(super) struct TestTurnCancelGateState {
    next_registration_id: usize,
    revoked_sessions: HashSet<SessionId>,
    registrations: HashMap<usize, TestTurnCancelGateEntry>,
}

pub(super) struct TestTurnCancelGateEntry {
    session_id: SessionId,
    key: AwaitEventKey,
    sender: tokio::sync::oneshot::Sender<RestateTurnCancelWake>,
}

pub(super) struct TestTurnCancelRegistration {
    id: usize,
    receiver: tokio::sync::oneshot::Receiver<RestateTurnCancelWake>,
}

pub(super) enum TestTurnCancelRegistrationVerdict {
    Registered(TestTurnCancelRegistration),
    Revoked,
}

impl TestTurnCancelGate {
    fn register(
        &self,
        key: AwaitEventKey,
    ) -> Result<TestTurnCancelRegistrationVerdict, TerminalError> {
        let Some(session_id) = key.scope.session_id().map(SessionId::from) else {
            return Err(TerminalError::new(
                "turn cancellation gate is missing its session id",
            ));
        };
        let mut state = self.state.lock_recover();
        if state.revoked_sessions.contains(&session_id) {
            return Ok(TestTurnCancelRegistrationVerdict::Revoked);
        }
        let id = state.next_registration_id;
        state.next_registration_id += 1;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        state.registrations.insert(
            id,
            TestTurnCancelGateEntry {
                session_id,
                key,
                sender,
            },
        );
        Ok(TestTurnCancelRegistrationVerdict::Registered(
            TestTurnCancelRegistration { id, receiver },
        ))
    }

    fn unregister(&self, registration_id: usize) {
        self.state
            .lock_recover()
            .registrations
            .remove(&registration_id);
    }

    fn resolve(&self, key: &AwaitEventKey, wake: RestateTurnCancelWake) -> bool {
        self.wake_matching(|entry| entry.key == *key, wake)
    }

    fn revoke_session(&self, session_id: &SessionId) {
        self.state
            .lock_recover()
            .revoked_sessions
            .insert(SessionId::from(session_id.to_string()));
        self.wake_matching(
            |entry| entry.session_id == session_id,
            RestateTurnCancelWake::SessionRevoked,
        );
    }

    fn is_revoked(&self, session_id: &SessionId) -> bool {
        self.state
            .lock_recover()
            .revoked_sessions
            .contains(session_id)
    }

    pub(super) fn registration_count(&self) -> usize {
        self.state.lock_recover().registrations.len()
    }

    pub(super) fn registered_keys(&self) -> Vec<AwaitEventKey> {
        let mut keys = self
            .state
            .lock_recover()
            .registrations
            .values()
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        keys.sort_by_key(AwaitEventKey::promise_key);
        keys
    }

    fn wake_matching(
        &self,
        predicate: impl Fn(&TestTurnCancelGateEntry) -> bool,
        wake: RestateTurnCancelWake,
    ) -> bool {
        let mut state = self.state.lock_recover();
        let registration_ids = state
            .registrations
            .iter()
            .filter_map(|(id, entry)| predicate(entry).then_some(*id))
            .collect::<Vec<_>>();
        for registration_id in &registration_ids {
            if let Some(entry) = state.registrations.remove(registration_id) {
                let _ = entry.sender.send(wake);
            }
        }
        !registration_ids.is_empty()
    }
}

pub(super) fn test_turn_cancel_wake_outcome<T>(
    wake: RestateTurnCancelWake,
    session_id: SessionId,
) -> RestateTurnCancelRaceOutcome<T> {
    match wake {
        RestateTurnCancelWake::TurnCancelled | RestateTurnCancelWake::TurnCancelDeferred => {
            RestateTurnCancelRaceOutcome::TurnCancelled
        }
        RestateTurnCancelWake::SessionRevoked => {
            RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
        }
    }
}

/// What the test gate race does after a wake lands while the guarded wait is
/// still pending. Mirrors the deployed `race_turn_cancel_gate` flow: a deferred
/// stop re-registers on the escalation key and keeps waiting; anything else
/// unwinds.
pub(super) enum TestTurnCancelWakeStep {
    Continue(TestTurnCancelRegistration),
    Unwind(RestateTurnCancelWake),
}

pub(super) fn test_turn_cancel_wake_step(
    gate: &TestTurnCancelGate,
    turn_cancel_key: &AwaitEventKey,
    escalated: bool,
    wake: RestateTurnCancelWake,
) -> Result<TestTurnCancelWakeStep, TerminalError> {
    if escalated || wake != RestateTurnCancelWake::TurnCancelDeferred {
        return Ok(TestTurnCancelWakeStep::Unwind(wake));
    }
    let escalation_key = restate_await_event_key(
        &turn_cancel_key.scope,
        AwaitEventWaitIdentity::TurnCancelEscalation,
    )
    .map_err(TerminalError::from_error)?;
    match gate.register(escalation_key)? {
        TestTurnCancelRegistrationVerdict::Registered(registration) => {
            Ok(TestTurnCancelWakeStep::Continue(registration))
        }
        TestTurnCancelRegistrationVerdict::Revoked => Ok(TestTurnCancelWakeStep::Unwind(
            RestateTurnCancelWake::SessionRevoked,
        )),
    }
}

pub(super) fn test_sleep_or_turn_cancel<'run, 'ctx, C>(
    context: &'run C,
    gate: &'run TestTurnCancelGate,
    duration: Duration,
    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    cancellation: tokio_util::sync::CancellationToken,
) -> TestTurnCancelRaceFuture<'run, ()>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Box::pin(async move {
        let Some(turn_cancel) = turn_cancel else {
            return tokio::select! {
                result = context.sleep_send(duration) => {
                    result.map(RestateTurnCancelRaceOutcome::Completed)
                }
                _ = cancellation.cancelled() => {
                    Ok(RestateTurnCancelRaceOutcome::TurnCancelled)
                }
            };
        };
        let session_id = turn_cancel
            .key
            .scope
            .session_id()
            .map(SessionId::from)
            .ok_or_else(|| {
                TerminalError::new("turn cancellation gate is missing its session id")
            })?;
        let turn_cancel_key = turn_cancel.key;
        let mut registration = match gate.register(turn_cancel_key.clone())? {
            TestTurnCancelRegistrationVerdict::Registered(registration) => registration,
            TestTurnCancelRegistrationVerdict::Revoked => {
                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id });
            }
        };
        let mut escalated = false;
        let guarded = context.sleep_send(duration);
        tokio::pin!(guarded);
        loop {
            tokio::select! {
                biased;
                result = &mut guarded => {
                    gate.unregister(registration.id);
                    return result.map(RestateTurnCancelRaceOutcome::Completed);
                }
                wake = &mut registration.receiver => {
                    let wake = wake.map_err(|_| TerminalError::new("test turn cancellation gate was dropped"))?;
                    match test_turn_cancel_wake_step(gate, &turn_cancel_key, escalated, wake)? {
                        TestTurnCancelWakeStep::Continue(next) => {
                            registration = next;
                            escalated = true;
                        }
                        TestTurnCancelWakeStep::Unwind(wake) => {
                            return Ok(test_turn_cancel_wake_outcome(wake, session_id));
                        }
                    }
                }
                _ = cancellation.cancelled() => {
                    gate.unregister(registration.id);
                    return Ok(RestateTurnCancelRaceOutcome::TurnCancelled);
                }
            }
        }
    })
}

pub(super) fn test_await_event_or_turn_cancel<'run, 'ctx, C>(
    context: &'run C,
    gate: &'run TestTurnCancelGate,
    request: RestateDurableWaitAwaitRequest,
    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    cancellation: tokio_util::sync::CancellationToken,
) -> TestTurnCancelRaceFuture<'run, Resolution>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Box::pin(async move {
        let Some(turn_cancel) = turn_cancel else {
            return context
                .await_event(request, cancellation)
                .await
                .map(RestateTurnCancelRaceOutcome::Completed);
        };
        let session_id = turn_cancel
            .key
            .scope
            .session_id()
            .map(SessionId::from)
            .ok_or_else(|| {
                TerminalError::new("turn cancellation gate is missing its session id")
            })?;
        let turn_cancel_key = turn_cancel.key;
        let mut registration = match gate.register(turn_cancel_key.clone())? {
            TestTurnCancelRegistrationVerdict::Registered(registration) => registration,
            TestTurnCancelRegistrationVerdict::Revoked => {
                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id });
            }
        };
        let event_key = request.key.clone();
        let mut escalated = false;
        let guarded = context.await_event(request, cancellation);
        tokio::pin!(guarded);
        loop {
            tokio::select! {
                biased;
                wake = &mut registration.receiver => {
                    let wake = wake.map_err(|_| TerminalError::new("test turn cancellation gate was dropped"))?;
                    match test_turn_cancel_wake_step(gate, &turn_cancel_key, escalated, wake)? {
                        TestTurnCancelWakeStep::Continue(next) => {
                            registration = next;
                            escalated = true;
                        }
                        TestTurnCancelWakeStep::Unwind(wake) => {
                            if wake != RestateTurnCancelWake::SessionRevoked {
                                context.resolve_event(RestateDurableWaitResolveRequest {
                                    key: event_key,
                                    resolution: Resolution::Cancelled,
                                }).await?;
                            }
                            return Ok(test_turn_cancel_wake_outcome(wake, session_id));
                        }
                    }
                }
                result = &mut guarded => {
                    gate.unregister(registration.id);
                    return result.map(RestateTurnCancelRaceOutcome::Completed);
                }
            }
        }
    })
}

pub(super) fn test_await_process_terminal_or_turn_cancel<'run, 'ctx, C>(
    context: &'run C,
    gate: &'run TestTurnCancelGate,
    process_id: ProcessId,
    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Box::pin(async move {
        let Some(turn_cancel) = turn_cancel else {
            return context
                .await_process_terminal(process_id)
                .await
                .map(Box::new)
                .map(RestateTurnCancelRaceOutcome::Completed);
        };
        let session_id = turn_cancel
            .key
            .scope
            .session_id()
            .map(SessionId::from)
            .ok_or_else(|| {
                TerminalError::new("turn cancellation gate is missing its session id")
            })?;
        let turn_cancel_key = turn_cancel.key;
        let mut registration = match gate.register(turn_cancel_key.clone())? {
            TestTurnCancelRegistrationVerdict::Registered(registration) => registration,
            TestTurnCancelRegistrationVerdict::Revoked => {
                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id });
            }
        };
        let mut escalated = false;
        let guarded = context.await_process_terminal(process_id);
        tokio::pin!(guarded);
        loop {
            tokio::select! {
                biased;
                wake = &mut registration.receiver => {
                    let wake = wake.map_err(|_| TerminalError::new("test turn cancellation gate was dropped"))?;
                    match test_turn_cancel_wake_step(gate, &turn_cancel_key, escalated, wake)? {
                        TestTurnCancelWakeStep::Continue(next) => {
                            registration = next;
                            escalated = true;
                        }
                        TestTurnCancelWakeStep::Unwind(wake) => {
                            return Ok(test_turn_cancel_wake_outcome(wake, session_id));
                        }
                    }
                }
                result = &mut guarded => {
                    gate.unregister(registration.id);
                    return result
                        .map(Box::new)
                        .map(RestateTurnCancelRaceOutcome::Completed);
                }
            }
        }
    })
}

pub(super) async fn wait_for_test_turn_cancel_registration(gate: &TestTurnCancelGate) {
    for _ in 0..100 {
        if gate.registration_count() > 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("test turn cancellation gate was never registered");
}

#[derive(Default)]
pub(super) struct RecordingContext {
    endpoint: Option<Endpoint>,
    pub(super) block_sleeps: AtomicBool,
    pub(super) sleeps: Mutex<Vec<u64>>,
    pub(super) runs: Mutex<Vec<String>>,
    pub(super) started: Mutex<Vec<ProcessRegistration>>,
    started_execution_contexts: Mutex<Vec<ProcessExecutionContext>>,
    pub(super) process_command_log: Mutex<Vec<String>>,
    pub(super) cancelled: Mutex<Vec<(String, Option<String>)>>,
    pub(super) resolved_events: Mutex<Vec<RestateDurableWaitResolveRequest>>,
    pub(super) scope_effect_begins: AtomicUsize,
    pub(super) scope_group_records: AtomicUsize,
    awaited_events: Mutex<HashMap<String, Resolution>>,
    durable_events: Mutex<HashMap<String, Resolution>>,
    durable_event_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    process_terminal_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    session_waits: Mutex<HashMap<SessionId, Vec<AwaitEventKey>>>,
    revoked_sessions: Mutex<HashSet<SessionId>>,
    pub(super) turn_cancel_gate: TestTurnCancelGate,
}

#[derive(Default)]
pub(super) struct RecordingTraceSink {
    pub(super) records: Mutex<Vec<lash_trace::TraceRecord>>,
}

impl lash_trace::TraceSink for RecordingTraceSink {
    fn append(&self, record: &lash_trace::TraceRecord) -> Result<(), lash_trace::TraceSinkError> {
        self.records.lock_recover().push(record.clone());
        Ok(())
    }
}

impl RecordingContext {
    pub(super) fn with_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint: Some(endpoint),
            ..Default::default()
        }
    }

    pub(super) fn resolve_process_terminal(
        &self,
        process_id: &ProcessId,
        output: &ProcessAwaitOutput,
    ) {
        let resolution =
            restate_process_terminal_resolution(output).expect("terminal await resolution");
        self.resolve_process_terminal_resolution(process_id, resolution);
    }

    pub(super) fn resolve_process_terminal_resolution(
        &self,
        process_id: &ProcessId,
        resolution: Resolution,
    ) {
        let key = restate_process_terminal_await_key(process_id).expect("terminal await key");
        self.awaited_events
            .lock_recover()
            .insert(key.promise_key(), resolution);
        self.process_terminal_notify(process_id).notify_waiters();
    }

    fn durable_event_notify(&self, workflow_key: &str) -> Arc<tokio::sync::Notify> {
        self.durable_event_notifies
            .lock_recover()
            .entry(workflow_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    fn process_terminal_notify(&self, process_id: &ProcessId) -> Arc<tokio::sync::Notify> {
        self.process_terminal_notifies
            .lock_recover()
            .entry(process_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    pub(super) fn reset_invocation_state_for_replay_preserving_durable_event(
        &self,
        workflow_key: &str,
    ) {
        // Restate replays invocation-local awakeables in journal order. This
        // in-memory context must discard the prior pass's turn-control and
        // process-terminal resolutions while retaining external input.
        let preserved = self
            .durable_events
            .lock_recover()
            .get(workflow_key)
            .cloned()
            .expect("durable event to preserve during replay");
        let mut events = self.durable_events.lock_recover();
        events.clear();
        events.insert(workflow_key.to_string(), preserved);
        drop(events);
        self.awaited_events.lock_recover().clear();
    }

    pub(super) fn resolve_durable_event(
        &self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveOutcome {
        if request.key.scope.session_id().is_some_and(|session_id| {
            self.revoked_sessions
                .lock_recover()
                .contains(&SessionId::from(session_id))
        }) {
            return ResolveOutcome::UnknownOrRevoked;
        }
        self.turn_cancel_gate.resolve(
            &request.key,
            RestateTurnCancelWake::for_gate_resolution(&request.resolution),
        );
        self.terminalize_durable_event(request)
    }

    fn terminalize_durable_event(
        &self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveOutcome {
        self.resolved_events.lock_recover().push(request.clone());
        let address = request.address();
        let mut events = self.durable_events.lock_recover();
        if let Some(terminal) = events.get(&address.workflow_key) {
            return ResolveOutcome::AlreadyResolved {
                terminal: terminal.clone(),
            };
        }
        events.insert(address.workflow_key.clone(), request.resolution);
        drop(events);
        self.durable_event_notify(&address.workflow_key)
            .notify_waiters();
        ResolveOutcome::Accepted
    }

    fn settle_session_wait(&self, key: &AwaitEventKey) {
        if key.wait.is_turn_control() {
            return;
        }
        let Some(session_id) = key.scope.session_id() else {
            return;
        };
        if let Some(waits) = self
            .session_waits
            .lock_recover()
            .get_mut(&SessionId::from(session_id))
        {
            waits.retain(|wait| wait != key);
        }
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<RecordingContext> {
    fn scope_group_record<'run>(
        &'run self,
        _index_key: String,
        _group_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.scope_group_records.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(true) })
    }

    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps.lock_recover().push(duration.as_millis() as u64);
        let block = self.block_sleeps.load(Ordering::SeqCst);
        Box::pin(async move {
            if block {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn sleep_or_turn_cancel<'run>(
        &'run self,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TestTurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run,
    {
        test_sleep_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            duration,
            turn_cancel,
            cancellation,
        )
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        _effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs.lock_recover().push(_effect_name);
        Box::pin(async move { Ok(Json(future.await)) })
    }

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let process_id = registration.id.clone();
        let endpoint = self.endpoint.clone();
        self.process_command_log
            .lock_recover()
            .push(format!("send:{process_id}"));
        self.started.lock_recover().push(registration.clone());
        self.started_execution_contexts
            .lock_recover()
            .push(execution_context.clone());
        Box::pin(async move {
            if let Some(endpoint) = endpoint {
                let complete_runs =
                    matches!(registration.input.as_ref(), ProcessInput::ToolCall { .. });
                invoke_process_workflow_endpoint(
                    &endpoint,
                    "run",
                    &process_id,
                    &RestateProcessWorkflowInput {
                        registration,
                        execution_context,
                        segment_ordinal: 0,
                        execution_id: None,
                    },
                    complete_runs,
                )
                .await?;
            }
            Ok(format!("invocation-{process_id}"))
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let endpoint = self.endpoint.clone();
        let process_id = request.process_id.clone();
        self.cancelled
            .lock_recover()
            .push((request.process_id.to_string(), request.reason.clone()));
        Box::pin(async move {
            if let Some(endpoint) = endpoint {
                invoke_process_workflow_endpoint(&endpoint, "cancel", &process_id, &request, false)
                    .await?;
            }
            Ok(())
        })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let context = Arc::clone(self);
        Box::pin(async move {
            let address = request.address();
            if let Some(session_id) = request.key.scope.session_id() {
                if context.revoked_sessions.lock_recover().contains(session_id) {
                    context.terminalize_durable_event(RestateDurableWaitResolveRequest {
                        key: request.key,
                        resolution: Resolution::Cancelled,
                    });
                    return Ok(Resolution::Cancelled);
                }
                context
                    .session_waits
                    .lock_recover()
                    .entry(SessionId::from(session_id.to_string()))
                    .or_default()
                    .push(request.key.clone());
            }
            let notify = context.durable_event_notify(&address.workflow_key);
            loop {
                if let Some(resolution) = context
                    .durable_events
                    .lock_recover()
                    .get(&address.workflow_key)
                    .cloned()
                {
                    context.settle_session_wait(&request.key);
                    return Ok(resolution);
                }
                if let Some(timeout_ms) = request.timeout_ms {
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = cancellation.cancelled() => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                key: request.key.clone(),
                                resolution: Resolution::Cancelled,
                            });
                        }
                        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                key: request.key.clone(),
                                resolution: Resolution::Timeout,
                            });
                        }
                    }
                } else {
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = cancellation.cancelled() => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                key: request.key.clone(),
                                resolution: Resolution::Cancelled,
                            });
                        }
                    }
                }
            }
        })
    }

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TestTurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        test_await_event_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            request,
            turn_cancel,
            cancellation,
        )
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        // A peek sees exactly what this context already terminalized: a
        // resolved promise reads back, an unresolved one reads as pending.
        let resolution = self
            .durable_events
            .lock_recover()
            .get(&address.workflow_key)
            .cloned();
        Box::pin(async move { Ok(resolution) })
    }

    fn await_process_terminal<'run>(
        &'run self,
        process_id: ProcessId,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.process_command_log
            .lock_recover()
            .push(format!("call:{process_id}"));
        let context = Arc::clone(self);
        Box::pin(async move {
            let key = restate_process_terminal_await_key(&process_id)
                .map_err(TerminalError::from_error)?;
            let notify = context.process_terminal_notify(&process_id);
            let resolution = loop {
                let notified = notify.notified();
                if let Some(resolution) = context
                    .awaited_events
                    .lock_recover()
                    .get(&key.promise_key())
                    .cloned()
                {
                    break resolution;
                }
                notified.await;
            };
            restate_process_terminal_output(&process_id, resolution)
                .map_err(TerminalError::from_error)
        })
    }

    fn await_process_terminal_or_turn_cancel<'run>(
        &'run self,
        process_id: ProcessId,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    ) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run,
    {
        test_await_process_terminal_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            process_id,
            turn_cancel,
        )
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let outcome = self.resolve_durable_event(request);
        Box::pin(async move { Ok(outcome) })
    }

    fn update_session_waits<'run>(
        &'run self,
        session_id: SessionId,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        if revoke {
            self.revoked_sessions
                .lock_recover()
                .insert(session_id.clone());
            self.turn_cancel_gate.revoke_session(&session_id);
        }
        let waits = self
            .session_waits
            .lock_recover()
            .remove(&session_id)
            .unwrap_or_default();
        let (resolve, retain): (Vec<_>, Vec<_>) = if revoke {
            (waits, Vec::new())
        } else {
            waits
                .into_iter()
                .partition(|key| !key.wait.is_turn_control())
        };
        if !retain.is_empty() {
            self.session_waits.lock_recover().insert(session_id, retain);
        }
        for key in resolve {
            self.terminalize_durable_event(RestateDurableWaitResolveRequest {
                key,
                resolution: Resolution::Cancelled,
            });
        }
        Box::pin(async { Ok(()) })
    }

    fn session_is_revoked<'run>(
        &'run self,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let revoked = self.revoked_sessions.lock_recover().contains(&session_id);
        Box::pin(async move { Ok(revoked) })
    }

    fn scope_effect_begin<'run>(
        &'run self,
        _index_key: String,
        _replay_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.scope_effect_begins.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(true) })
    }
}

pub(super) struct ZeroPermitSemaphore(tokio::sync::Semaphore);

impl Default for ZeroPermitSemaphore {
    fn default() -> Self {
        Self(tokio::sync::Semaphore::new(0))
    }
}

impl std::ops::Deref for ZeroPermitSemaphore {
    type Target = tokio::sync::Semaphore;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Default)]
pub(super) struct ReplayableRecordingContext {
    pub(super) sleeps: Mutex<Vec<u64>>,
    pub(super) park_sleeps: AtomicBool,
    pub(super) sleep_started: tokio::sync::Notify,
    pub(super) sleep_release: tokio::sync::Notify,
    pub(super) crash_after_run_commit: AtomicBool,
    pub(super) run_committed: ZeroPermitSemaphore,
    pub(super) runs: Mutex<Vec<String>>,
    pub(super) records: Mutex<HashMap<String, Vec<u8>>>,
    pub(super) replaying: AtomicBool,
    pub(super) append_missing_on_replay: AtomicBool,
    pub(super) peek_records: Mutex<Vec<Option<Resolution>>>,
    pub(super) peek_cursor: AtomicUsize,
    pub(super) events: Arc<RecordingContext>,
    pub(super) process_worker: Mutex<Option<DurableProcessWorker>>,
    pub(super) defer_process_workflows: AtomicBool,
    pub(super) replay_process_workflow_starts_from_journal: AtomicBool,
    pub(super) live_process_workflow_starts: AtomicUsize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(super) struct ToolIntentJournalCorpusFixture {
    crash_point: String,
    captured_from_endpoint_interruption: bool,
    invocation_body_bytes: Vec<u8>,
    expected_response_command_frame_types: Vec<u16>,
    expected_output: Option<serde_json::Value>,
    expected_signal_events: usize,
}

pub(super) const TOOL_INTENT_CORPUS_KEY: &str = "tool-intent-corpus-v2";
pub(super) const TOOL_INTENT_CORPUS_SESSION: &str = "tool-intent-corpus-session";
pub(super) const TOOL_INTENT_CORPUS_TURN: &str = "tool-intent-corpus-turn";
pub(super) const TOOL_INTENT_CORPUS_TARGET: &str = "tool-intent-corpus-target";

#[restate_sdk::workflow]
trait ToolIntentCorpusReplay {
    async fn run(input: Json<()>) -> HandlerResult<Json<serde_json::Value>>;
}

pub(super) struct ToolIntentCorpusReplayImpl {
    registry: Arc<dyn ProcessRegistry>,
}

impl ToolIntentCorpusReplay for ToolIntentCorpusReplayImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(()): Json<()>,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let scope = ExecutionScope::turn(TOOL_INTENT_CORPUS_SESSION, TOOL_INTENT_CORPUS_TURN);
        let attempt = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    lash_core::RuntimeEffectInvocation::new(
                        lash_core::EffectAddress::new(scope.clone(), "tool-intent-corpus-attempt")
                            .expect("valid tool-intent corpus address"),
                        lash_core::RuntimeAttribution::for_turn(
                            TOOL_INTENT_CORPUS_SESSION,
                            TOOL_INTENT_CORPUS_TURN,
                            0,
                            0,
                        ),
                        "tool-intent-corpus-attempt",
                    ),
                    RuntimeEffectCommand::ToolAttempt {
                        call: prepared_tool_call_with(
                            "tool-intent-corpus-call",
                            "tool_intent_corpus",
                        ),
                        execution_grant: None,
                        attempt: 1,
                        max_attempts: 1,
                    },
                ),
                RuntimeEffectLocalExecutor::testing(|_| async {
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                            record: Box::new(completed_tool_record(
                                "tool-intent-corpus-call",
                                "tool_intent_corpus",
                            )),
                            intents: lash_core::ToolIntents::v1(vec![
                                lash_core::ToolIntent::SignalProcess(
                                    lash_core::SignalProcessIntent {
                                        session_id: SessionId::from(
                                            TOOL_INTENT_CORPUS_SESSION.to_string(),
                                        ),
                                        process_id: ProcessId::from(
                                            TOOL_INTENT_CORPUS_TARGET.to_string(),
                                        ),
                                        signal_name: "resume".to_string(),
                                        payload: serde_json::json!({
                                            "source": "checked-in-endpoint-corpus"
                                        }),
                                    },
                                ),
                            ]),
                        }),
                        triggers: Vec::new(),
                    })
                }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = attempt else {
            return Err(TerminalError::new("corpus attempt returned the wrong effect").into());
        };
        let lash_core::ToolAttemptLaunch::Done { intents, .. } = *launch else {
            return Err(TerminalError::new("corpus attempt did not finish").into());
        };
        let outcomes = lash_core::testing::execute_tool_intents_with_services(
            controller
                .scoped_effect_controller(scope)
                .map_err(TerminalError::from_error)?,
            lash_core::testing::effect_backed_process_service(Arc::clone(&self.registry)),
            &SessionId::from(TOOL_INTENT_CORPUS_SESSION),
            "tool-intent-corpus-call",
            &intents,
        )
        .await
        .map_err(TerminalError::from_error)?;
        Ok(Json(
            serde_json::to_value(outcomes).map_err(TerminalError::from_error)?,
        ))
    }
}

pub(super) async fn tool_intent_corpus_endpoint() -> (Endpoint, Arc<dyn ProcessRegistry>) {
    let clock: Arc<dyn lash_core::Clock> = Arc::new(ToolIntentCorpusClock);
    let registry = lash_sqlite_store::SqliteProcessRegistry::memory()
        .await
        .expect("open corpus process registry")
        .with_runtime_clock(clock)
        .expect("install fixed corpus clock");
    registry
        .register_process(
            ProcessRegistration::new(
                TOOL_INTENT_CORPUS_TARGET,
                ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "endpoint-corpus"}),
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("seed corpus signal target");
    let endpoint = Endpoint::builder()
        .bind(
            ToolIntentCorpusReplayImpl {
                registry: Arc::clone(&registry),
            }
            .serve(),
        )
        .build();
    (endpoint, registry)
}

#[derive(Debug)]
pub(super) struct ToolIntentCorpusClock;

#[async_trait::async_trait]
impl lash_core::Clock for ToolIntentCorpusClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = 1_700_000_000_123;
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[test]
pub(super) fn tool_intent_corpus_clock_wall_clock_faces_agree() {
    let clock = ToolIntentCorpusClock;
    let clock: &dyn lash_core::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

pub(super) async fn replay_tool_intent_corpus_fixture(
    fixture: &ToolIntentJournalCorpusFixture,
) -> (Vec<u16>, Option<serde_json::Value>, usize) {
    let (endpoint, registry) = tool_intent_corpus_endpoint().await;
    let response = invoke_endpoint_body(
        &endpoint,
        "ToolIntentCorpusReplay",
        "run",
        bytes::Bytes::from(fixture.invocation_body_bytes.clone()),
    )
    .await
    .expect("feed checked-in corpus bytes through the Restate endpoint");
    let signal_events = registry
        .events_after(&ProcessId::from(TOOL_INTENT_CORPUS_TARGET), 0)
        .await
        .expect("read corpus signal outcomes")
        .into_iter()
        .filter(|event| event.event_type == "signal.resume")
        .count();
    (
        restate_command_frame_types(&response),
        restate_output_json::<serde_json::Value>(&response),
        signal_events,
    )
}

#[tokio::test]
pub(super) async fn checked_in_tool_intent_journals_replay_through_endpoint_with_literal_outcomes()
{
    for checked_in in [
        // The mid-drain prefix ends before the durable-wait index call, so
        // its v2 capture is unchanged by the scope-keyed index cutover.
        include_bytes!("../../tests/fixtures/tool_intent_journals/v2-mid-drain.json").as_slice(),
        include_bytes!("../../tests/fixtures/tool_intent_journals/v3-mid-intent.json").as_slice(),
        include_bytes!("../../tests/fixtures/tool_intent_journals/v3-full-drain.json").as_slice(),
    ] {
        let fixture: ToolIntentJournalCorpusFixture =
            serde_json::from_slice(checked_in).expect("decode checked-in endpoint corpus fixture");
        assert!(
            fixture.captured_from_endpoint_interruption,
            "{} must name its real endpoint-interruption provenance",
            fixture.crash_point
        );
        let (command_frames, output, signal_events) =
            replay_tool_intent_corpus_fixture(&fixture).await;
        assert_eq!(
            command_frames, fixture.expected_response_command_frame_types,
            "{} response command frames",
            fixture.crash_point
        );
        assert_eq!(
            output, fixture.expected_output,
            "{} output",
            fixture.crash_point
        );
        assert_eq!(
            signal_events, fixture.expected_signal_events,
            "{} literal process outcome count",
            fixture.crash_point
        );
    }
}

/// The untouched pre-cutover endpoint artifacts now encounter the earlier
/// effect-envelope shape fence. That refusal happens before their recorded
/// signal effect is reconstructed, so a fresh registry stays empty.
#[tokio::test]
pub(super) async fn checked_in_pre_cutover_tool_intent_journals_refuse_loudly_without_duplicate_effect()
 {
    for (name, checked_in) in [
        (
            "v1-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v1-full-drain.json")
                .as_slice(),
        ),
        (
            "v2-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v2-mid-intent.json")
                .as_slice(),
        ),
        (
            "v2-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v2-full-drain.json")
                .as_slice(),
        ),
    ] {
        let fixture: ToolIntentJournalCorpusFixture = serde_json::from_slice(checked_in)
            .expect("decode the pre-cutover endpoint corpus fixture");
        let (endpoint, registry) = tool_intent_corpus_endpoint().await;

        let response = invoke_endpoint_body(
            &endpoint,
            "ToolIntentCorpusReplay",
            "run",
            bytes::Bytes::from(fixture.invocation_body_bytes),
        )
        .await
        .expect("feed the pre-cutover journal through the current endpoint");
        let error = restate_output_failure_message(&response)
            .or_else(|| restate_error_message(&response))
            .unwrap_or_else(|| {
                panic!(
                    "{name} replay must refuse loudly; messages={:?}; frames={:?}; output={:?}",
                    restate_message_types(&response),
                    restate_command_frame_types(&response),
                    restate_output_json::<serde_json::Value>(&response)
                )
            });
        assert!(
            error.contains("Found a mismatch between the code paths taken during the previous execution and the paths taken during this execution"),
            "{name} must retain its current Restate shape refusal: {error}"
        );
        assert_eq!(
            registry
                .events_after(&ProcessId::from(TOOL_INTENT_CORPUS_TARGET), 0)
                .await
                .expect("read the refusal witness target")
                .into_iter()
                .filter(|event| event.event_type == "signal.resume")
                .count(),
            0,
            "{name}: the earlier shape refusal must happen before any effect is reconstructed"
        );
    }
}

fn replace_equal_length_bytes(input: &[u8], from: &[u8], to: &[u8]) -> (Vec<u8>, usize) {
    assert_eq!(
        from.len(),
        to.len(),
        "fixture substitution must preserve framing"
    );
    let mut result = input.to_vec();
    let mut cursor = 0;
    let mut replacements = 0;
    while let Some(offset) = result[cursor..]
        .windows(from.len())
        .position(|window| window == from)
    {
        let start = cursor + offset;
        result[start..start + from.len()].copy_from_slice(to);
        cursor = start + to.len();
        replacements += 1;
    }
    (result, replacements)
}

/// Isolate an old process-reference identity from the earlier envelope and
/// durable-wait shape changes. The current supported endpoint corpus is
/// rebound byte-for-byte to the equal-length historical replay key. Restate
/// must refuse its obsolete command name before duplicate execution can occur.
#[tokio::test]
pub(super) async fn pre_cutover_process_reference_name_refuses_before_duplicate_effect() {
    let historical_bytes =
        include_bytes!("../../tests/fixtures/tool_intent_journals/v1-full-drain.json");
    let historical: ToolIntentJournalCorpusFixture = serde_json::from_slice(historical_bytes)
        .expect("decode immutable v1 endpoint corpus fixture");
    let current: ToolIntentJournalCorpusFixture = serde_json::from_slice(include_bytes!(
        "../../tests/fixtures/tool_intent_journals/v3-full-drain.json"
    ))
    .expect("decode current endpoint corpus fixture");
    assert_eq!(
        historical
            .expected_output
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1),
        "the historical artifact must contain one actually recorded signal outcome"
    );

    let historical_replay_key = historical
        .expected_output
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .and_then(|outcomes| outcomes.first())
        .and_then(|outcome| outcome.pointer("/identity/replay_key"))
        .and_then(serde_json::Value::as_str)
        .expect("historical fixture records its tool-intent identity");
    let current_replay_key = current
        .expected_output
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .and_then(|outcomes| outcomes.first())
        .and_then(|outcome| outcome.pointer("/identity/replay_key"))
        .and_then(serde_json::Value::as_str)
        .expect("current fixture records its tool-intent identity");
    let (old_identity_witness, replacements) = replace_equal_length_bytes(
        &current.invocation_body_bytes,
        current_replay_key.as_bytes(),
        historical_replay_key.as_bytes(),
    );
    assert_eq!(
        replacements, 7,
        "the current full-drain fixture must contain the pinned identity in its command and recorded effect"
    );
    let (endpoint, registry) = tool_intent_corpus_endpoint().await;
    let response = invoke_endpoint_body(
        &endpoint,
        "ToolIntentCorpusReplay",
        "run",
        bytes::Bytes::from(old_identity_witness),
    )
    .await
    .expect("feed isolated old-identity witness through the current endpoint");
    let error = restate_output_failure_message(&response)
        .or_else(|| restate_error_message(&response))
        .unwrap_or_else(|| {
            panic!(
                "old process reference must refuse loudly; messages={:?}; frames={:?}; output={:?}",
                restate_message_types(&response),
                restate_command_frame_types(&response),
                restate_output_json::<serde_json::Value>(&response)
            )
        });
    assert!(
        error.contains("Found a mismatch between the code paths taken during the previous execution and the paths taken during this execution"),
        "isolated old process reference must retain its Restate command-name refusal: {error}"
    );
    assert_eq!(
        registry
            .events_after(&ProcessId::from(TOOL_INTENT_CORPUS_TARGET), 0)
            .await
            .expect("read the isolated refusal witness target")
            .into_iter()
            .filter(|event| event.event_type == "signal.resume")
            .count(),
        1,
        "the isolated journal may reconstruct its one committed signal but must not duplicate it"
    );
}

/// Regeneration is deliberately separate from the replay law above: the law
/// only consumes checked-in bytes. This ignored capture utility obtains each
/// prefix by closing a real endpoint invocation at the named `RunCommand`.
#[tokio::test]
#[ignore = "explicit corpus capture utility"]
pub(super) async fn capture_tool_intent_journal_corpus_from_real_endpoint_interruptions() {
    let (endpoint, _) = tool_intent_corpus_endpoint().await;
    let first_interruption = invoke_endpoint(
        &endpoint,
        "ToolIntentCorpusReplay",
        "run",
        TOOL_INTENT_CORPUS_KEY,
        &(),
    )
    .await
    .expect("interrupt at the ToolAttempt run");
    let mid_drain = encode_captured_run_command_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &[],
        &[],
    )
    .expect("capture the completed-attempt journal prefix");
    let second_interruption = invoke_endpoint_body(
        &endpoint,
        "ToolIntentCorpusReplay",
        "run",
        mid_drain.clone(),
    )
    .await
    .expect("interrupt at the intent command run");
    let call_completion = serde_json::to_value(ResolveOutcome::Accepted)
        .expect("serialize durable-wait resolution outcome");
    let mid_intent = encode_captured_run_and_interrupted_call_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &second_interruption,
        None,
    )
    .expect("capture pending intent-command journal");
    let progressed = encode_captured_run_and_interrupted_call_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &second_interruption,
        Some(call_completion.clone()),
    )
    .expect("complete the intent's nested durable-wait call");
    let third_interruption =
        invoke_endpoint_body(&endpoint, "ToolIntentCorpusReplay", "run", progressed)
            .await
            .expect("interrupt while recording the settled intent outcome");
    let full = encode_completed_intent_drain_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &second_interruption,
        &third_interruption,
        call_completion,
    )
    .expect("capture completed intent-command journal");

    let captures = [
        (
            "v2-mid-drain",
            "after_tool_attempt_before_signal_command",
            mid_drain,
        ),
        (
            "v3-mid-intent",
            "after_signal_command_commit_before_reply",
            mid_intent,
        ),
        ("v3-full-drain", "full_drain", full),
    ];
    for (name, crash_point, invocation_body) in captures {
        let mut fixture = ToolIntentJournalCorpusFixture {
            crash_point: crash_point.to_string(),
            captured_from_endpoint_interruption: true,
            invocation_body_bytes: invocation_body.to_vec(),
            expected_response_command_frame_types: Vec::new(),
            expected_output: None,
            expected_signal_events: 0,
        };
        let (frames, output, signal_events) = replay_tool_intent_corpus_fixture(&fixture).await;
        fixture.expected_response_command_frame_types = frames;
        fixture.expected_output = output;
        fixture.expected_signal_events = signal_events;
        let mut bytes = serde_json::to_vec_pretty(&fixture).expect("serialize corpus fixture");
        bytes.push(b'\n');
        std::fs::write(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/tool_intent_journals")
                .join(format!("{name}.json")),
            bytes,
        )
        .expect("write captured endpoint corpus fixture");
    }
}

impl ReplayableRecordingContext {
    pub(super) fn park_sleeps(&self) {
        self.park_sleeps.store(true, Ordering::SeqCst);
    }

    pub(super) async fn await_sleep_started(&self) {
        self.sleep_started.notified().await;
    }

    pub(super) fn release_sleep(&self) {
        self.park_sleeps.store(false, Ordering::SeqCst);
        self.sleep_release.notify_one();
    }

    pub(super) fn crash_after_next_run_commit(&self) {
        self.crash_after_run_commit.store(true, Ordering::SeqCst);
    }

    pub(super) async fn await_run_committed(&self) {
        self.run_committed
            .acquire()
            .await
            .expect("post-commit semaphore remains open")
            .forget();
    }

    pub(super) fn start_replay(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.append_missing_on_replay.store(false, Ordering::SeqCst);
        self.peek_cursor.store(0, Ordering::SeqCst);
    }

    pub(super) fn start_replay_allowing_journal_extension(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.append_missing_on_replay.store(true, Ordering::SeqCst);
        self.peek_cursor.store(0, Ordering::SeqCst);
    }

    pub(super) fn runs(&self) -> Vec<String> {
        self.runs.lock_recover().clone()
    }

    pub(super) fn recorded_runtime_effect_envelopes(&self) -> Vec<(String, RuntimeEffectEnvelope)> {
        let mut envelopes = self
            .records
            .lock_recover()
            .iter()
            .map(|(effect_name, bytes)| {
                let recorded: RecordedRuntimeEffect =
                    serde_json::from_slice(bytes).expect("recorded runtime effect");
                let canonical =
                    serde_json::to_value(recorded.envelope).expect("canonical envelope value");
                let json = canonical
                    .get("json")
                    .and_then(serde_json::Value::as_str)
                    .expect("canonical envelope json");
                let envelope =
                    serde_json::from_str(json).expect("canonical runtime effect envelope");
                (effect_name.clone(), envelope)
            })
            .collect::<Vec<_>>();
        envelopes.sort_by(|left, right| left.0.cmp(&right.0));
        envelopes
    }

    pub(super) fn recorded_runtime_effects(
        &self,
    ) -> std::collections::BTreeMap<String, RecordedRuntimeEffect> {
        self.records
            .lock_recover()
            .iter()
            .map(|(effect_name, bytes)| {
                let recorded =
                    serde_json::from_slice(bytes).expect("decode recorded runtime effect");
                (effect_name.clone(), recorded)
            })
            .collect()
    }

    pub(super) fn install_recorded_runtime_effects(
        &self,
        records: std::collections::BTreeMap<String, RecordedRuntimeEffect>,
    ) {
        *self.records.lock_recover() = records
            .into_iter()
            .map(|(effect_name, recorded)| {
                let bytes = serde_json::to_vec(&recorded)
                    .expect("encode installed recorded runtime effect");
                (effect_name, bytes)
            })
            .collect();
    }

    pub(super) fn recorded_runtime_effect(
        &self,
        effect_name: &str,
    ) -> Option<RecordedRuntimeEffect> {
        self.records
            .lock_recover()
            .get(effect_name)
            .map(|bytes| serde_json::from_slice(bytes).expect("decode recorded runtime effect"))
    }

    pub(super) fn install_process_worker(&self, worker: DurableProcessWorker) {
        *self.process_worker.lock_recover() = Some(worker);
    }

    pub(super) fn defer_process_workflows(&self) {
        self.defer_process_workflows.store(true, Ordering::SeqCst);
    }

    pub(super) fn replay_process_workflow_starts_from_journal(&self) {
        self.replay_process_workflow_starts_from_journal
            .store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
pub(super) struct PositionalReplayContext {
    pub(super) sleeps: Mutex<Vec<u64>>,
    pub(super) runs: Mutex<Vec<String>>,
    pub(super) records: Mutex<Vec<(String, Vec<u8>)>>,
    pub(super) replaying: AtomicBool,
    replay_cursor: AtomicUsize,
    pub(super) turn_cancel_gate: TestTurnCancelGate,
}

impl PositionalReplayContext {
    pub(super) fn start_replay(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.replay_cursor.store(0, Ordering::SeqCst);
    }

    pub(super) fn runs(&self) -> Vec<String> {
        self.runs.lock_recover().clone()
    }

    pub(super) fn record_count(&self) -> usize {
        self.records.lock_recover().len()
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<PositionalReplayContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps.lock_recover().push(duration.as_millis() as u64);
        Box::pin(async { Ok(()) })
    }

    fn sleep_or_turn_cancel<'run>(
        &'run self,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TestTurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run,
    {
        test_sleep_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            duration,
            turn_cancel,
            cancellation,
        )
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs.lock_recover().push(effect_name.clone());
        if self.replaying.load(Ordering::SeqCst) {
            let position = self.replay_cursor.fetch_add(1, Ordering::SeqCst);
            let recorded = self.records.lock_recover().get(position).cloned();
            return Box::pin(async move {
                let (recorded_effect_name, bytes) = recorded.ok_or_else(|| {
                    TerminalError::new(format!("missing recorded effect at position {position}"))
                })?;
                if recorded_effect_name != effect_name {
                    return Err(TerminalError::new(format!(
                        "recorded effect at position {position} was `{recorded_effect_name}`, got `{effect_name}`"
                    )));
                }
                serde_json::from_slice(&bytes)
                    .map(Json)
                    .map_err(TerminalError::from_error)
            });
        }

        let context = Arc::clone(self);
        Box::pin(async move {
            let value = future.await;
            let bytes = serde_json::to_vec(&value).map_err(TerminalError::from_error)?;
            context.records.lock_recover().push((effect_name, bytes));
            Ok(Json(value))
        })
    }

    fn start_process_workflow<'run>(
        &'run self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow start is unsupported")) })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        _request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow cancel is unsupported")) })
    }

    fn await_event<'run>(
        &'run self,
        _request: RestateDurableWaitAwaitRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("event await is unsupported")) })
    }

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TestTurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        test_await_event_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            request,
            turn_cancel,
            cancellation,
        )
    }

    fn peek_event<'run>(
        &'run self,
        _address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(None) })
    }

    fn await_process_terminal<'run>(
        &'run self,
        _process_id: ProcessId,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(std::future::pending())
    }

    fn await_process_terminal_or_turn_cancel<'run>(
        &'run self,
        process_id: ProcessId,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    ) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run,
    {
        test_await_process_terminal_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            process_id,
            turn_cancel,
        )
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let outcome = if self.turn_cancel_gate.resolve(
            &request.key,
            RestateTurnCancelWake::for_gate_resolution(&request.resolution),
        ) {
            ResolveOutcome::Accepted
        } else {
            ResolveOutcome::UnknownOrRevoked
        };
        Box::pin(async move { Ok(outcome) })
    }

    fn update_session_waits<'run>(
        &'run self,
        session_id: SessionId,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        if revoke {
            self.turn_cancel_gate.revoke_session(&session_id);
        }
        Box::pin(async { Ok(()) })
    }

    fn session_is_revoked<'run>(
        &'run self,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let revoked = self.turn_cancel_gate.is_revoked(&session_id);
        Box::pin(async move { Ok(revoked) })
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<ReplayableRecordingContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps.lock_recover().push(duration.as_millis() as u64);
        let context = Arc::clone(self);
        Box::pin(async move {
            if context.park_sleeps.load(Ordering::SeqCst) {
                context.sleep_started.notify_one();
                context.sleep_release.notified().await;
            }
            Ok(())
        })
    }

    fn sleep_or_turn_cancel<'run>(
        &'run self,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TestTurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run,
    {
        test_sleep_or_turn_cancel(
            self,
            &self.events.turn_cancel_gate,
            duration,
            turn_cancel,
            cancellation,
        )
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs.lock_recover().push(effect_name.clone());
        let replaying = self.replaying.load(Ordering::SeqCst);
        if replaying {
            let recorded = self.records.lock_recover().get(&effect_name).cloned();
            if let Some(bytes) = recorded {
                return Box::pin(async move {
                    serde_json::from_slice(&bytes)
                        .map(Json)
                        .map_err(TerminalError::from_error)
                });
            }
            if !self.append_missing_on_replay.load(Ordering::SeqCst) {
                return Box::pin(async move {
                    Err(TerminalError::new(format!(
                        "missing recorded effect `{effect_name}`"
                    )))
                });
            }
        }

        let context = Arc::clone(self);
        Box::pin(async move {
            let value = future.await;
            let bytes = serde_json::to_vec(&value).map_err(TerminalError::from_error)?;
            context.records.lock_recover().insert(effect_name, bytes);
            if context.crash_after_run_commit.swap(false, Ordering::SeqCst) {
                context.run_committed.add_permits(1);
                panic!("injected worker crash after the post-wake effect committed");
            }
            Ok(Json(value))
        })
    }

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let worker = self.process_worker.lock_recover().clone();
        let context = Arc::clone(self);
        Box::pin(async move {
            if context.replaying.load(Ordering::SeqCst)
                && context
                    .replay_process_workflow_starts_from_journal
                    .load(Ordering::SeqCst)
            {
                return Ok(format!("invocation-{}", registration.id));
            }
            if context.defer_process_workflows.load(Ordering::SeqCst) {
                return Ok(format!("invocation-{}", registration.id));
            }
            context
                .live_process_workflow_starts
                .fetch_add(1, Ordering::SeqCst);
            let Some(worker) = worker else {
                return Err(TerminalError::new("process workflow start is unsupported"));
            };
            let process_id = registration.id.clone();
            let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
            let scoped_effect_controller = controller
                .scoped_effect_controller(ExecutionScope::process(&process_id))
                .map_err(TerminalError::from_error)?;
            let cancellation = tokio_util::sync::CancellationToken::new();
            let mut handover = None;
            let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
                &process_id,
                format!("test-workflow:{process_id}"),
            );
            let output = loop {
                match worker
                    .run_process_segment_with_scoped_effect_controller(
                        registration.clone(),
                        execution_context.clone(),
                        execution_write_authority.clone(),
                        scoped_effect_controller.clone(),
                        cancellation.clone(),
                        handover,
                    )
                    .await
                    .map_err(TerminalError::from_error)?
                {
                    lash_core::ProcessRunOutcome::Terminal { output, .. } => {
                        break *output;
                    }
                    lash_core::ProcessRunOutcome::SegmentBoundary(next) => handover = Some(next),
                }
            };
            context
                .events
                .resolve_process_terminal(&process_id, &output);
            Ok(format!("invocation-{process_id}"))
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        _request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(()) })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_event(request, cancellation)
    }

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TestTurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        test_await_event_or_turn_cancel(
            self,
            &self.events.turn_cancel_gate,
            request,
            turn_cancel,
            cancellation,
        )
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let resolution = if self.replaying.load(Ordering::SeqCst) {
            let position = self.peek_cursor.fetch_add(1, Ordering::SeqCst);
            if let Some(recorded) = self.peek_records.lock_recover().get(position).cloned() {
                Ok(recorded)
            } else if self.append_missing_on_replay.load(Ordering::SeqCst) {
                let resolution = self
                    .events
                    .durable_events
                    .lock_recover()
                    .get(&address.workflow_key)
                    .cloned();
                self.peek_records.lock_recover().push(resolution.clone());
                Ok(resolution)
            } else {
                Err(TerminalError::new(format!(
                    "missing recorded await-event peek at position {position}"
                )))
            }
        } else {
            let resolution = self
                .events
                .durable_events
                .lock_recover()
                .get(&address.workflow_key)
                .cloned();
            self.peek_records.lock_recover().push(resolution.clone());
            Ok(resolution)
        };
        Box::pin(async move { resolution })
    }

    fn await_process_terminal<'run>(
        &'run self,
        process_id: ProcessId,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_process_terminal(process_id)
    }

    fn await_process_terminal_or_turn_cancel<'run>(
        &'run self,
        process_id: ProcessId,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    ) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run,
    {
        test_await_process_terminal_or_turn_cancel(
            self,
            &self.events.turn_cancel_gate,
            process_id,
            turn_cancel,
        )
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.resolve_event(request)
    }

    fn update_session_waits<'run>(
        &'run self,
        session_id: SessionId,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.update_session_waits(session_id, revoke)
    }

    fn session_is_revoked<'run>(
        &'run self,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.session_is_revoked(session_id)
    }
}

pub(super) fn runtime_invocation(
    kind: RuntimeEffectKind,
    effect_id: &str,
) -> lash_core::RuntimeEffectInvocation {
    lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            durable_turn_scope("session", "turn"),
            format!("session:turn:1:0:{}:{effect_id}", kind.as_str()),
        )
        .expect("valid recording-context effect address"),
        lash_core::RuntimeAttribution::for_turn("session", "turn", 1, 0),
        effect_id,
    )
}

pub(super) fn test_turn_cancel_wait_request(
    session_id: &SessionId,
    turn_id: &TurnId,
) -> RestateDurableWaitAwaitRequest {
    let key = restate_await_event_key(
        &durable_turn_scope(session_id, turn_id),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("test turn cancellation gate key");
    RestateDurableWaitAwaitRequest {
        key,
        timeout_ms: None,
    }
}
