// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::controller::context::{ProcessWorkflowStartFailure, ResolveEventFuture};
use crate::durable_wait::RestateDurableWaitResolveResponse;

mod helpers;
pub(super) use helpers::runtime_invocation;
use helpers::{TestTurnCancelWakeStep, test_turn_cancel_wake_step};

#[macro_use]
mod attempt;
pub(crate) use attempt::{AttemptEnd, AttemptFailure, run_json_or_end_attempt};
mod turn_cancel_gate;

pub(super) use turn_cancel_gate::*;

#[derive(Default)]
pub(super) struct RecordingContext {
    /// The attempt this context runs, which a step's retried fault ends.
    pub(super) attempt: AttemptEnd,
    pub(super) block_sleeps: AtomicBool,
    pub(super) sleeps: Mutex<Vec<u64>>,
    pub(super) runs: Mutex<Vec<String>>,
    pub(super) started: Mutex<Vec<ProcessRegistration>>,
    fail_process_workflow_starts: AtomicUsize,
    fail_process_workflow_starts_ambiguously: AtomicUsize,
    cancel_process_workflow_starts: AtomicUsize,
    /// Journal operations whose next run the engine's cancellation of the
    /// invocation interrupts after its closure ran.
    cancel_after_runs: Mutex<Vec<String>>,
    /// A registry the next submission's run is started through before the
    /// submission itself is refused: another delivery of the start won.
    start_elsewhere_then_refuse: Mutex<Option<Arc<dyn ProcessRegistry>>>,
    started_execution_contexts: Mutex<Vec<ProcessExecutionContext>>,
    pub(super) process_command_log: Mutex<Vec<String>>,
    pub(super) cancelled: Mutex<Vec<RestateProcessCancelRequest>>,
    pub(super) resolved_events: Mutex<Vec<RestateDurableWaitResolveRequest>>,
    pub(super) process_attachments:
        Mutex<Vec<crate::durable_wait::process_terminal::RestateProcessTerminalRequest>>,
    pub(super) scope_effect_begins: AtomicUsize,
    pub(super) awaited_replay_keys: Mutex<Vec<String>>,
    pub(super) awaited_requests: Mutex<Vec<RestateDurableWaitAwaitRequest>>,
    awaited_events: Mutex<HashMap<String, Resolution>>,
    durable_events: Mutex<HashMap<String, Resolution>>,
    durable_event_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    process_terminal_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    session_waits: Mutex<HashMap<SessionId, Vec<AwaitEventKey>>>,
    revoked_sessions: Mutex<HashSet<SessionId>>,
    pub(super) session_revocation_checks: AtomicUsize,
    /// Set, the engine cancels the invocation while the next read of a
    /// session's revocation awaits its answer: the read answers `409`.
    pub(super) cancel_revocation_read: AtomicBool,
    pub(super) turn_cancel_gate: TestTurnCancelGate,
    /// Source select keys this context hands out: a counter stands in for
    /// the engine's notification handles.
    pub(super) select_keys: AtomicU64,
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

    /// The engine cancels the invocation while the next submission's await
    /// is pending: the send was journaled, and its await answers `409`.
    pub(super) fn cancel_next_process_workflow_start(&self) {
        self.cancel_process_workflow_starts
            .fetch_add(1, Ordering::SeqCst);
    }

    /// The engine cancels the invocation while the next run of the journal
    /// step named `journal_name` awaits its answer: the closure ran, and the
    /// step answers `409`.
    pub(super) fn cancel_after_next_run(&self, journal_name: String) {
        self.cancel_after_runs.lock_recover().push(journal_name);
    }

