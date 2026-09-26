// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::controller::context::{ProcessWorkflowStartFailure, ResolveEventFuture};
use crate::durable_wait::RestateDurableWaitResolveResponse;
use lash_core::ProcessEventLogTestSupport as _;

mod helpers;
use helpers::{TestTurnCancelWakeStep, test_turn_cancel_wake_step};
pub(super) use helpers::{runtime_invocation, test_turn_cancel_wait_request};

#[test]
pub(super) fn restate_command_execution_plan_is_explicit_for_every_command() {
    let cases = vec![
        (
            RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For { duration_ms: 1 },
            },
            "timer",
        ),
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
                provider_id: "test".to_string(),
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
            RuntimeEffectCommand::LanguageRuntimeValue {
                operation: "deferred_tool_resolution:v2:[\"web.fetch\"]".to_string(),
            },
            // FIG-2910 intentionally consumes one Restate journal ordinal
            // before any dependent effect in a resource-bearing ExecCode body.
            // Pre-cutover in-flight bodies must be drained or recreated; this
            // command is never folded into the outer DirectLocal run.
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::Checkpoint {
                checkpoint: lash_core::CheckpointKind::AfterWork,
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::SyncExecutionEnvironment,
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::AcceptTurnInput {
                draft: Box::new(lash_core::PendingTurnInputDraft::new(
                    "session",
                    lash_core::TurnInputIngress::next_turn(),
                    lash_core::TurnInput::text("accepted"),
                )),
            },
            "journaled_run",
        ),
        (
            // FIG-3532: the initial drive set is journaled like acceptance.
            RuntimeEffectCommand::ClaimAcceptedTurnInput {
                input_id: lash_core::InputId::from("in_7"),
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
            RestateEffectExecution::Timer { .. } => "timer",
            RestateEffectExecution::AwaitEvent { .. } => "await_event",
            RestateEffectExecution::PeekAwaitEvent { .. } => "peek_await_event",
            RestateEffectExecution::JournaledRun { .. } => "journaled_run",
        };
        assert_eq!(actual, expected);
    }
}

mod turn_cancel_gate;

pub(super) use turn_cancel_gate::*;

#[derive(Default)]
pub(super) struct RecordingContext {
    endpoint: Option<Endpoint>,
    pub(super) block_sleeps: AtomicBool,
    pub(super) sleeps: Mutex<Vec<u64>>,
    pub(super) runs: Mutex<Vec<String>>,
    pub(super) started: Mutex<Vec<ProcessRegistration>>,
    fail_process_workflow_starts: AtomicUsize,
    fail_process_workflow_starts_ambiguously: AtomicUsize,
    started_execution_contexts: Mutex<Vec<ProcessExecutionContext>>,
    pub(super) process_command_log: Mutex<Vec<String>>,
    pub(super) cancelled: Mutex<Vec<RestateProcessCancelRequest>>,
    pub(super) resolved_events: Mutex<Vec<RestateDurableWaitResolveRequest>>,
    pub(super) process_attachments: Mutex<Vec<crate::process_attach::RestateProcessAttachRequest>>,
    pub(super) scope_effect_begins: AtomicUsize,
    pub(super) scope_group_records: AtomicUsize,
    pub(super) awaited_replay_keys: Mutex<Vec<String>>,
    awaited_events: Mutex<HashMap<String, Resolution>>,
    durable_events: Mutex<HashMap<String, Resolution>>,
    durable_event_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    process_terminal_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    session_waits: Mutex<HashMap<SessionId, Vec<AwaitEventKey>>>,
    revoked_sessions: Mutex<HashSet<SessionId>>,
    pub(super) session_revocation_checks: AtomicUsize,
    pub(super) turn_cancel_gate: TestTurnCancelGate,
    /// What the index's `drain_blockers` answers; `Admitted` when unset.
    pub(super) drain_blockers:
        Mutex<Option<Result<crate::effect_group::EffectGroupDrainBlockersResponse, TerminalError>>>,
    /// Every effect-group wake awaited, in order, and what each resolves to.
    pub(super) group_waits: Mutex<Vec<RestateDurableWaitAwaitRequest>>,
    /// The turn-cancel gate each effect-group wake raced, when it raced one.
    pub(super) group_wait_turn_cancels: Mutex<Vec<RestateDurableWaitAwaitRequest>>,
    /// What the index's `read_rank` answers; unregistered when unset.
    pub(super) group_rank_read: Mutex<Option<crate::effect_group::EffectGroupReadRankResponse>>,
    pub(super) group_wait_resolution: Mutex<Option<Resolution>>,
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
    /// The next submission is refused: the reply proves no run was accepted.
    pub(super) fn fail_next_process_workflow_start(&self) {
        self.fail_process_workflow_starts
            .fetch_add(1, Ordering::SeqCst);
    }

