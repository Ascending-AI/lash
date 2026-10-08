//! Completion facts retained outside the spawned provider task.

use super::*;

#[derive(Debug)]
struct ProviderCompletionSidebandState {
    serving_route: ProviderRouteIdentity,
    replay_drops: Vec<crate::ProviderReplayDrop>,
    origin_conflict: Option<ProviderReplayOriginConflict>,
    attempts: Vec<AttemptRecord>,
}

/// Replay safety and sealed attempt facts shared with the runtime independently
/// of the spawned LLM Provider task's terminal return. This is in-process
/// retention; it does not make uncommitted attempts survive a crash.
#[derive(Clone)]
pub struct ProviderCompletionSideband {
    state: Arc<Mutex<ProviderCompletionSidebandState>>,
    pub(super) attempt_observer: Option<Arc<dyn Fn(lash_trace::TraceLlmAttempt) + Send + Sync>>,
    pub(super) attempt_clock: Option<Arc<dyn crate::Clock>>,
}

impl std::fmt::Debug for ProviderCompletionSideband {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCompletionSideband")
            .field("state", &self.state)
            .field("observed", &self.attempt_observer.is_some())
            .finish()
    }
}

impl ProviderCompletionSideband {
    #[must_use]
    pub fn with_attempt_observer(
        mut self,
        observer: Arc<dyn Fn(lash_trace::TraceLlmAttempt) + Send + Sync>,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        self.attempt_observer = Some(observer);
        self.attempt_clock = Some(clock);
        self
    }

    pub(super) fn new(
        serving_route: ProviderRouteIdentity,
        replay_drops: Vec<crate::ProviderReplayDrop>,
    ) -> Self {
        Self {
            attempt_observer: None,
            attempt_clock: None,
            state: Arc::new(Mutex::new(ProviderCompletionSidebandState {
                serving_route,
                replay_drops,
                origin_conflict: None,
                attempts: Vec::new(),
            })),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ProviderCompletionSidebandState) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut state)
    }

    pub(super) fn record_origin_conflict(&self, conflict: ProviderReplayOriginConflict) {
        self.with_state(|state| {
            if state.origin_conflict.is_none() {
                state.origin_conflict = Some(conflict);
            }
        });
    }

    /// Seal a terminal interruption after the provider task has stopped.
    pub fn terminal_call_record(
        &self,
        call_id: LlmCallId,
        outcome: AttemptOutcome,
        failure: &LlmTransportError,
        retry_budget_consumed: bool,
        protocol_position: ProtocolPosition,
    ) -> LlmCallRecord {
        self.with_state(|state| {
            let mut attempt = failure_attempt_record(
                state.attempts.len() as u32 + 1,
                failure,
                retry_budget_consumed,
                protocol_position,
                None,
            );
            attempt.outcome = outcome;
            state.attempts.push(attempt);
        });
        self.call_record(call_id)
    }

    pub(super) fn next_attempt_ordinal(&self) -> u32 {
        self.with_state(|state| state.attempts.len() as u32 + 1)
    }

    pub(super) fn seal_attempt(&self, attempt: AttemptRecord) {
        self.with_state(|state| state.attempts.push(attempt));
    }

    pub(super) fn call_record(&self, call_id: LlmCallId) -> LlmCallRecord {
        self.with_state(|state| LlmCallRecord {
            call_id,
            label: None,
            replay_drops: state.replay_drops.clone(),
            attempts: state.attempts.clone(),
        })
    }

    pub fn replay_drops(&self) -> Vec<crate::ProviderReplayDrop> {
        self.with_state(|state| state.replay_drops.clone())
    }

    pub(super) fn serving_route(&self) -> ProviderRouteIdentity {
        self.with_state(|state| state.serving_route.clone())
    }

    pub fn origin_conflict(&self) -> Option<ProviderReplayOriginConflict> {
        self.with_state(|state| state.origin_conflict.clone())
    }

    pub fn fence_response(&self, response: &mut LlmResponse) -> Result<(), LlmTransportError> {
        let serving_route = self.serving_route();
        if let Err(conflict) = response.stamp_replay_origin(&serving_route) {
            self.record_origin_conflict(conflict);
        }
        match self.origin_conflict() {
            Some(conflict) => Err(replay_origin_conflict_error(conflict)),
            None => Ok(()),
        }
    }

    pub(super) fn fence_error(&self, mut error: LlmTransportError) -> LlmTransportError {
        let serving_route = self.serving_route();
        if let Some(partial) = error.partial_response.as_deref_mut()
            && let Err(conflict) = partial.stamp_replay_origin(&serving_route)
        {
            self.record_origin_conflict(conflict);
        }
        match self.origin_conflict() {
            Some(conflict) => replay_origin_conflict_with_provider_error(conflict, error),
            None => error,
        }
    }
}
