//! Effect-controller test support: a strict replay journal plus controllers
//! that refuse concurrency, reject effects, or answer with the wrong outcome
//! shape. The doubles share the recording harness in the parent module.

use crate::runtime_support::*;

type StrictReplayEntry = (String, CanonicalRuntimeEffectEnvelope);
type StrictReplayTerminal = (
    CanonicalRuntimeEffectEnvelope,
    Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
);

#[derive(Clone, Default)]
pub struct StrictReplayJournal {
    pub enabled: bool,
    pub outcomes: Arc<Mutex<std::collections::BTreeMap<String, StrictReplayTerminal>>>,
}

impl StrictReplayJournal {
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    pub fn prepare(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<Option<StrictReplayEntry>, RuntimeEffectControllerError> {
        self.enabled
            .then(|| {
                Ok((
                    envelope.invocation.address.graph_key(),
                    envelope.canonical_form()?,
                ))
            })
            .transpose()
    }

    pub fn replay(
        &self,
        prepared: &Option<StrictReplayEntry>,
    ) -> Result<Option<RuntimeEffectOutcome>, RuntimeEffectControllerError> {
        let Some((key, reconstructed)) = prepared else {
            return Ok(None);
        };
        let Some((recorded, outcome)) = self.outcomes.lock_recover().get(key).cloned() else {
            return Ok(None);
        };
        validate_replayed_effect_envelope(
            &recorded,
            reconstructed,
            lash_core::RuntimeErrorCode::SqliteEffectReplayHashConflict,
            None,
        )?;
        outcome.map(Some)
    }

    pub fn record(
        &self,
        prepared: Option<StrictReplayEntry>,
        kind: lash_core::RuntimeEffectKind,
        outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    ) {
        if outcome
            .as_ref()
            .is_err_and(|error| error.journal_disposition(kind).is_retryable_derivation())
        {
            return;
        }
        if let Some((key, canonical)) = prepared {
            self.outcomes
                .lock_recover()
                .insert(key, (canonical, outcome.clone()));
        }
    }
}

#[derive(Default)]
pub struct RejectingEffectController {
    pub native: NativeRuntimeEffectController,
    pub abort_invocation_on_failure: bool,
    pub mismatch_summary: Option<RuntimeEffectReplayMismatchReport>,
}

impl RejectingEffectController {
    pub fn with_replay_mismatch(mut self) -> Self {
        self.abort_invocation_on_failure = true;
        self.mismatch_summary = Some(RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.request.model".to_string()],
        });
        self
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for RejectingEffectController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(format!("rejecting-controller:{:p}", self))
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RejectingEffectController {
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        if self.abort_invocation_on_failure {
            lash_core::EffectJournaling::Journaled
        } else {
            lash_core::EffectJournaling::Local
        }
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        _local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        if let Some(summary) = self.mismatch_summary.clone() {
            return Err(RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::SqliteEffectReplayHashConflict,
                "recorded runtime effect diverged at command.request.model",
            )
            .with_summary(summary));
        }
        Err(RuntimeEffectControllerError::foreign(
            "test_controller_rejected",
            format!("rejected {}", envelope.command.kind().as_str()),
        ))
    }

    async fn open_effect_group(
        &self,
        _group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "RejectingEffectController",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "RejectingEffectController",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        _disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "RejectingEffectController",
        ))
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.native
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

#[derive(Default)]
pub struct WrongOutcomeEffectController {
    pub native: NativeRuntimeEffectController,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for WrongOutcomeEffectController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(format!("wrong-outcome-controller:{:p}", self))
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for WrongOutcomeEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        _local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        Ok(RuntimeEffectOutcome::Sleep)
    }

    async fn open_effect_group(
        &self,
        _group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "WrongOutcomeEffectController",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "WrongOutcomeEffectController",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        _disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "WrongOutcomeEffectController",
        ))
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.native
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

#[derive(Clone, Debug)]
pub struct EffectControllerRecord {
    pub kind: RuntimeEffectKind,
    pub turn_id: Option<TurnId>,
    pub replay_key: String,
}