    /// The next submission fails with no proof of non-acceptance -- the
    /// connection dropped, or the reply never arrived. A run may be executing.
    pub(super) fn fail_next_process_workflow_start_ambiguously(&self) {
        self.fail_process_workflow_starts_ambiguously
            .fetch_add(1, Ordering::SeqCst);
    }

    pub(super) async fn wait_for_await_event_registration(
        &self,
        session_id: &SessionId,
        key: &AwaitEventKey,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .session_waits
                    .lock_recover()
                    .get(session_id)
                    .is_some_and(|waits| waits.contains(key))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Restate durable waiter is registered before the sweep");
    }

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
        let key = restate_process_terminal_await_key(&test_restate_authority_id(), process_id)
            .expect("terminal await key");
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
    fn attach_process_terminal<'run>(
        &'run self,
        request: crate::process_attach::RestateProcessAttachRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.process_attachments.lock_recover().push(request);
        Box::pin(async move { Ok(()) })
    }

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

    fn scope_group_child_membership<'run>(
        &'run self,
        _index_key: String,
        _replay_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(None) })
    }

    fn effect_group_drain_blockers<'run>(
        &'run self,
        _group_key: String,
        _commit_seq: u64,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::effect_group::EffectGroupDrainBlockersResponse,
                        TerminalError,
                    >,
                > + Send
                + 'run,
        >,
    >
    where
        'ctx: 'run,
    {
        let answer = self.drain_blockers.lock_recover().clone().unwrap_or(Ok(
            crate::effect_group::EffectGroupDrainBlockersResponse::Admitted,
        ));
        Box::pin(async move { answer })
    }

    fn effect_group_read_rank<'run>(
        &'run self,
        _group_key: String,
        _request: crate::effect_group::EffectGroupReadRankRequest,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::effect_group::EffectGroupReadRankResponse,
                        TerminalError,
                    >,
                > + Send
                + 'run,
        >,
    >
    where
        'ctx: 'run,
    {
        let answer = self.group_rank_read.lock_recover().clone();
        Box::pin(async move {
            answer.ok_or_else(|| TerminalError::new("EffectGroupIndex/read_rank is not registered"))
        })
    }

    fn await_effect_group_wait<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        _replay_key: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        self.group_waits.lock_recover().push(request);
        if let Some(turn_cancel) = turn_cancel {
            self.group_wait_turn_cancels
                .lock_recover()
                .push(turn_cancel);
        }
        let resolution = self.group_wait_resolution.lock_recover().clone();
        Box::pin(async move {
            Ok(match resolution {
                Some(resolution) => RestateTurnCancelRaceOutcome::Completed(resolution),
                None => RestateTurnCancelRaceOutcome::TurnCancelled,
            })
        })
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
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run,
    {
        test_sleep_or_turn_cancel(self, &self.turn_cancel_gate, duration, turn_cancel, None)
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
        process_id: lash_core::ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProcessWorkflowStartFailure>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let endpoint = self.endpoint.clone();
        self.process_command_log
            .lock_recover()
            .push(format!("send:{process_id}"));
        self.started.lock_recover().push(registration.clone());
        self.started_execution_contexts
            .lock_recover()
            .push(execution_context.clone());
        Box::pin(async move {
            if self
                .fail_process_workflow_starts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ProcessWorkflowStartFailure::Rejected(TerminalError::new(
                    "injected process workflow start failure",
                )));
            }
            if self
                .fail_process_workflow_starts_ambiguously
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ProcessWorkflowStartFailure::Ambiguous(TerminalError::new(
                    "injected ambiguous process workflow start failure",
                )));
            }
            if let Some(endpoint) = endpoint {
                // Every run completes: the handler's admission steps are
                // runs (FIG-3588), ahead of whatever the process does.
                invoke_process_workflow_endpoint(
                    &endpoint,
                    "run",
                    process_id.as_str(),
                    &RestateProcessWorkflowInput {
                        process_id: process_id.clone(),
                        registration,
                        execution_context,
                        segment_ordinal: 0,
                        journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
                    },
                    true,
                )
                .await
                .map_err(ProcessWorkflowStartFailure::Rejected)?;
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
        self.cancelled.lock_recover().push(request.clone());
        Box::pin(async move {
            if let Some(endpoint) = endpoint {
                invoke_process_workflow_endpoint(
                    &endpoint,
                    "cancel",
                    process_id.as_str(),
                    &request,
                    false,
                )
                .await?;
            }
            Ok(())
        })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.awaited_replay_keys.lock_recover().push(replay_key);
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
                if let Some(deadline) = request.deadline {
                    let timeout = deadline.remaining(lash_core::ClockWallTime::timestamp_ms(
                        &lash_core::facade_support::SystemClock,
                    ))?;
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = cancellation.cancelled() => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                key: request.key.clone(),
                                resolution: Resolution::Cancelled,
                            });
                        }
                        _ = tokio::time::sleep(timeout) => {
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
        replay_key: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        test_await_event_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            request,
            replay_key,
            turn_cancel,
            None,
        )
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
        _replay_key: String,
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
            let key = restate_process_terminal_await_key(&test_restate_authority_id(), &process_id)
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
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run,
    {
        test_await_process_terminal_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            process_id,
            turn_cancel,
            None,
        )
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveEventFuture<'run>
    where
        'ctx: 'run,
    {
        let outcome = self.resolve_durable_event(request);
        Box::pin(async move { Ok(RestateDurableWaitResolveResponse::Outcome(outcome)) })
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
        self.session_revocation_checks
            .fetch_add(1, Ordering::SeqCst);
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
    pub(super) journal_commands: Mutex<Vec<String>>,
    pub(super) records: Mutex<HashMap<String, Vec<u8>>>,
    pub(super) replaying: AtomicBool,
    pub(super) append_missing_on_replay: AtomicBool,
    pub(super) peek_records: Mutex<Vec<Option<Resolution>>>,
    pub(super) peek_cursor: AtomicUsize,
    /// Live process cancellation state, standing in for the resolved
    /// `process_cancel_requested` workflow promise (FIG-3149).
    pub(super) process_cancel_committed: AtomicBool,
    pub(super) process_cancel_peek_records: Mutex<Vec<bool>>,
    pub(super) process_cancel_peek_cursor: AtomicUsize,
    /// Wakes a recorded process cancel race when the stand-in promise is
    /// committed (FIG-3673).
    pub(super) process_cancel_notify: tokio::sync::Notify,
    /// Which side each process-drive wait's recorded race took: `true` when
    /// the cancel promise won.
    pub(super) process_cancel_race_records: Mutex<Vec<bool>>,
    pub(super) process_cancel_race_cursor: AtomicUsize,
    pub(super) events: Arc<RecordingContext>,
    pub(super) process_worker: Mutex<Option<lash_core_worker::DurableProcessWorker>>,
    pub(super) defer_process_workflows: AtomicBool,
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
/// The corpus's signal target: the first process the corpus backend's
/// sequential mint registers, so the checked-in journal bytes name it
/// (ADR 0107).
pub(super) fn tool_intent_corpus_target() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(1)
}