    /// The next submission is refused, after another delivery of the start
    /// already had the run start the process through `registry`.
    pub(super) fn start_elsewhere_then_refuse_next(&self, registry: Arc<dyn ProcessRegistry>) {
        *self.start_elsewhere_then_refuse.lock_recover() = Some(registry);
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
        .expect("the Restate durable waiter is registered before the reconcile");
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

    /// The terminal the test published for `process_id`, as the attach
    /// workflow resolves a direct await's wait with it.
    async fn published_terminal(&self, process_id: &ProcessId) -> Resolution {
        let key = restate_process_terminal_await_key(&test_restate_authority_id(), process_id)
            .expect("terminal await key");
        let notify = self.process_terminal_notify(process_id);
        let resolution = loop {
            let notified = notify.notified();
            if let Some(resolution) = self
                .awaited_events
                .lock_recover()
                .get(&key.promise_key())
                .cloned()
            {
                break resolution;
            }
            notified.await;
        };
        let output = crate::process::restate_process_terminal_output(process_id, resolution)
            .expect("a published terminal decodes");
        crate::process::restate_process_terminal_resolution(&output)
            .expect("a published terminal encodes")
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
        self.settle_durable_event(request)
    }

    /// Settle a wait as the attach workflow does from its own journal: the
    /// controller under test resolved nothing, so `resolved_events` does not
    /// record it.
    fn settle_attached_terminal(&self, request: RestateDurableWaitResolveRequest) {
        self.turn_cancel_gate.resolve(
            &request.key,
            RestateTurnCancelWake::for_gate_resolution(&request.resolution),
        );
        self.settle_durable_event(request);
    }

    fn settle_durable_event(&self, request: RestateDurableWaitResolveRequest) -> ResolveOutcome {
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
    fn invocation_id(&self) -> &str {
        "RecordingContext"
    }

    fn attach_process_terminal<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        request: crate::durable_wait::process_terminal::RestateProcessTerminalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let direct_await = matches!(
            &request.key.wait,
            lash_core::AwaitEventWaitIdentity::Custom { key } if key.starts_with("process-await:")
        );
        self.process_attachments
            .lock_recover()
            .push(request.clone());
        if direct_await {
            // Stand in for the attach workflow of a direct await: its
            // terminal call, then the wait resolved with the terminal the
            // test publishes. A parked call's attach is resolved by its test.
            self.process_command_log
                .lock_recover()
                .push(format!("call:{}", request.process_id));
            let context = Arc::clone(self);
            tokio::spawn(async move {
                let resolution = context.published_terminal(&request.process_id).await;
                context.settle_attached_terminal(RestateDurableWaitResolveRequest {
                    key: request.key,
                    resolution,
                });
            });
        }
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
        _namespace: &'run crate::RestateNamespace,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        _process_cancel: ProcessCancelRace,
    ) -> TestTurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run,
    {
        test_sleep_or_turn_cancel(self, &self.turn_cancel_gate, duration, turn_cancel, None)
    }

    fn run_json_send<'run, S, Fut>(
        &'run self,
        step: S,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<S::Output>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        S: crate::JournalStep,
        Fut: Future<Output = S::Output> + Send + 'run,
    {
        let effect_name = crate::journal_step_name(&step);
        let cancelled = {
            let mut pending = self.cancel_after_runs.lock_recover();
            pending
                .iter()
                .position(|journal_name| *journal_name == effect_name)
                .map(|index| pending.remove(index))
                .is_some()
        };
        self.runs.lock_recover().push(effect_name);
        Box::pin(async move {
            let answer = future.await;
            if cancelled {
                return Err(TerminalError::new_with_code(409, "cancelled"));
            }
            Ok(Json(answer))
        })
    }

    fn run_json_eager_or_retry_send<'run, S, Fut>(
        &'run self,
        step: S,
        future: Fut,
    ) -> (
        impl std::future::Future<Output = ()> + Send + 'run,
        Option<u32>,
        impl std::future::Future<Output = Result<Json<S::Output>, TerminalError>> + Send + 'run,
    )
    where
        'ctx: 'run,
        S: crate::JournalStep,
        Fut: std::future::Future<Output = Result<S::Output, String>> + Send + 'run,
    {
        let context = Arc::clone(self);
        (
            std::future::ready(()),
            Some(self.select_keys.fetch_add(1, Ordering::SeqCst) as u32),
            async move { context.run_json_or_retry_send(step, future).await },
        )
    }

    run_json_or_retry_send_ends_the_attempt!();