#[derive(Clone, Default)]
pub enum CancelWatchBehavior {
    #[default]
    Delegate,
    AlwaysError {
        attempts: Arc<std::sync::atomic::AtomicUsize>,
        exhausted: Arc<tokio::sync::Notify>,
        failures_released: Arc<std::sync::atomic::AtomicBool>,
        release_failures: Arc<tokio::sync::Notify>,
    },
}

#[derive(Clone, Default)]
pub struct RecordingEffectController {
    pub records: Arc<Mutex<Vec<EffectControllerRecord>>>,
    pub envelopes: Arc<Mutex<Vec<String>>>,
    pub llm_calls: Arc<Mutex<usize>>,
    pub native: NativeRuntimeEffectController,
    pub cancel_after_llm: bool,
    pub cancel_after_step: bool,
    pub escalate_after_llm: bool,
    pub controller_owned_replay: bool,
    pub engine_paced_lane: bool,
    pub replay_by_key: bool,
    pub strict_replay: StrictReplayJournal,
    pub execute_llm_locally: bool,
    pub execute_code_locally: bool,
    pub fail_exec_after_local: Arc<std::sync::atomic::AtomicBool>,
    pub cancel_watch: CancelWatchBehavior,
    /// Model a host crash in the window between the journaled raw provider
    /// completion (phase 1) and hook post-processing (phase 2).
    pub crash_before_first_response_hooks: bool,
    pub response_hook_crash_fired: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) replay_outcomes:
        Arc<Mutex<std::collections::BTreeMap<String, RuntimeEffectOutcome>>>,
    replay_errors: Arc<Mutex<std::collections::BTreeMap<String, RuntimeEffectControllerError>>>,
    pub direct_gate: Option<
        Arc<(
            tokio::sync::Notify,
            tokio::sync::Notify,
            std::sync::atomic::AtomicBool,
        )>,
    >,
    /// Get-or-init slot for `EffectHost::install_tool_child_host` when the
    /// recorder itself is installed as a runtime's effect host.
    pub tool_children: std::sync::OnceLock<Arc<lash_core::facade_support::ToolChildHost>>,
}

impl RecordingEffectController {
    pub fn with_cancel_after_llm(mut self) -> Self {
        self.cancel_after_llm = true;
        self
    }

    /// The journaled cancel gate holds an after-step request once the model
    /// has run; the escalation promise stays unresolved.
    pub fn with_after_step_cancel(mut self) -> Self {
        self.cancel_after_step = true;
        self
    }

    /// Alongside [`Self::with_after_step_cancel`]: the escalation promise
    /// holds an immediate abort by the time the after-LLM peek runs.
    pub fn with_escalation_after_llm(mut self) -> Self {
        self.escalate_after_llm = true;
        self
    }

    /// A replaying owner with no live cancel state: every canned gate
    /// resolution is off, so what replay sees comes from the journal alone.
    pub fn without_canned_cancel(mut self) -> Self {
        self.cancel_after_llm = false;
        self.cancel_after_step = false;
        self.escalate_after_llm = false;
        self
    }

    pub fn with_controller_owned_replay(mut self) -> Self {
        self.controller_owned_replay = true;
        self
    }

    /// Deliberately separate from controller-owned replay, so tests hold the two behaviors
    /// apart exactly as the product does.
    pub fn with_engine_paced_lane(mut self) -> Self {
        self.engine_paced_lane = true;
        self
    }

    pub fn with_replay_by_key(mut self) -> Self {
        self.replay_by_key = true;
        self
    }

    pub fn with_strict_replay_by_address(mut self) -> Self {
        self.strict_replay.enable();
        self
    }

    pub fn with_local_llm_execution(mut self) -> Self {
        self.execute_llm_locally = true;
        self
    }

    pub fn with_local_code_execution(mut self) -> Self {
        self.execute_code_locally = true;
        self
    }

    pub fn with_failing_exec_handoff_once(self) -> Self {
        self.fail_exec_after_local.store(true, Ordering::SeqCst);
        self
    }

    pub fn with_always_failing_cancel_watch(mut self) -> Self {
        self.cancel_watch = CancelWatchBehavior::AlwaysError {
            attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            exhausted: Arc::new(tokio::sync::Notify::new()),
            failures_released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            release_failures: Arc::new(tokio::sync::Notify::new()),
        };
        self
    }

