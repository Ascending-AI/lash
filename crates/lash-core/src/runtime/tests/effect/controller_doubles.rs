//! The negative effect-controller doubles: a controller that refuses
//! concurrency, one that rejects every effect, and one that answers with the
//! wrong outcome shape. They exist to prove the turn loop fails explicitly
//! rather than silently, and they share the recording harness in the parent
//! module.

use super::*;

#[derive(Clone, Default)]
pub(super) struct SerialOnlyEffectController {
    inner: RecordingEffectController,
    in_flight_tool_attempts: Arc<std::sync::atomic::AtomicUsize>,
    max_in_flight_tool_attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl SerialOnlyEffectController {
    pub(super) fn count_kind(&self, kind: RuntimeEffectKind) -> usize {
        self.inner.count_kind(kind)
    }

    pub(super) fn max_in_flight_tool_attempts(&self) -> usize {
        self.max_in_flight_tool_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for SerialOnlyEffectController {
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for SerialOnlyEffectController {
    fn supports_concurrent_effects(&self) -> bool {
        false
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let is_tool_attempt = envelope.command.kind() == RuntimeEffectKind::ToolAttempt;
        if is_tool_attempt {
            let current = self
                .in_flight_tool_attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let mut observed = self.max_in_flight_tool_attempts();
            while current > observed {
                match self.max_in_flight_tool_attempts.compare_exchange(
                    observed,
                    current,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let outcome = self.inner.execute_effect(envelope, local_executor).await;

        if is_tool_attempt {
            self.in_flight_tool_attempts
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }

        outcome
    }
}

#[derive(Default)]
pub(in crate::runtime::tests) struct RejectingEffectController {
    inline: InlineRuntimeEffectController,
    abort_invocation_on_failure: bool,
    mismatch_summary: Option<RuntimeEffectReplayMismatchReport>,
}

impl RejectingEffectController {
    pub(in crate::runtime::tests) fn with_replay_mismatch(mut self) -> Self {
        self.abort_invocation_on_failure = true;
        self.mismatch_summary = Some(RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.request.model".to_string()],
        });
        self
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for RejectingEffectController {
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inline.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inline.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inline.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inline.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RejectingEffectController {
    async fn runtime_effect_failure_disposition(
        &self,
        _code: crate::RuntimeErrorCode,
    ) -> Result<crate::RuntimeEffectFailureDisposition, RuntimeError> {
        Ok(if self.abort_invocation_on_failure {
            crate::RuntimeEffectFailureDisposition::AbortInvocation
        } else {
            crate::RuntimeEffectFailureDisposition::RecordTurnFailure
        })
    }

    async fn turn_control_participation(
        &self,
    ) -> Result<crate::TurnControlParticipation, RuntimeError> {
        Ok(if self.abort_invocation_on_failure {
            crate::TurnControlParticipation::DurableJournaled
        } else {
            crate::TurnControlParticipation::Local
        })
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        if let Some(summary) = self.mismatch_summary.clone() {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::SqliteEffectReplayHashConflict,
                "recorded runtime effect diverged at command.request.model",
            )
            .with_summary(summary));
        }
        Err(RuntimeEffectControllerError::foreign(
            "test_controller_rejected",
            format!("rejected {}", envelope.command.kind().as_str()),
        ))
    }
}

#[derive(Default)]
pub(super) struct WrongOutcomeEffectController {
    inline: InlineRuntimeEffectController,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for WrongOutcomeEffectController {
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inline.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inline.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inline.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inline.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for WrongOutcomeEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        Ok(RuntimeEffectOutcome::Sleep)
    }
}
