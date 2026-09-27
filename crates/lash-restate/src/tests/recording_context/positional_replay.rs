//! The positional replay context: a recording context that replays its
//! journal by position.
//!
//! Extracted from its parent purely to keep that file inside the repository's
//! test-file line budget; it is the same context, with the same visibility.

use super::*;

#[derive(Default)]
pub(crate) struct PositionalReplayContext {
    pub(crate) sleeps: Mutex<Vec<u64>>,
    pub(crate) runs: Mutex<Vec<String>>,
    pub(crate) records: Mutex<Vec<(String, Vec<u8>)>>,
    pub(crate) replaying: AtomicBool,
    replay_cursor: AtomicUsize,
    pub(crate) turn_cancel_gate: TestTurnCancelGate,
}

impl PositionalReplayContext {
    pub(crate) fn start_replay(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.replay_cursor.store(0, Ordering::SeqCst);
    }

    pub(crate) fn runs(&self) -> Vec<String> {
        self.runs.lock_recover().clone()
    }

    pub(crate) fn record_count(&self) -> usize {
        self.records.lock_recover().len()
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<PositionalReplayContext> {
    fn attach_process_terminal<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
        _process_id: lash_core::ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _sender_generation: Option<lash_core::engine::BuildGeneration>,
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
        _namespace: &'run crate::RestateNamespace,
        _request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow cancel is unsupported")) })
    }

    fn await_event<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
        _process_id: ProcessId,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(std::future::pending())
    }

    fn await_process_terminal_or_turn_cancel<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
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
        _namespace: &'run crate::RestateNamespace,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let revoked = self.turn_cancel_gate.is_revoked(&session_id);
        Box::pin(async move { Ok(revoked) })
    }
}