    fn select_run_sources<'run>(&'run self, keys: Vec<u32>) -> crate::JournaledFuture<'run, usize>
    where
        'ctx: 'run,
    {
        select_first_offered(keys)
    }

    fn start_process_workflow<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        process_id: lash_core::ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        _sender_generation: lash_core::engine::BuildGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProcessWorkflowStartFailure>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.process_command_log
            .lock_recover()
            .push(format!("send:{process_id}"));
        self.started.lock_recover().push(registration);
        self.started_execution_contexts
            .lock_recover()
            .push(execution_context);
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
            if self
                .cancel_process_workflow_starts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ProcessWorkflowStartFailure::of_send(
                    TerminalError::new_with_code(409, "cancelled"),
                ));
            }
            let started_elsewhere = self.start_elsewhere_then_refuse.lock_recover().take();
            if let Some(registry) = started_elsewhere {
                registry
                    .set_external_ref(
                        &process_id,
                        lash_core::ProcessExternalRef {
                            backend: "restate".to_string(),
                            id: format!("elsewhere/{process_id}"),
                            metadata: None,
                            segment_ordinal: Some(0),
                        },
                    )
                    .await
                    .map_err(|error| {
                        ProcessWorkflowStartFailure::Ambiguous(TerminalError::new(
                            error.to_string(),
                        ))
                    })?;
                return Err(ProcessWorkflowStartFailure::Rejected(TerminalError::new(
                    "injected refusal of a start another delivery already started",
                )));
            }
            Ok(format!("invocation-{process_id}"))
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.cancelled.lock_recover().push(request);
        Box::pin(std::future::ready(Ok(())))
    }

    fn await_event<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.awaited_replay_keys.lock_recover().push(replay_key);
        self.awaited_requests.lock_recover().push(request.clone());
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
                    .entry(SessionId::fixture(session_id.to_string()))
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
        })
    }

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
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

    fn resolve_event<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.session_revocation_checks
            .fetch_add(1, Ordering::SeqCst);
        if self.cancel_revocation_read.swap(false, Ordering::SeqCst) {
            return Box::pin(async { Err(TerminalError::new_with_code(409, "cancelled")) });
        }
        let revoked = self.revoked_sessions.lock_recover().contains(&session_id);
        Box::pin(async move { Ok(revoked) })
    }

    fn scope_effect_begin<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
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
    /// The attempt this context runs, which a step's retried fault ends.
    pub(super) attempt: AttemptEnd,
    pub(super) sleeps: Mutex<Vec<u64>>,
    pub(super) park_sleeps: AtomicBool,
    pub(super) sleep_started: tokio::sync::Notify,
    pub(super) sleep_release: tokio::sync::Notify,
    /// A fragment of the name of the run step the worker crashes right
    /// after committing, once.
    pub(super) crash_after_run_commit: Mutex<Option<String>>,
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
    /// Source select keys this context hands out: a counter stands in for
    /// the engine's notification handles.
    pub(super) select_keys: AtomicU64,
    /// The realization invocations this context's Runs issued, by
    /// invocation id: each one's request and, once it answered, its receipt
    /// (ADR 0130).
    pub(super) realizations: Mutex<HashMap<String, IssuedRealizationRecord>>,
}

/// One realization invocation a recording context stands in for: a send
/// under the same key reaches it rather than starting another.
#[derive(Clone)]
pub(super) struct IssuedRealizationRecord {
    request: lash_core::tool_dispatch::RealizationRequest,
    receipt: Option<lash_core::tool_dispatch::RealizationReceipt>,
}