#[restate_sdk::workflow]
trait ToolIntentCorpusReplay {
    async fn run(input: Json<()>) -> HandlerResult<Json<serde_json::Value>>;
}

pub(super) struct ToolIntentCorpusReplayImpl {
    registry: Arc<dyn ProcessRegistry>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
}

impl ToolIntentCorpusReplay for ToolIntentCorpusReplayImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(()): Json<()>,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let controller = RestateRuntimeEffectController::new_for_test(ctx);
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
                            intents: lash_core::ToolIntents::v3(vec![
                                lash_core::ToolIntent::SignalProcess(
                                    lash_core::SignalProcessIntent {
                                        session_id: SessionId::from(
                                            TOOL_INTENT_CORPUS_SESSION.to_string(),
                                        ),
                                        process_id: tool_intent_corpus_target(),
                                        signal_name: "resume".to_string(),
                                        payload: serde_json::json!({
                                            "source": "checked-in-endpoint-corpus"
                                        }),
                                    },
                                ),
                            ]),
                        }),
                        triggers: Vec::new(),
                        capture: None,
                    })
                }),
            )
            .await
            .map_err(|error| -> HandlerError {
                // A turn handler's contract (lash_restate::turn_service): a
                // refusal that parks fails the attempt retryably, so the
                // invocation keeps its journal.
                if error.turn_failure_cause() == lash_core::TurnFailureCause::Parked {
                    crate::parked_turn_failure(error)
                } else {
                    TerminalError::from_error(error).into()
                }
            })?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = attempt else {
            return Err(TerminalError::new("corpus attempt returned the wrong effect").into());
        };
        let lash_core::ToolAttemptLaunch::Done { intents, .. } = *launch else {
            return Err(TerminalError::new("corpus attempt did not finish").into());
        };
        let outcomes = lash_core::testing::execute_tool_intents_with_services(
            controller
                .scoped_effect_controller(durable_admission(&scope))
                .map_err(TerminalError::from_error)?,
            lash_core::testing::effect_backed_process_service(
                Arc::clone(&self.registry),
                Arc::clone(&self.process_env_store),
            ),
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
    let backend = lash_sqlite_store::SqliteBackend::memory_with_options_and_clock(
        lash_sqlite_store::SqliteBackendOptions {
            process_id_mint: lash_core::ProcessIdMint::sequential_for_testing(),
            ..lash_sqlite_store::SqliteBackendOptions::memory()
        },
        clock,
    )
    .await
    .expect("open corpus backend");
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        backend.process_env_store();
    let tool_intent_corpus_target = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "endpoint-corpus"}),
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("seed corpus signal target")
        .id;
    assert_eq!(tool_intent_corpus_target, self::tool_intent_corpus_target());
    let endpoint = Endpoint::builder()
        .bind(
            ToolIntentCorpusReplayImpl {
                registry: Arc::clone(&registry),
                process_env_store,
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
        .full_event_window(&tool_intent_corpus_target(), 0)
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
        include_bytes!("../../tests/fixtures/tool_intent_journals/v20-mid-drain.json").as_slice(),
        include_bytes!("../../tests/fixtures/tool_intent_journals/v20-mid-intent.json").as_slice(),
        include_bytes!("../../tests/fixtures/tool_intent_journals/v20-full-drain.json").as_slice(),
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

/// Journals another effect-journal generation wrote (ADR 0105 §12) replay
/// their shape unchanged, so they reach the generation gate: the tool
/// attempt's journal entry carries no generation (it predates the stamp) or a
/// retired one, and is refused, typed, before the attempt's outcome is acted
/// on. Nothing is dispatched, so the recorded signal effect never reaches a
/// fresh registry.
#[tokio::test]
pub(super) async fn checked_in_tool_intent_journals_of_other_generations_refuse_at_the_generation_gate()
 {
    const PRE_STAMP: &str = "carries no effect-journal generation";
    const GENERATION_ONE: &str = "carries effect-journal generation 1;";
    const GENERATION_TWO: &str = "carries effect-journal generation 2;";
    const GENERATION_THREE: &str = "carries effect-journal generation 3;";
    const GENERATION_FOUR: &str = "carries effect-journal generation 4;";
    const GENERATION_FIVE: &str = "carries effect-journal generation 5;";
    const GENERATION_SIX: &str = "carries effect-journal generation 6;";
    const GENERATION_SEVEN: &str = "carries effect-journal generation 7;";
    const GENERATION_EIGHT: &str = "carries effect-journal generation 8;";
    const GENERATION_NINE: &str = "carries effect-journal generation 9;";
    const GENERATION_TEN: &str = "carries effect-journal generation 10;";
    const GENERATION_ELEVEN: &str = "carries effect-journal generation 11;";
    const GENERATION_TWELVE: &str = "carries effect-journal generation 12;";
    const GENERATION_THIRTEEN: &str = "carries effect-journal generation 13;";
    const GENERATION_FOURTEEN: &str = "carries effect-journal generation 14;";
    for (name, checked_in, refusal) in [
        (
            "v1-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v1-full-drain.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v2-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v2-mid-drain.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v2-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v2-mid-intent.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v2-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v2-full-drain.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v3-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v3-mid-intent.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v3-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v3-full-drain.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v5-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v5-mid-drain.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v5-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v5-mid-intent.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v5-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v5-full-drain.json")
                .as_slice(),
            PRE_STAMP,
        ),
        (
            "v6-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v6-mid-drain.json")
                .as_slice(),
            GENERATION_ONE,
        ),
        (
            "v6-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v6-mid-intent.json")
                .as_slice(),
            GENERATION_ONE,
        ),
        (
            "v6-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v6-full-drain.json")
                .as_slice(),
            GENERATION_ONE,
        ),
        (
            "v7-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v7-mid-drain.json")
                .as_slice(),
            GENERATION_TWO,
        ),
        (
            "v7-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v7-mid-intent.json")
                .as_slice(),
            GENERATION_TWO,
        ),
        (
            "v7-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v7-full-drain.json")
                .as_slice(),
            GENERATION_TWO,
        ),
        (
            "v8-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v8-mid-drain.json")
                .as_slice(),
            GENERATION_THREE,
        ),
        (
            "v8-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v8-mid-intent.json")
                .as_slice(),
            GENERATION_THREE,
        ),
        (
            "v8-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v8-full-drain.json")
                .as_slice(),
            GENERATION_THREE,
        ),
        (
            "v9-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v9-mid-drain.json")
                .as_slice(),
            GENERATION_FOUR,
        ),
        (
            "v9-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v9-mid-intent.json")
                .as_slice(),
            GENERATION_FOUR,
        ),
        (
            "v9-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v9-full-drain.json")
                .as_slice(),
            GENERATION_FOUR,
        ),
        (
            "v10-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v10-mid-drain.json")
                .as_slice(),
            GENERATION_FIVE,
        ),
        (
            "v10-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v10-mid-intent.json")
                .as_slice(),
            GENERATION_FIVE,
        ),
        (
            "v10-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v10-full-drain.json")
                .as_slice(),
            GENERATION_FIVE,
        ),
        (
            "v11-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v11-mid-drain.json")
                .as_slice(),
            GENERATION_SIX,
        ),
        (
            "v11-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v11-mid-intent.json")
                .as_slice(),
            GENERATION_SIX,
        ),
        (
            "v11-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v11-full-drain.json")
                .as_slice(),
            GENERATION_SIX,
        ),
        (
            "v12-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v12-mid-drain.json")
                .as_slice(),
            GENERATION_SEVEN,
        ),
        (
            "v12-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v12-mid-intent.json")
                .as_slice(),
            GENERATION_SEVEN,
        ),
        (
            "v12-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v12-full-drain.json")
                .as_slice(),
            GENERATION_SEVEN,
        ),
        (
            "v13-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v13-mid-drain.json")
                .as_slice(),
            GENERATION_EIGHT,
        ),
        (
            "v13-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v13-mid-intent.json")
                .as_slice(),
            GENERATION_EIGHT,
        ),
        (
            "v13-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v13-full-drain.json")
                .as_slice(),
            GENERATION_EIGHT,
        ),
        (
            "v14-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v14-mid-drain.json")
                .as_slice(),
            GENERATION_NINE,
        ),
        (
            "v14-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v14-mid-intent.json")
                .as_slice(),
            GENERATION_NINE,
        ),
        (
            "v14-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v14-full-drain.json")
                .as_slice(),
            GENERATION_NINE,
        ),
        (
            "v15-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v15-mid-drain.json")
                .as_slice(),
            GENERATION_TEN,
        ),
        (
            "v15-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v15-mid-intent.json")
                .as_slice(),
            GENERATION_TEN,
        ),
        (
            "v15-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v15-full-drain.json")
                .as_slice(),
            GENERATION_TEN,
        ),
        (
            "v16-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v16-mid-drain.json")
                .as_slice(),
            GENERATION_ELEVEN,
        ),
        (
            "v16-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v16-mid-intent.json")
                .as_slice(),
            GENERATION_ELEVEN,
        ),
        (
            "v16-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v16-full-drain.json")
                .as_slice(),
            GENERATION_ELEVEN,
        ),
        (
            "v17-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v17-mid-drain.json")
                .as_slice(),
            GENERATION_TWELVE,
        ),
        (
            "v17-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v17-mid-intent.json")
                .as_slice(),
            GENERATION_TWELVE,
        ),
        (
            "v17-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v17-full-drain.json")
                .as_slice(),
            GENERATION_TWELVE,
        ),
        (
            "v18-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v18-mid-drain.json")
                .as_slice(),
            GENERATION_THIRTEEN,
        ),
        (
            "v18-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v18-mid-intent.json")
                .as_slice(),
            GENERATION_THIRTEEN,
        ),
        (
            "v18-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v18-full-drain.json")
                .as_slice(),
            GENERATION_THIRTEEN,
        ),
        (
            "v19-mid-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v19-mid-drain.json")
                .as_slice(),
            GENERATION_FOURTEEN,
        ),
        (
            "v19-mid-intent",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v19-mid-intent.json")
                .as_slice(),
            GENERATION_FOURTEEN,
        ),
        (
            "v19-full-drain",
            include_bytes!("../../tests/fixtures/tool_intent_journals/v19-full-drain.json")
                .as_slice(),
            GENERATION_FOURTEEN,
        ),
    ] {
        let fixture: ToolIntentJournalCorpusFixture = serde_json::from_slice(checked_in)
            .expect("decode the checked-in endpoint corpus fixture");
        let (endpoint, registry) = tool_intent_corpus_endpoint().await;
        let response = invoke_endpoint_body(
            &endpoint,
            "ToolIntentCorpusReplay",
            "run",
            bytes::Bytes::from(fixture.invocation_body_bytes),
        )
        .await
        .expect("feed the retired journal through the current endpoint");
        let error = restate_output_failure_message(&response)
            .or_else(|| restate_error_message(&response))
            .unwrap_or_else(|| {
                panic!(
                    "{name} replay must refuse; frames={:?}; output={:?}",
                    restate_command_frame_types(&response),
                    restate_output_json::<serde_json::Value>(&response)
                )
            });
        assert!(
            error.contains("effect_replay_divergence") && error.contains(refusal),
            "{name} must refuse at the effect-journal generation gate: {error}"
        );
        assert_eq!(
            restate_command_frame_types(&response),
            Vec::<u16>::new(),
            "{name}: the refusal journals nothing further"
        );
        assert_eq!(
            registry
                .full_event_window(&tool_intent_corpus_target(), 0)
                .await
                .expect("read the refusal witness target")
                .into_iter()
                .filter(|event| event.event_type == "signal.resume")
                .count(),
            0,
            "{name}: the refusal happens before any effect is dispatched"
        );
    }
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
            "v20-mid-drain",
            "after_tool_attempt_before_signal_command",
            mid_drain,
        ),
        (
            "v20-mid-intent",
            "after_signal_command_commit_before_reply",
            mid_intent,
        ),
        ("v20-full-drain", "full_drain", full),
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
        // Under `kiln run` the source tree is the Bazel workspace, not the
        // compile-time manifest directory.
        let manifest_dir = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map_or_else(
            || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            |root| std::path::PathBuf::from(root).join("crates/lash-restate"),
        );
        std::fs::write(
            manifest_dir
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
        self.process_cancel_peek_cursor.store(0, Ordering::SeqCst);
        self.process_cancel_race_cursor.store(0, Ordering::SeqCst);
    }

    pub(super) fn start_replay_allowing_journal_extension(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.append_missing_on_replay.store(true, Ordering::SeqCst);
        self.peek_cursor.store(0, Ordering::SeqCst);
        self.process_cancel_peek_cursor.store(0, Ordering::SeqCst);
        self.process_cancel_race_cursor.store(0, Ordering::SeqCst);
    }

    /// Resolves the stand-in process cancellation promise.
    pub(super) fn commit_process_cancel(&self) {
        self.process_cancel_committed.store(true, Ordering::SeqCst);
        self.process_cancel_notify.notify_waiters();
    }

    /// The answers each journaled process cancel peek recorded (FIG-3673).
    pub(super) fn process_cancel_peek_verdicts(&self) -> Vec<bool> {
        self.process_cancel_peek_records.lock_recover().clone()
    }

    /// Which side each recorded process cancel race took (FIG-3673).
    pub(super) fn process_cancel_race_verdicts(&self) -> Vec<bool> {
        self.process_cancel_race_records.lock_recover().clone()
    }

    /// The stand-in cancel promise a process-drive wait races. A live race
    /// resolves it once the cancel is committed; a replayed race answers from
    /// the recorded winner, never from live state.
    fn test_process_cancel(
        self: &Arc<Self>,
        race: ProcessCancelRace,
    ) -> TestProcessCancel<'static> {
        if race == ProcessCancelRace::NotRaced {
            return None;
        }
        let context = Arc::clone(self);
        if context.replaying.load(Ordering::SeqCst) {
            let cursor = context
                .process_cancel_race_cursor
                .fetch_add(1, Ordering::SeqCst);
            let won = context
                .process_cancel_race_records
                .lock_recover()
                .get(cursor)
                .copied()
                .unwrap_or(false);
            return Some(Box::pin(async move {
                if !won {
                    std::future::pending::<()>().await;
                }
            }));
        }
        Some(Box::pin(async move {
            loop {
                let notified = context.process_cancel_notify.notified();
                if context.process_cancel_committed.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        }))
    }

    /// Record a live race's winner; a replay reads it back.
    fn record_process_cancel_race<T>(
        &self,
        race: ProcessCancelRace,
        outcome: &Result<RestateTurnCancelRaceOutcome<T>, TerminalError>,
    ) {
        if race == ProcessCancelRace::Raced && !self.replaying.load(Ordering::SeqCst) {
            self.process_cancel_race_records
                .lock_recover()
                .push(matches!(
                    outcome,
                    Ok(RestateTurnCancelRaceOutcome::ProcessCancelled)
                ));
        }
    }

    /// Clears live cancellation so a replayed wake can only answer from the
    /// journal.
    pub(super) fn clear_process_cancel(&self) {
        self.process_cancel_committed.store(false, Ordering::SeqCst);
    }

    pub(super) fn runs(&self) -> Vec<String> {
        self.runs.lock_recover().clone()
    }

    pub(super) fn recorded_runtime_effect_envelopes(&self) -> Vec<(String, RuntimeEffectEnvelope)> {
        let mut envelopes = self
            .records
            .lock_recover()
            .iter()
            .filter(|(effect_name, _)| !is_process_command_journal_fact(effect_name))
            .map(|(effect_name, bytes)| {
                let recorded = decode_recorded_runtime_effect(bytes);
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
            .filter(|(effect_name, _)| !is_process_command_journal_fact(effect_name))
            .map(|(effect_name, bytes)| {
                (effect_name.clone(), decode_recorded_runtime_effect(bytes))
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
                // Installed as the journal holds it: stamped with this
                // build's effect-journal generation.
                let bytes = serde_json::to_vec(&JournaledEffectRecord::Recorded(recorded))
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
            .map(|bytes| decode_recorded_runtime_effect(bytes.as_slice()))
    }

    pub(super) fn install_process_worker(&self, worker: DurableProcessWorker) {
        *self.process_worker.lock_recover() = Some(worker);
    }
}

/// A journaled step that is not a recorded effect: a process command's
/// journaled fact (a cancel admission, an await's existence guard, a process
/// wait step, or a start's registration, compensation and external reference,
/// ADR 0107), or the frontier marker a process start or a sleep journals
/// before it acts (FIG-3779).
fn is_process_command_journal_fact(effect_name: &str) -> bool {
    [
        ".process-cancel-admission:v1",
        ".process-await-guard:v1",
        ".process-start-register:v1",
        ".process-start-compensate:v1",
        ".process-start-external-ref:v1",
    ]
    .iter()
    .any(|suffix| effect_name.ends_with(suffix))
        || effect_name.starts_with("lash.process.wait.")
        || effect_name.ends_with(":frontier")
}

/// Decodes one journaled record into its recorded effect. A step whose engine
/// faults are retried journals its run's `Result` under these contexts — a
/// recording context cannot end the attempt, so `run_json_or_retry_send`'s
/// default wraps the record in `{"Ok": ...}` (or the fault in `{"Err": ...}`);
/// a step whose faults are recorded journals the stamped record bare.
fn decode_recorded_runtime_effect(bytes: &[u8]) -> RecordedRuntimeEffect {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("decode journaled record");
    let unwrapped = match value {
        serde_json::Value::Object(mut entry)
            if entry.contains_key("Ok") || entry.contains_key("Err") =>
        {
            match entry.remove("Ok").or_else(|| entry.remove("Err")) {
                Some(inner) if inner.is_object() => inner,
                Some(serde_json::Value::String(fault)) => {
                    panic!("the step's fault was journaled as its run's result: {fault}")
                }
                other => panic!("malformed journaled run result: {other:?}"),
            }
        }
        value => value,
    };
    serde_json::from_value(unwrapped).expect("recorded runtime effect")
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
    fn attach_process_terminal<'run>(
        &'run self,
        _request: crate::process_attach::RestateProcessAttachRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async move { Ok(()) })
    }

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
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run,
    {
        test_sleep_or_turn_cancel(self, &self.turn_cancel_gate, duration, turn_cancel, None)
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
        _process_id: lash_core::ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProcessWorkflowStartFailure>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(ProcessWorkflowStartFailure::Rejected(TerminalError::new(
                "process workflow start is unsupported",
            )))
        })
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
        _replay_key: String,
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
        replay_key: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        test_await_event_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            request,
            replay_key,
            turn_cancel,
            None,
        )
    }

    fn peek_event<'run>(
        &'run self,
        _address: RestateDurableWaitAddress,
        _replay_key: String,
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
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run,
    {
        test_await_process_terminal_or_turn_cancel(
            self,
            &self.turn_cancel_gate,
            process_id,
            turn_cancel,
            None,
        )
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveEventFuture<'run>
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
        Box::pin(async move { Ok(RestateDurableWaitResolveResponse::Outcome(outcome)) })
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
    /// Journaled wake verdict (FIG-3149). A live wake records the verdict it
    /// observed; a replayed wake answers from that record, never from live
    /// state, and a journal written before the command existed extends only
    /// when the fixture allows it.
    fn peek_process_cancel_requested<'run>(
        &'run self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let context = Arc::clone(self);
        Box::pin(async move {
            if context.replaying.load(Ordering::SeqCst) {
                let cursor = context
                    .process_cancel_peek_cursor
                    .fetch_add(1, Ordering::SeqCst);
                let recorded = context
                    .process_cancel_peek_records
                    .lock_recover()
                    .get(cursor)
                    .copied();
                if let Some(recorded) = recorded {
                    return Ok(recorded);
                }
                if !context.append_missing_on_replay.load(Ordering::SeqCst) {
                    return Err(TerminalError::new(
                        "missing recorded process cancellation wake verdict",
                    ));
                }
            }
            let verdict = context.process_cancel_committed.load(Ordering::SeqCst);
            context
                .process_cancel_peek_records
                .lock_recover()
                .push(verdict);
            Ok(verdict)
        })
    }

    fn attach_process_terminal<'run>(
        &'run self,
        request: crate::process_attach::RestateProcessAttachRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.attach_process_terminal(request)
    }

    fn scope_group_child_membership<'run>(
        &'run self,
        index_key: String,
        replay_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events
            .scope_group_child_membership(index_key, replay_key)
    }

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
            // A replayed timer completes from its journal entry.
            if context.park_sleeps.load(Ordering::SeqCst)
                && !context.replaying.load(Ordering::SeqCst)
            {
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
        process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run,
    {
        let race = test_sleep_or_turn_cancel(
            self,
            &self.events.turn_cancel_gate,
            duration,
            turn_cancel,
            self.test_process_cancel(process_cancel),
        );
        Box::pin(async move {
            let outcome = race.await;
            self.record_process_cancel_race(process_cancel, &outcome);
            outcome
        })
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
        self.journal_commands
            .lock_recover()
            .push(format!("run:{effect_name}"));
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
        process_id: lash_core::ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProcessWorkflowStartFailure>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let worker = self.process_worker.lock_recover().clone();
        let context = Arc::clone(self);
        Box::pin(async move {
            if context.defer_process_workflows.load(Ordering::SeqCst) {
                return Ok(format!("invocation-{process_id}"));
            }
            let Some(worker) = worker else {
                return Err(ProcessWorkflowStartFailure::Rejected(TerminalError::new(
                    "process workflow start is unsupported",
                )));
            };
            let process_task_context = Arc::clone(&context);
            let process_task_id = process_id.clone();
            let process_task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                let controller = RestateRuntimeEffectController::new_for_test(process_task_context);
                let scoped_effect_controller = controller
                    .process_scope_for_test(
                        recorded_process_admission(
                            worker.config().process_registry().as_ref(),
                            &process_task_id,
                        )
                        .await,
                    )
                    .map_err(TerminalError::from_error)?;
                let cancellation = tokio_util::sync::CancellationToken::new();
                let execution_write_authority =
                    lash_core::ProcessExecutionWriteAuthority::invocation(
                        &process_task_id,
                        format!("test-workflow:{process_task_id}"),
                    );
                let mut handover = None;
                loop {
                    match worker
                        .run_process_segment_with_scoped_effect_controller(
                            process_task_id.clone(),
                            registration.clone(),
                            execution_context.clone(),
                            execution_write_authority.clone(),
                            scoped_effect_controller.clone(),
                            cancellation.clone(),
                            handover,
                        )
                        .await
                    {
                        Ok(lash_core::ProcessRunOutcome::Terminal { output, .. }) => {
                            break Ok(*output);
                        }
                        Ok(lash_core::ProcessRunOutcome::SegmentBoundary(next)) => {
                            handover = Some(next);
                        }
                        Err(error) => break Err(TerminalError::from_error(error)),
                    }
                }
            }));
            let output = match process_task.await {
                Ok(Ok(output)) => output,
                // The task ran: whatever it reports, the run was accepted, so
                // this is never proof of non-acceptance. `Rejected` here would
                // contradict its own invariant inside the harness that proves
                // compensation.
                Ok(Err(error)) => return Err(ProcessWorkflowStartFailure::Ambiguous(error)),
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(error) => {
                    return Err(ProcessWorkflowStartFailure::Ambiguous(TerminalError::new(
                        format!("test process workflow task failed: {error}"),
                    )));
                }
            };
            let invocation = format!("invocation-{process_id}");
            context
                .events
                .resolve_process_terminal(&process_id, &output);
            Ok(invocation)
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.journal_commands
            .lock_recover()
            .push(format!("call:process-cancel:{}", request.process_id));
        self.events.cancelled.lock_recover().push(request);
        Box::pin(async { Ok(()) })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_event(request, replay_key, cancellation)
    }

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        let race = test_await_event_or_turn_cancel(
            self,
            &self.events.turn_cancel_gate,
            request,
            replay_key,
            turn_cancel,
            self.test_process_cancel(process_cancel),
        );
        Box::pin(async move {
            let outcome = race.await;
            self.record_process_cancel_race(process_cancel, &outcome);
            outcome
        })
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
        _replay_key: String,
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
        process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run,
    {
        let race = test_await_process_terminal_or_turn_cancel(
            self,
            &self.events.turn_cancel_gate,
            process_id,
            turn_cancel,
            self.test_process_cancel(process_cancel),
        );
        Box::pin(async move {
            let outcome = race.await;
            self.record_process_cancel_race(process_cancel, &outcome);
            outcome
        })
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveEventFuture<'run>
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