    pub fn release_cancel_watch_failures(&self) {
        match &self.cancel_watch {
            CancelWatchBehavior::Delegate => {
                panic!("cancel-watch failure gate is unavailable in delegate mode")
            }
            CancelWatchBehavior::AlwaysError {
                failures_released,
                release_failures,
                ..
            } => {
                failures_released.store(true, Ordering::SeqCst);
                release_failures.notify_one();
            }
        }
    }

    pub fn cancel_watch_attempts(&self) -> usize {
        match &self.cancel_watch {
            CancelWatchBehavior::Delegate => 0,
            CancelWatchBehavior::AlwaysError { attempts, .. } => attempts.load(Ordering::SeqCst),
        }
    }

    pub(crate) async fn wait_for_cancel_watch_exhaustion(&self) {
        match &self.cancel_watch {
            CancelWatchBehavior::Delegate => {
                panic!("cancel-watch exhaustion is unavailable in delegate mode")
            }
            CancelWatchBehavior::AlwaysError {
                attempts,
                exhausted,
                ..
            } => loop {
                let notified = exhausted.notified();
                if attempts.load(Ordering::SeqCst)
                    >= lash_core::runtime::turn_loop::TURN_CANCEL_WATCH_MAX_ATTEMPTS
                {
                    return;
                }
                notified.await;
            },
        }
    }

    /// Fail the first assistant-response-hooks effect without executing it, so
    /// a test can stand where a crashed host would: phase 1 durable, phase 2
    /// never committed.
    pub fn with_crash_before_first_response_hooks(mut self) -> Self {
        self.crash_before_first_response_hooks = true;
        self
    }

    pub fn with_direct_gate(
        mut self,
        gate: Arc<(
            tokio::sync::Notify,
            tokio::sync::Notify,
            std::sync::atomic::AtomicBool,
        )>,
    ) -> Self {
        self.direct_gate = Some(gate);
        self
    }

    pub fn records(&self) -> Vec<EffectControllerRecord> {
        self.records.lock_recover().clone()
    }

    pub fn envelopes(&self) -> Vec<String> {
        self.envelopes.lock_recover().clone()
    }

    pub fn count_kind(&self, kind: RuntimeEffectKind) -> usize {
        self.records()
            .iter()
            .filter(|record| record.kind == kind)
            .count()
    }

    pub fn has_kind_for_turn(&self, kind: RuntimeEffectKind, turn_id: &TurnId) -> bool {
        self.records()
            .iter()
            .any(|record| record.kind == kind && record.turn_id.as_ref() == Some(turn_id))
    }

    pub fn record(&self, envelope: &RuntimeEffectEnvelope) {
        self.records.lock_recover().push(EffectControllerRecord {
            kind: envelope.command.kind(),
            turn_id: envelope.invocation.attribution.turn_id.clone(),
            replay_key: envelope.invocation.replay_key().to_string(),
        });
    }
}

