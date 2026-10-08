//! Completion facts retained outside the spawned provider task.

use super::*;

#[derive(Debug)]
struct ProviderCompletionSidebandState {
    serving_route: ProviderRouteIdentity,
    replay_drops: Vec<crate::ProviderReplayDrop>,
    origin_conflict: Option<ProviderReplayOriginConflict>,
    attempts: Vec<AttemptRecord>,
    /// The dispatched attempt that has not been sealed yet, when an observer
    /// is installed.
    open_attempt: Option<lash_trace::TraceAttemptObservation>,
}

type AttemptObserver =
    Arc<dyn Fn(AttemptRecord, lash_trace::TraceAttemptObservation) + Send + Sync>;

/// Replay safety and sealed attempt facts shared with the runtime independently
/// of the spawned LLM Provider task's terminal return. This is in-process
/// retention; it does not make uncommitted attempts survive a crash.
#[derive(Clone)]
pub struct ProviderCompletionSideband {
    state: Arc<Mutex<ProviderCompletionSidebandState>>,
    attempt_observer: Option<AttemptObserver>,
    attempt_clock: Option<Arc<dyn crate::Clock>>,
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
    /// Report each dispatched provider attempt once, when it is sealed: the
    /// observer receives the same [`AttemptRecord`] the call record keeps.
    #[must_use]
    pub fn with_attempt_observer(
        mut self,
        observer: AttemptObserver,
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
                open_attempt: None,
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
        let mut attempt = failure_attempt_record(
            self.next_attempt_ordinal(),
            failure,
            retry_budget_consumed,
            protocol_position,
            None,
        );
        attempt.outcome = outcome;
        self.seal_attempt(attempt);
        self.call_record(call_id)
    }

    pub(super) fn next_attempt_ordinal(&self) -> u32 {
        self.with_state(|state| state.attempts.len() as u32 + 1)
    }

    /// Open the observation of an attempt that is about to be dispatched.
    pub(super) fn begin_attempt(&self, provider: &str, request_model: &str) {
        let Some(clock) = &self.attempt_clock else {
            return;
        };
        let observation = lash_trace::TraceAttemptObservation {
            provider: Some(provider.to_string()),
            request_model: request_model.to_string(),
            started_at_ms: Some(clock.timestamp_ms()),
            ended_at_ms: None,
        };
        self.with_state(|state| state.open_attempt = Some(observation));
    }

    /// Seal an attempt into the call record and report it to the observer.
    /// A record sealed with no dispatched attempt open (a call cut between
    /// attempts) is ledger-only: no provider request stands behind it.
    pub(super) fn seal_attempt(&self, attempt: AttemptRecord) {
        let observation = self.with_state(|state| {
            state.attempts.push(attempt.clone());
            state.open_attempt.take()
        });
        if let (Some(observer), Some(mut observation)) = (&self.attempt_observer, observation) {
            observation.ended_at_ms = self
                .attempt_clock
                .as_ref()
                .map(|clock| clock.timestamp_ms());
            observer(attempt, observation);
        }
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