impl ReplayableRecordingContext {
    /// The receipt of the realization invocation `invocation_id`: its recorded
    /// answer, or the installed worker realizing its intents in a journal of
    /// its own, as the realization service does on a deployment.
    async fn realization_receipt(
        self: Arc<Self>,
        invocation_id: String,
    ) -> Result<lash_core::tool_dispatch::RealizationReceipt, lash_core::RuntimeEffectControllerError>
    {
        let issued = self
            .realizations
            .lock_recover()
            .get(&invocation_id)
            .cloned()
            .ok_or_else(|| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::EngineEffectController,
                    format!("no realization invocation `{invocation_id}` was issued"),
                )
            })?;
        if let Some(receipt) = issued.receipt {
            return Ok(receipt);
        }
        let worker = self.process_worker.lock_recover().clone().ok_or_else(|| {
            lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::EngineControlUnsupported,
                "this context has no process worker to realize intents",
            )
        })?;
        let invocation = Arc::new(ReplayableRecordingContext {
            events: Arc::clone(&self.events),
            process_worker: Mutex::new(Some(worker.clone())),
            ..ReplayableRecordingContext::default()
        });
        let controller = RestateRuntimeEffectController::new_for_test(invocation);
        let scoped = controller
            .realization_controller(issued.request.scope.clone())
            .map_err(lash_core::RuntimeEffectControllerError::from)?;
        let receipt =
            lash_core::tool_dispatch::ToolRealizer::realize(&worker, issued.request, scoped)
                .await?;
        if let Some(issued) = self.realizations.lock_recover().get_mut(&invocation_id) {
            issued.receipt = Some(receipt.clone());
        }
        Ok(receipt)
    }

    fn realization_selectable<'run>(
        self: &Arc<Self>,
        invocation_id: String,
    ) -> lash_core::tool_dispatch::RunSelectable<'run, lash_core::tool_dispatch::RealizationReceipt>
    {
        let key = self.select_keys.fetch_add(1, Ordering::SeqCst) as u32;
        lash_core::tool_dispatch::RunSelectable {
            key: Box::pin(std::future::ready(Ok(
                lash_core::tool_dispatch::SelectKey::from_engine(key),
            ))),
            value: Box::pin(Arc::clone(self).realization_receipt(invocation_id)),
        }
    }

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

    pub(super) fn crash_after_next_run_commit_named(&self, fragment: &str) {
        *self.crash_after_run_commit.lock_recover() = Some(fragment.to_owned());
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

    /// Every recorded effect's envelope, decoded back into its command —
    /// except a model request's, which journals its request by digest
    /// (FIG-3980) and so has no command to decode back into.
    pub(super) fn recorded_runtime_effect_envelopes(&self) -> Vec<(String, RuntimeEffectEnvelope)> {
        let mut envelopes = self
            .records
            .lock_recover()
            .iter()
            .filter(|(effect_name, _)| {
                crate::controller::is_recorded_effect_journal_name(effect_name)
            })
            .filter_map(|(effect_name, bytes)| {
                let recorded = decode_recorded_runtime_effect(bytes);
                let json: serde_json::Value = serde_json::from_str(recorded.envelope.json())
                    .expect("canonical envelope json");
                if matches!(
                    json.pointer("/command/type")
                        .and_then(serde_json::Value::as_str),
                    Some("before_llm_call" | "llm_call")
                ) {
                    return None;
                }
                let envelope =
                    serde_json::from_value(json).expect("canonical runtime effect envelope");
                Some((effect_name.clone(), envelope))
            })
            .collect::<Vec<_>>();
        envelopes.sort_by(|left, right| left.0.cmp(&right.0));
        envelopes
    }

    pub(super) fn install_recorded_runtime_effects(
        &self,
        records: std::collections::BTreeMap<String, RecordedRuntimeEffect>,
    ) {
        *self.records.lock_recover() = records
            .into_iter()
            .map(|(effect_name, recorded)| {
                let envelope: serde_json::Value = serde_json::from_str(recorded.envelope.json())
                    .expect("decode recorded effect envelope");
                let entry = JournaledEffectRecord::Recorded(recorded);
                // Retried runs journal the closure's Result. Recorded runs
                // journal the stamped entry directly.
                let retried = envelope
                    .pointer("/command/type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| {
                        matches!(
                            kind,
                            "present_tool_result" | "llm_call" | "direct" | "tool_attempt"
                        )
                    });
                let bytes = if retried {
                    serde_json::to_vec(&Ok::<_, String>(entry))
                } else {
                    serde_json::to_vec(&entry)
                }
                .expect("encode installed recorded runtime effect");
                (effect_name, bytes)
            })
            .collect();
    }

    pub(super) fn recorded_process_command_facts(
        &self,
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        self.records
            .lock_recover()
            .iter()
            .filter(|(name, _)| crate::controller::is_process_command_journal_name(name))
            .map(|(name, bytes)| {
                (
                    name.clone(),
                    serde_json::from_slice(bytes).expect("recorded process command fact"),
                )
            })
            .collect()
    }

    pub(super) fn install_recorded_process_command_facts(
        &self,
        facts: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.records
            .lock_recover()
            .extend(facts.into_iter().map(|(name, value)| {
                assert!(crate::controller::is_process_command_journal_name(&name));
                (
                    name,
                    serde_json::to_vec(&value).expect("encode process command fact"),
                )
            }));
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
/// journaled fact (a cancel admission, an await or attachment's terminal
/// observation, a process wait step, a start's registration, obligation claim
/// and settle, compensation and external reference and their reruns past the
/// engine's cancellation, ADR 0107, or a command's
/// recorded store work, FIG-3827), or the frontier marker a process start or
/// a sleep journals before it acts (FIG-3779).
/// A recording context runs a source's step only when its value is awaited,
/// so the first offered source is the one it completes first.
fn select_first_offered<'run>(keys: Vec<u32>) -> crate::JournaledFuture<'run, usize> {
    Box::pin(async move {
        if keys.is_empty() {
            return Err(TerminalError::new("a Run selection names no source"));
        }
        Ok(0)
    })
}

/// Decodes one journaled record into its recorded effect. A step whose engine
/// faults are retried journals its record as `{"Ok": ...}` under these
/// contexts, and its fault never ([`run_json_or_end_attempt`]); a step whose
/// faults are recorded journals the stamped record bare.
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

impl<'ctx> RestateControllerContext<'ctx> for Arc<ReplayableRecordingContext> {
    fn invocation_id(&self) -> &str {
        "ReplayableRecordingContext"
    }

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
        _namespace: &'run crate::RestateNamespace,
        request: crate::durable_wait::process_terminal::RestateProcessTerminalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events
            .attach_process_terminal(&crate::services::DEFAULT_NAMESPACE, request)
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
        _namespace: &'run crate::RestateNamespace,
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

    fn run_json_send<'run, S, Fut>(
        &'run self,
        step: S,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<S::Output>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        S: crate::JournalStep,
        Fut: Future<Output = S::Output> + Send + 'run,
    {
        let effect_name = crate::journal_step_name(&step);
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
            let crash = {
                let mut armed = context.crash_after_run_commit.lock_recover();
                let hit = armed
                    .as_deref()
                    .is_some_and(|fragment| effect_name.contains(fragment));
                if hit {
                    *armed = None;
                }
                hit
            };
            context.records.lock_recover().insert(effect_name, bytes);
            if crash {
                context.run_committed.add_permits(1);
                panic!("injected worker crash after the post-wake effect committed");
            }
            Ok(Json(value))
        })
    }

    fn run_json_eager_or_retry_send<'run, S, Fut>(
        &'run self,
        step: S,
        future: Fut,
    ) -> (
        impl std::future::Future<Output = ()> + Send + 'run,
        Option<u32>,
        impl std::future::Future<Output = Result<Json<S::Output>, TerminalError>> + Send + 'run,
    )
    where
        'ctx: 'run,
        S: crate::JournalStep,
        Fut: std::future::Future<Output = Result<S::Output, String>> + Send + 'run,
    {
        let context = Arc::clone(self);
        (
            std::future::ready(()),
            Some(self.select_keys.fetch_add(1, Ordering::SeqCst) as u32),
            async move { context.run_json_or_retry_send(step, future).await },
        )
    }

    run_json_or_retry_send_ends_the_attempt!();

    fn select_run_sources<'run>(&'run self, keys: Vec<u32>) -> crate::JournaledFuture<'run, usize>
    where
        'ctx: 'run,
    {
        select_first_offered(keys)
    }

    fn issue_run_realization<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        request: lash_core::tool_dispatch::RealizationRequest,
    ) -> crate::JournaledFuture<
        'run,
        lash_core::tool_dispatch::IssuedRealization<'run>,
        lash_core::RuntimeEffectControllerError,
    >
    where
        'ctx: 'run,
    {
        let invocation_id = format!("realization-{}", request.key);
        self.realizations
            .lock_recover()
            .entry(invocation_id.clone())
            .or_insert(IssuedRealizationRecord {
                request,
                receipt: None,
            });
        let receipt = self.realization_selectable(invocation_id.clone());
        Box::pin(std::future::ready(Ok(
            lash_core::tool_dispatch::IssuedRealization {
                invocation_id,
                receipt,
            },
        )))
    }

    fn attach_run_realization<'run>(
        &'run self,
        invocation_id: String,
    ) -> crate::JournaledFuture<
        'run,
        lash_core::tool_dispatch::RunSelectable<'run, lash_core::tool_dispatch::RealizationReceipt>,
        lash_core::RuntimeEffectControllerError,
    >
    where
        'ctx: 'run,
    {
        Box::pin(std::future::ready(Ok(
            self.realization_selectable(invocation_id)
        )))
    }

    fn start_process_workflow<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        process_id: lash_core::ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        _sender_generation: lash_core::engine::BuildGeneration,
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
            // A started workflow has its own invocation journal. Sharing
            // the parent's journal aliases their plugin-transition records.
            let process_task_context = Arc::new(ReplayableRecordingContext {
                events: Arc::clone(&context.events),
                process_worker: Mutex::new(Some(worker.clone())),
                ..ReplayableRecordingContext::default()
            });
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
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_event(
            &crate::services::DEFAULT_NAMESPACE,
            request,
            replay_key,
            cancellation,
        )
    }

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
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

    fn resolve_event<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveEventFuture<'run>
    where
        'ctx: 'run,
    {
        self.events
            .resolve_event(&crate::services::DEFAULT_NAMESPACE, request)
    }

    fn update_session_waits<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        session_id: SessionId,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events
            .update_session_waits(&crate::services::DEFAULT_NAMESPACE, session_id, revoke)
    }

    fn session_is_revoked<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events
            .session_is_revoked(&crate::services::DEFAULT_NAMESPACE, session_id)
    }
}