pub fn scoped_test_turn<'a>(
    controller: &'a dyn RuntimeEffectController,
    turn_id: &TurnId,
) -> ScopedEffectController<'a> {
    ScopedEffectController::borrowed(controller, AdmittedScope::turn("root", turn_id))
        .expect("scoped effect controller")
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for RecordingEffectController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(format!(
            "recording-controller:{:p}",
            Arc::as_ptr(&self.records)
        ))
    }

    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn lash_core::QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<lash_core::QueuedLaneAcquisition, RuntimeError> {
        if self.engine_paced_lane {
            self.wait_out_crashed_lane_holder(lane, cancel).await
        } else {
            match lane.try_acquire().await? {
                lash_core::QueuedLaneAttempt::Acquired(guard) => {
                    Ok(lash_core::QueuedLaneAcquisition::Acquired(guard))
                }
                lash_core::QueuedLaneAttempt::Busy(_) => {
                    Ok(lash_core::QueuedLaneAcquisition::NotAcquired)
                }
            }
        }
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        if matches!(key.wait, AwaitEventWaitIdentity::TurnCancelGate)
            && let CancelWatchBehavior::AlwaysError {
                attempts,
                exhausted,
                failures_released,
                release_failures,
            } = &self.cancel_watch
        {
            while !failures_released.load(Ordering::SeqCst) {
                let released = release_failures.notified();
                if failures_released.load(Ordering::SeqCst) {
                    break;
                }
                released.await;
            }
            let attempts = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempts == lash_core::runtime::turn_loop::TURN_CANCEL_WATCH_MAX_ATTEMPTS {
                exhausted.notify_one();
            }
            return Err(RuntimeError::new(
                lash_core::RuntimeErrorCode::TransientCancelWatch,
                "cancel resolver remains unavailable",
            ));
        }
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RecordingEffectController {
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        if self.controller_owned_replay {
            lash_core::EffectJournaling::Journaled
        } else {
            lash_core::EffectJournaling::Local
        }
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let command_kind = envelope.command.kind();
        let strict_replay = self.strict_replay.prepare(&envelope)?;
        if let Some(outcome) = self.strict_replay.replay(&strict_replay)? {
            return Ok(outcome);
        }
        let replay_key = envelope.invocation.replay_key().to_string();
        if self.replay_by_key
            && let Some(outcome) = self
                .replay_outcomes
                .lock_recover()
                .get(&replay_key)
                .cloned()
        {
            return Ok(outcome);
        }
        if self.replay_by_key
            && let Some(error) = self.replay_errors.lock_recover().get(&replay_key).cloned()
        {
            return Err(error);
        }
        self.envelopes
            .lock_recover()
            .push(serde_json::to_string(&envelope).expect("serialize effect envelope"));
        self.record(&envelope);
        if matches!(
            envelope.command,
            RuntimeEffectCommand::AssistantResponseHooks { .. }
        ) && self.crash_before_first_response_hooks
            && !self.response_hook_crash_fired.swap(true, Ordering::SeqCst)
        {
            return Err(RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeEffectLocalTaskClosed,
                "simulated host crash between the journaled completion and hook post-processing",
            ));
        }
        let outcome = match envelope.command {
            RuntimeEffectCommand::LlmCall { request } => {
                if self.execute_llm_locally {
                    local_executor
                        .execute(RuntimeEffectEnvelope::new(
                            envelope.invocation,
                            RuntimeEffectCommand::LlmCall { request },
                        ))
                        .await
                } else {
                    let mut llm_calls = self.llm_calls.lock_recover();
                    *llm_calls += 1;
                    let first_call = *llm_calls == 1;
                    let prompt = format!("{:?}", request.messages);
                    let parts = if first_call && prompt.contains("use the tool") {
                        vec![
                            LlmOutputPart::ToolCall {
                                call_id: "call-1".to_string(),
                                tool_name: "echo_tool".to_string(),
                                input_json: serde_json::json!({"value": "hi"}).to_string(),
                                replay: None,
                            },
                            LlmOutputPart::ToolCall {
                                call_id: "call-2".to_string(),
                                tool_name: "echo_tool".to_string(),
                                input_json: serde_json::json!({"value": "there"}).to_string(),
                                replay: None,
                            },
                        ]
                    } else if first_call && prompt.contains("use direct tool") {
                        vec![LlmOutputPart::ToolCall {
                            call_id: "direct-call-1".to_string(),
                            tool_name: "direct_tool".to_string(),
                            input_json: serde_json::json!({}).to_string(),
                            replay: None,
                        }]
                    } else if first_call && prompt.contains("use retry tool") {
                        vec![LlmOutputPart::ToolCall {
                            call_id: "retry-call-1".to_string(),
                            tool_name: "retry_once".to_string(),
                            input_json: serde_json::json!({}).to_string(),
                            replay: None,
                        }]
                    } else {
                        vec![LlmOutputPart::Text {
                            text: "finished".to_string(),
                            response_meta: None,
                        }]
                    };
                    Ok(RuntimeEffectOutcome::LlmCall {
                        result: Box::new(Ok(LlmResponse {
                            parts,
                            usage: LlmUsage {
                                input_tokens: 1,
                                output_tokens: 1,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                reasoning_output_tokens: 0,
                            },
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        })),
                        text_streamed: false,
                        call_record: None,
                    })
                }
            }
            RuntimeEffectCommand::ToolAttempt {
                call,
                execution_grant,
                attempt,
                max_attempts,
            } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::ToolAttempt {
                            call,
                            execution_grant,
                            attempt,
                            max_attempts,
                        },
                    ))
                    .await
            }
            RuntimeEffectCommand::ToolBatch { batch } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::ToolBatch { batch },
                    ))
                    .await
            }
            RuntimeEffectCommand::AssistantResponseHooks { response } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::AssistantResponseHooks { response },
                    ))
                    .await
            }
            RuntimeEffectCommand::Process { command } => {
                let result = local_executor.into_process()?.execute(*command).await?;
                Ok(RuntimeEffectOutcome::Process { result })
            }
            RuntimeEffectCommand::Trigger { command } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::Trigger { command },
                    ))
                    .await
            }
            RuntimeEffectCommand::Checkpoint { .. } => Ok(RuntimeEffectOutcome::Checkpoint {
                result: Ok(lash_core::CheckpointDelivery::default()),
                claims: Box::default(),
            }),
            RuntimeEffectCommand::SyncExecutionEnvironment { .. } => {
                Ok(RuntimeEffectOutcome::SyncExecutionEnvironment { result: Ok(None) })
            }
            command @ RuntimeEffectCommand::AcceptTurnInput { .. } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(envelope.invocation, command))
                    .await
            }
            RuntimeEffectCommand::ExecCode { language, code } if self.execute_code_locally => {
                let outcome = local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::ExecCode { language, code },
                    ))
                    .await;
                if self.fail_exec_after_local.swap(false, Ordering::SeqCst) {
                    return Err(RuntimeEffectControllerError::foreign(
                        "injected_exec_handoff_failure",
                        "injected code-effect response handoff failure",
                    ));
                }
                outcome
            }
            RuntimeEffectCommand::ExecCode { .. } => Ok(RuntimeEffectOutcome::ExecCode {
                result: Box::new(Ok(lash_core::ExecResponse {
                    observations: Vec::new(),
                    calls: Vec::new(),
                    printed_images: Vec::new(),
                    error: None,
                    duration_ms: 0,
                    degraded_bindings: Vec::new(),
                    terminal_finish: Some(serde_json::json!("ok")),
                })),
            }),
            // Delegated, exactly like every other command this double records
            // and runs locally. The handler-level driver (FIG-2266) arrives as
            // the local executor the host's registered resolver handed out, so
            // this double no longer has to refuse a tool child for want of one —
            // and it still synthesizes nothing, which is what made the earlier
            // refusal right.
            command @ RuntimeEffectCommand::ToolInvocation { .. } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(envelope.invocation, command))
                    .await
            }
            command @ RuntimeEffectCommand::IncorporateGroupSettlements { .. } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(envelope.invocation, command))
                    .await
            }
            // The recorded presentation boundary (FIG-3420): delegated like
            // every other command this double journals — the local executor
            // runs the step chain once and the record above is what replay
            // serves.
            command @ RuntimeEffectCommand::PresentToolResult { .. } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(envelope.invocation, command))
                    .await
            }
            RuntimeEffectCommand::Sleep { .. } => Ok(RuntimeEffectOutcome::Sleep),
            RuntimeEffectCommand::AwaitEvent { .. } => Ok(RuntimeEffectOutcome::AwaitEvent {
                resolution: lash_core::Resolution::Ok(serde_json::json!(null)),
            }),
            RuntimeEffectCommand::PeekAwaitEvent { key }
                if self.cancel_after_step && *self.llm_calls.lock_recover() > 0 =>
            {
                let resolution = match key.wait {
                    AwaitEventWaitIdentity::TurnCancelGate => {
                        Some(Resolution::Ok(serde_json::json!({
                            "state": "cancel_requested",
                            "cancellation": {
                                "request_id": "stop-after-step",
                                "origin": "effect-controller-test",
                                "reason": "stop after the current step",
                                "mode": "after_step"
                            }
                        })))
                    }
                    AwaitEventWaitIdentity::TurnCancelEscalation if self.escalate_after_llm => {
                        Some(Resolution::Ok(serde_json::json!({
                            "state": "cancel_requested",
                            "cancellation": {
                                "request_id": "abort-escalated",
                                "origin": "effect-controller-test",
                                "reason": "escalated to an immediate abort"
                            }
                        })))
                    }
                    _ => None,
                };
                if let Some(resolution) = resolution {
                    self.native.resolve_await_event(&key, resolution).await?;
                }
                Ok(RuntimeEffectOutcome::PeekAwaitEvent {
                    resolution: self.native.peek_await_event(&key).await?,
                })
            }
            RuntimeEffectCommand::PeekAwaitEvent { key }
                if self.cancel_after_llm && *self.llm_calls.lock_recover() > 0 =>
            {
                self.native
                    .resolve_await_event(
                        &key,
                        Resolution::Ok(serde_json::json!({
                            "state": "cancel_requested",
                            "cancellation": {
                                "request_id": "cancel-after-llm",
                                "origin": "effect-controller-test",
                                "reason": "cancel landed during the journaled LLM run"
                            }
                        })),
                    )
                    .await?;
                Ok(RuntimeEffectOutcome::PeekAwaitEvent {
                    resolution: self.native.peek_await_event(&key).await?,
                })
            }
            RuntimeEffectCommand::PeekAwaitEvent { key }
                if matches!(self.cancel_watch, CancelWatchBehavior::AlwaysError { .. }) =>
            {
                Ok(RuntimeEffectOutcome::PeekAwaitEvent {
                    resolution: self.native.peek_await_event(&key).await?,
                })
            }
            RuntimeEffectCommand::PeekAwaitEvent { .. } => {
                Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None })
            }
            RuntimeEffectCommand::LanguageRuntimeValue { operation } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::LanguageRuntimeValue { operation },
                    ))
                    .await
            }
            RuntimeEffectCommand::Direct { request, .. } => {
                if let Some(gate) = &self.direct_gate
                    && gate.2.swap(false, Ordering::SeqCst)
                {
                    gate.0.notify_one();
                    gate.1.notified().await;
                }
                let prompt = format!("{:?}", request.messages);
                let is_full = prompt.contains("raw prompt") || !request.attachments().is_empty();
                let (text, usage) = if is_full {
                    (
                        "raw direct answer",
                        LlmUsage {
                            input_tokens: 4,
                            output_tokens: 6,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_output_tokens: 1,
                        },
                    )
                } else {
                    (
                        "direct answer",
                        LlmUsage {
                            input_tokens: 7,
                            output_tokens: 5,
                            cache_read_input_tokens: 1,
                            cache_write_input_tokens: 0,
                            reasoning_output_tokens: 2,
                        },
                    )
                };
                Ok(RuntimeEffectOutcome::Direct {
                    result: Box::new(Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: text.to_string(),
                            response_meta: None,
                        }],
                        usage,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })),
                    call_record: Some(lash_core::LlmCallRecord {
                        call_id: lash_core::LlmCallId("direct-effect-test".to_string()),
                        label: None,
                        replay_drops: Vec::new(),
                        attempts: Vec::new(),
                    }),
                })
            }
        };
        if self.replay_by_key
            && let Ok(outcome) = &outcome
        {
            self.replay_outcomes
                .lock_recover()
                .insert(replay_key.clone(), outcome.clone());
        }
        if self.replay_by_key
            && let Err(error) = &outcome
            && !error
                .journal_disposition(command_kind)
                .is_retryable_derivation()
        {
            self.replay_errors
                .lock_recover()
                .insert(replay_key, error.clone());
        }
        self.strict_replay
            .record(strict_replay, command_kind, &outcome);
        outcome
    }

    // A tool batch is a durable effect group now (FIG-3397), so the recorder
    // hosts groups on its embedded native substrate: opens and settlements
    // forward there, and the tool-child resolver registered through
    // `register_group_executors` lands on the same group map.
    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.native.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.register_group_executors(executors)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.native.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.close_effect_group(handle, disposition).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.native
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}
