//! Effect-layer test support: a strict replay journal plus layers that
//! record, reject effects, or answer with the wrong outcome shape. Each double
//! is an [`EffectLayer`](lash_core::testing::EffectLayer) over a real backend's
//! effect host (FIG-3580): it answers the effects it models itself and leaves
//! every group operation, await-event registry read and journal lever to the
//! backend underneath.

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
    pub abort_invocation_on_failure: bool,
    pub mismatch_summary: Option<RuntimeEffectReplayMismatchReport>,
}

impl RejectingEffectController {
    pub fn with_replay_mismatch(mut self) -> Self {
        self.abort_invocation_on_failure = true;
        self.mismatch_summary = Some(RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.request.model".to_string()],
            effect_kind: None,
        });
        self
    }
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for RejectingEffectController {
    async fn execute_effect(
        &self,
        _inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        // The root's recorded session config is the funnel's, not the turn's:
        // this double judges the turn's own effects.
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::ResolveTurnConfig { .. }
        ) {
            return local_executor.execute(envelope).await;
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
            lash_core::TurnFailureCause::Outcome,
            format!("rejected {}", envelope.command.kind().as_str()),
        ))
    }

    async fn open_effect_group(
        &self,
        _inner: &dyn RuntimeEffectController,
        _group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "RejectingEffectController",
        ))
    }
}

#[derive(Default)]
pub struct WrongOutcomeEffectController;

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for WrongOutcomeEffectController {
    async fn execute_effect(
        &self,
        _inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        // The root's recorded session config is the funnel's, not the turn's:
        // this double judges the turn's own effects.
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::ResolveTurnConfig { .. }
        ) {
            return local_executor.execute(envelope).await;
        }
        Ok(RuntimeEffectOutcome::Sleep)
    }

    async fn open_effect_group(
        &self,
        _inner: &dyn RuntimeEffectController,
        _group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "WrongOutcomeEffectController",
        ))
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
    /// The first `remaining` watches of the gate fail, then every watch
    /// delegates: a transient fault the watch's retry ladder rides out.
    FailFirst {
        remaining: Arc<std::sync::atomic::AtomicUsize>,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    },
}

#[derive(Clone, Default)]
pub struct RecordingEffectController {
    pub records: Arc<Mutex<Vec<EffectControllerRecord>>>,
    pub envelopes: Arc<Mutex<Vec<String>>>,
    pub llm_calls: Arc<Mutex<usize>>,
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

    pub fn with_transient_cancel_watch_failures(mut self, failures: usize) -> Self {
        self.cancel_watch = CancelWatchBehavior::FailFirst {
            remaining: Arc::new(std::sync::atomic::AtomicUsize::new(failures)),
            attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        self
    }

    pub fn release_cancel_watch_failures(&self) {
        match &self.cancel_watch {
            CancelWatchBehavior::Delegate | CancelWatchBehavior::FailFirst { .. } => {
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
            CancelWatchBehavior::AlwaysError { attempts, .. }
            | CancelWatchBehavior::FailFirst { attempts, .. } => attempts.load(Ordering::SeqCst),
        }
    }

    pub(crate) async fn wait_for_cancel_watch_failure(&self) {
        match &self.cancel_watch {
            CancelWatchBehavior::Delegate | CancelWatchBehavior::FailFirst { .. } => {
                panic!("cancel-watch exhaustion is only available in always-error mode")
            }
            CancelWatchBehavior::AlwaysError {
                attempts,
                exhausted,
                ..
            } => loop {
                let notified = exhausted.notified();
                if attempts.load(Ordering::SeqCst) >= 1 {
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

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for RecordingEffectController {
    async fn acquire_queued_lane(
        &self,
        inner: &dyn lash_core::AwaitEventResolver,
        lane: Arc<dyn lash_core::QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<lash_core::QueuedLaneAcquisition, RuntimeError> {
        if self.engine_paced_lane {
            inner.wait_out_crashed_lane_holder(lane, cancel).await
        } else {
            inner.acquire_queued_lane(lane, cancel).await
        }
    }

    async fn await_await_event(
        &self,
        inner: &dyn lash_core::AwaitEventResolver,
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
            attempts.fetch_add(1, Ordering::SeqCst);
            exhausted.notify_one();
            return Err(RuntimeError::new(
                lash_core::RuntimeErrorCode::TransientCancelWatch,
                "cancel resolver remains unavailable",
            ));
        }
        if matches!(key.wait, AwaitEventWaitIdentity::TurnCancelGate)
            && let CancelWatchBehavior::FailFirst {
                remaining,
                attempts,
            } = &self.cancel_watch
            && remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        {
            attempts.fetch_add(1, Ordering::SeqCst);
            return Err(RuntimeError::new(
                lash_core::RuntimeErrorCode::TransientCancelWatch,
                "cancel resolver briefly unavailable",
            ));
        }
        inner.await_await_event(key, cancel, deadline).await
    }

    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
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
            RuntimeEffectCommand::LlmCall {
                provider_id,
                request,
            } => {
                if self.execute_llm_locally {
                    local_executor
                        .execute(RuntimeEffectEnvelope::new(
                            envelope.invocation,
                            RuntimeEffectCommand::LlmCall {
                                provider_id,
                                request,
                            },
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
                        stream: Box::default(),
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
            RuntimeEffectCommand::AssistantResponseHooks {
                response,
                stream_hook_states,
            } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::AssistantResponseHooks {
                            response,
                            stream_hook_states,
                        },
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
            // The sync is the only way a turn machine gets its environment
            // and its tool surface, so the double runs it as the host does.
            command @ (RuntimeEffectCommand::SyncExecutionEnvironment
            | RuntimeEffectCommand::AcceptTurnInput { .. }
            | RuntimeEffectCommand::ClaimAcceptedTurnInput { .. }
            | RuntimeEffectCommand::AdmitDrive { .. }
            | RuntimeEffectCommand::SealDriveAdmission { .. }
            | RuntimeEffectCommand::ResolveTurnConfig { .. }) => {
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
                        lash_core::TurnFailureCause::LiveFault,
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
            command @ (RuntimeEffectCommand::PresentToolResult { .. }
            | RuntimeEffectCommand::LoadExecutionEnv { .. }) => {
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
                    inner.resolve_await_event(&key, resolution).await?;
                }
                Ok(RuntimeEffectOutcome::PeekAwaitEvent {
                    resolution: inner.peek_await_event(&key).await?,
                })
            }
            RuntimeEffectCommand::PeekAwaitEvent { key }
                if self.cancel_after_llm && *self.llm_calls.lock_recover() > 0 =>
            {
                inner
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
                    resolution: inner.peek_await_event(&key).await?,
                })
            }
            // A peek reads the real gate: since FIG-3672 P9 the turn learns a
            // cancellation only from its peeks and its steps' outcomes.
            RuntimeEffectCommand::PeekAwaitEvent { key } => {
                Ok(RuntimeEffectOutcome::PeekAwaitEvent {
                    resolution: inner.peek_await_event(&key).await?,
                })
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
}
