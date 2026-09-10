#[cfg(test)]
use super::logical_turn::agent_frame_follow_turn_id;
use super::logical_turn::{
    LogicalTurnClaims, LogicalTurnStart, PhysicalTurnExecution, PreparedLogicalTurn,
};
use super::turn_control::ActiveTurnControl;
use super::*;
use crate::facade_support::{
    ProtocolTurnOptionsFacadeOps, RuntimeSessionStateFacadeOps, ScopedEffectControllerFacadeOps,
};
use lash_sansio::core_support::*;
use std::pin::Pin;

mod accept;
mod commit;
mod execute;
mod lease;
mod post_commit;
mod prepare;
mod queued_work;
mod resident_session;

use commit::TurnFinishInput;
#[cfg(test)]
pub(in crate::runtime) use execute::TURN_CANCEL_WATCH_MAX_ATTEMPTS;
use execute::TurnDriverRemainder;
#[cfg(test)]
use execute::{
    TURN_CANCEL_START_GATE_ATTEMPTS, await_turn_cancellation_start_gate,
    await_turn_cancellation_with_retry,
};
use post_commit::PostCommitDelivery;
pub use queued_work::{
    EmptyQueuedDrainReason, QueuedTurnDrain, SelectedQueuedWorkBatchSatisfaction,
    SelectedQueuedWorkDrainError, SelectedQueuedWorkDrainOutcome,
    SelectedQueuedWorkDrainRefusalCause,
};
pub(in crate::runtime) use resident_session::ResidentSessionContinuity;
pub(crate) use resident_session::ResidentSessionState;

/// How many pending next-turn inputs one idle claim absorbs into a single turn.
///
/// Direct and drained ingress share the bound because they share the claim
/// (ADR 0069): a direct turn takes the head of the same queue a drain does.
pub(super) const MAX_CLAIMED_TURN_INPUTS: usize = 64;

/// Projects a terminal turn outcome onto the closed trace outcome.
///
/// Cancellation is its own trace variant carrying the evidence
/// [`TurnStop::Cancelled`] already holds, so a cancelled turn is never traced
/// as a failure.
fn trace_outcome(outcome: &TurnOutcome) -> lash_trace::TraceTurnOutcome {
    use lash_trace::{TraceTurnCompletionReason as Reason, TraceTurnOutcome as Outcome};
    match outcome {
        TurnOutcome::Finished(TurnFinish::AssistantMessage { .. }) => Outcome::Completed {
            done_reason: Reason::AssistantMessage,
        },
        TurnOutcome::Finished(TurnFinish::FinalValue { .. }) => Outcome::Completed {
            done_reason: Reason::FinalValue,
        },
        TurnOutcome::Finished(TurnFinish::ToolValue { .. }) => Outcome::Completed {
            done_reason: Reason::ToolValue,
        },
        TurnOutcome::AgentFrameSwitch { frame_key, .. } => Outcome::AgentFrameSwitch {
            frame_switch: lash_trace::TraceAgentFrameSwitch {
                frame_key: frame_key.as_str().to_string(),
            },
        },
        TurnOutcome::Stopped(stop) => {
            use lash_trace::TraceTurnFailureReason as Failure;
            match stop {
                TurnStop::Cancelled { evidence } => Outcome::Cancelled {
                    evidence: lash_trace::TraceTurnCancellationEvidence {
                        request_id: evidence.request_id.clone(),
                        origin: evidence.origin.clone(),
                        reason: evidence.reason.clone(),
                    },
                },
                TurnStop::Incomplete => Outcome::Failed {
                    done_reason: Failure::Incomplete,
                },
                TurnStop::InvalidInput => Outcome::Failed {
                    done_reason: Failure::InvalidInput,
                },
                TurnStop::MaxTurns => Outcome::Failed {
                    done_reason: Failure::MaxTurns,
                },
                TurnStop::ToolFailure => Outcome::Failed {
                    done_reason: Failure::ToolFailure,
                },
                TurnStop::ProviderError => Outcome::Failed {
                    done_reason: Failure::ProviderError,
                },
                TurnStop::PluginAbort => Outcome::Failed {
                    done_reason: Failure::PluginAbort,
                },
                TurnStop::RuntimeError => Outcome::Failed {
                    done_reason: Failure::RuntimeError,
                },
                TurnStop::SubmittedError { .. } => Outcome::Failed {
                    done_reason: Failure::SubmittedError,
                },
                TurnStop::ToolError { .. } => Outcome::Failed {
                    done_reason: Failure::ToolError,
                },
            }
        }
    }
}

pub(super) fn post_commit_delivery_issue(
    code: impl Into<String>,
    message: impl Into<String>,
) -> TurnIssue {
    TurnIssue {
        severity: crate::runtime::TurnIssueSeverity::Blocking,
        kind: "runtime".to_string(),
        code: Some(code.into()),
        terminal_reason: None,
        message: message.into(),
        raw: None,
        retryable: Some(false),
        provider_failure_kind: None,
    }
}

fn session_head_refresh_error(err: SessionError) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::SessionHeadRefresh, err.to_string())
}

#[derive(Clone, Copy)]
pub(super) enum SessionExecutionLeaseReleasePolicy {
    KeepOnAgentFrameSwitch,
}

impl SessionExecutionLeaseReleasePolicy {
    fn should_release(self, outcome: &TurnOutcome) -> bool {
        match self {
            Self::KeepOnAgentFrameSwitch => {
                !matches!(outcome, TurnOutcome::AgentFrameSwitch { .. })
            }
        }
    }
}

fn queued_work_payload_type(payload: &crate::QueuedWorkPayload) -> &'static str {
    match payload {
        crate::QueuedWorkPayload::ProcessWake { .. } => "process_wake",
        crate::QueuedWorkPayload::AgentFrameTask { .. } => "agent_frame_task",
        crate::QueuedWorkPayload::SessionCommand { command } => command.kind(),
    }
}

fn queued_work_batch_ids(claim: &crate::QueuedWorkClaim) -> Vec<String> {
    claim
        .batches
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect()
}

/// Measures the whole host-visible turn.
///
/// Opened before the runtime claims the turn (session-execution lease and
/// queued-work/turn-input claims) and stamped onto the assembled turn after
/// the final commit and post-persist hooks complete, so
/// [`TurnExecutionMetrics`](crate::TurnExecutionMetrics) timing covers
/// claim → final commit. Reads only the injected [`Clock`](crate::Clock):
/// `started_at_ms` comes from the wall-clock source and the duration from the
/// monotonic source, so deterministic clocks produce deterministic timing.
#[derive(Clone, Copy)]
pub(super) struct TurnStopwatch {
    started: std::time::Instant,
    started_at_ms: u64,
}

impl TurnStopwatch {
    pub(super) fn start(clock: &dyn crate::Clock) -> Self {
        Self {
            started: clock.now(),
            started_at_ms: clock.timestamp_ms(),
        }
    }

    pub(super) fn stamp(&self, turn: &mut AssembledTurn, clock: &dyn crate::Clock) {
        turn.execution.started_at_ms = self.started_at_ms;
        turn.execution.duration_ms = clock
            .now()
            .saturating_duration_since(self.started)
            .as_millis() as u64;
    }
}

fn turn_phase_id(parent_turn_id: &str, phase: &str) -> String {
    format!("{parent_turn_id}:{phase}")
}

fn scoped_child_turn_controller<'run>(
    scoped_effect_controller: &ScopedEffectController<'run>,
    session_id: &str,
    turn_id: &str,
) -> Result<ScopedEffectController<'run>, RuntimeError> {
    let scope = ExecutionScope::turn(session_id, turn_id);
    scoped_effect_controller.rescope(scope)
}

async fn turn_control_binding<'a>(
    effect_host: &'a dyn EffectHost,
    scoped_effect_controller: &'a ScopedEffectController<'_>,
) -> Result<crate::TurnControlBinding<'a>, RuntimeError> {
    effect_host
        .turn_control_binding(scoped_effect_controller)
        .await
}

pub(in crate::runtime) fn queued_work_trace_payload(
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: &[crate::TurnCause],
) -> serde_json::Value {
    serde_json::json!({
        "boundary": boundary,
        "claim_id": claim.claim_id,
        "owner_id": claim.owner.owner_id,
        "incarnation_id": claim.owner.incarnation_id,
        "batch_ids": queued_work_batch_ids(claim),
        "payload_types": claim.batches.iter()
            .flat_map(|batch| batch.items.iter())
            .map(|item| queued_work_payload_type(&item.payload))
            .collect::<Vec<_>>(),
        "causes": causes,
    })
}

pub(in crate::runtime) fn queued_work_completion_trace_payload(
    completions: &[crate::QueuedWorkCompletion],
) -> serde_json::Value {
    serde_json::json!({
        "claims": completions.iter().map(|completion| {
            serde_json::json!({
                "session_id": completion.session_id,
                "claim_id": completion.claim_id,
                "batch_ids": completion.batch_ids,
            })
        }).collect::<Vec<_>>(),
    })
}

pub(in crate::runtime) fn turn_input_completion_trace_payload(
    completions: &[crate::TurnInputCompletion],
) -> serde_json::Value {
    serde_json::json!({
        "claims": completions.iter().map(|completion| {
            serde_json::json!({
                "session_id": completion.session_id,
                "claim_id": completion.claim_id(),
                "input_ids": completion.input_ids,
            })
        }).collect::<Vec<_>>(),
    })
}

pub(in crate::runtime) async fn emit_turn_started_to_sink(
    events: &dyn TurnActivitySink,
    turn_id: &str,
) {
    emit_turn_activity_to_sink_for_turn(
        events,
        turn_id,
        TurnActivity::independent(TurnEvent::TurnStarted {
            turn_id: turn_id.to_string(),
        }),
    )
    .await;
}

pub(in crate::runtime) async fn emit_queued_work_started_to_sink(
    events: &dyn TurnActivitySink,
    turn_id: &str,
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: Vec<crate::TurnCause>,
) {
    emit_turn_activity_to_sink_for_turn(
        events,
        turn_id,
        TurnActivity::independent(TurnEvent::QueuedWorkStarted {
            boundary,
            batch_ids: queued_work_batch_ids(claim),
            causes,
        }),
    )
    .await;
}

pub(in crate::runtime) async fn send_queued_work_started_event(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: Vec<crate::TurnCause>,
) {
    send_turn_activity(
        event_tx,
        TurnActivityId::new(uuid::Uuid::new_v4().to_string()),
        TurnEvent::QueuedWorkStarted {
            boundary,
            batch_ids: queued_work_batch_ids(claim),
            causes,
        },
    )
    .await;
}

trait TypedTurnPhase {
    const RUNTIME_PHASE: RuntimeTurnPhase;
}

impl LashRuntime {
    pub(super) fn max_context_tokens(&self) -> usize {
        self.state.effective_policy().context_window_tokens()
    }

    #[doc(hidden)]
    pub fn set_turn_phase_probe(&mut self, probe: Arc<dyn RuntimeTurnPhaseProbe>) {
        self.turn_phase_probe = Some(probe);
    }

    #[doc(hidden)]
    pub fn set_turn_phase_probe_if_changed(
        &mut self,
        probe: Arc<dyn RuntimeTurnPhaseProbe>,
    ) -> bool {
        let changed = self
            .turn_phase_probe
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &probe));
        self.set_turn_phase_probe(probe);
        changed
    }

    fn mark_phase_begin(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.begin(phase);
        }
    }

    fn mark_phase_end(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.end(phase);
        }
    }
}

pub fn ensure_durable_effect_input(input: &TurnInput) -> Result<(), RuntimeError> {
    if input.protocol_extension.is_some() {
        return Err(RuntimeError::new(
            RuntimeErrorCode::DurableEffectLiveProtocolExtension,
            "durable effect hosts do not support live protocol_extension inputs; encode replayable data in protocol_turn_options or persisted plugin state",
        ));
    }
    input
        .turn_context
        .live_plugin_inputs()
        .durable_effect_rejection()?;
    Ok(())
}

async fn emit_turn_activity_to_sink(events: &dyn TurnActivitySink, activity: TurnActivity) {
    if !events.is_noop() {
        events.emit(activity).await;
    }
}

async fn emit_turn_activity_to_sink_for_turn(
    events: &dyn TurnActivitySink,
    turn_id: &str,
    activity: TurnActivity,
) {
    if !events.is_noop() {
        events.emit_for_turn(turn_id, activity).await;
    }
}

/// Kind tag carried by a terminal diagnostic's error envelope.
#[derive(Clone, Copy)]
enum TerminalDiagnosticKind {
    /// The runtime itself refused to continue the turn.
    Runtime,
    /// Turn input failed normalization before any provider work.
    InputValidation,
    /// A plugin aborted the prepared turn.
    Plugin,
}

impl TerminalDiagnosticKind {
    fn as_envelope_kind(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::InputValidation => "input_validation",
            Self::Plugin => "plugin",
        }
    }
}

/// How a terminal diagnostic's turn activity is addressed to its sink.
enum TerminalActivityTarget<'a> {
    /// The sink is already turn-scoped, so the activity is emitted directly.
    TurnScopedSink(&'a dyn TurnActivitySink),
    /// The sink is unscoped, so the activity is addressed to `turn_id`.
    UnscopedSink {
        sink: &'a dyn TurnActivitySink,
        turn_id: &'a str,
    },
}

/// Typed diagnostic emitted immediately ahead of a terminal `TurnOutcome`.
struct TerminalDiagnostic<'a> {
    kind: TerminalDiagnosticKind,
    code: Option<String>,
    message: String,
    retryable: Option<bool>,
    activity: TerminalActivityTarget<'a>,
}

/// Emit the canonical terminal sequence for a stopped turn.
///
/// The order is fixed and load-bearing for host transcripts: the optional
/// diagnostic's session `Error` event (with its turn activity emitted in
/// between), then `TurnOutcome::Stopped(stop)`, then `Done`. Every session
/// event is recorded on `assembler` in emission order so the assembled turn
/// matches what the host streamed.
async fn emit_terminal_sequence(
    assembler: &mut TurnAssembler,
    events: &dyn EventSink,
    diagnostic: Option<TerminalDiagnostic<'_>>,
    stop: TurnStop,
) {
    if let Some(diagnostic) = diagnostic {
        let error_event = SessionStreamEvent::Error {
            message: diagnostic.message.clone(),
            envelope: Some(crate::session_model::ErrorEnvelope {
                kind: diagnostic.kind.as_envelope_kind().to_string(),
                code: diagnostic.code,
                terminal_reason: None,
                user_message: diagnostic.message.clone(),
                raw: None,
                retryable: diagnostic.retryable,
                provider_failure_kind: None,
            }),
        };
        assembler.push(&error_event);
        let activity = TurnActivity::independent(TurnEvent::Error {
            message: diagnostic.message,
        });
        match diagnostic.activity {
            TerminalActivityTarget::TurnScopedSink(sink) => {
                emit_turn_activity_to_sink(sink, activity).await;
            }
            TerminalActivityTarget::UnscopedSink { sink, turn_id } => {
                emit_turn_activity_to_sink_for_turn(sink, turn_id, activity).await;
            }
        }
        emit_session_event_to_sink(events, error_event).await;
    }
    let outcome_event = SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Stopped(stop),
    };
    assembler.push(&outcome_event);
    emit_session_event_to_sink(events, outcome_event).await;
    assembler.push(&SessionStreamEvent::Done);
    emit_session_event_to_sink(events, SessionStreamEvent::Done).await;
}

struct TurnScopedActivitySink<'a> {
    turn_id: String,
    inner: &'a dyn TurnActivitySink,
}

#[async_trait::async_trait]
impl TurnActivitySink for TurnScopedActivitySink<'_> {
    fn is_noop(&self) -> bool {
        self.inner.is_noop()
    }

    async fn emit(&self, activity: TurnActivity) {
        self.inner.emit_for_turn(&self.turn_id, activity).await;
    }
}

async fn emit_parent_end_events(
    parent_end_events: Vec<SessionStreamEvent>,
    assembler: &mut TurnAssembler,
    events: &dyn EventSink,
) {
    for event in parent_end_events {
        assembler.push(&event);
        emit_session_event_to_sink(events, event).await;
    }
}

async fn publish_terminal_after_commit(
    turn_control: &ActiveTurnControl,
    resolver: &dyn AwaitEventResolver,
    terminal: &TurnTerminal,
    session_id: &str,
    turn_id: &str,
) {
    if let Err(err) = turn_control.publish_terminal(resolver, terminal).await {
        tracing::warn!(
            error = %err,
            session_id,
            turn_id,
            "turn committed but terminal publication failed"
        );
    }
}

struct RuntimeStreamEventPump<'pump> {
    assembler: &'pump mut TurnAssembler,
    events: &'pump dyn EventSink,
    turn_events: &'pump dyn TurnActivitySink,
}

impl RuntimeStreamEventPump<'_> {
    async fn emit(&mut self, event: RuntimeStreamEvent) {
        emit_runtime_stream_event_to_sinks(self.events, self.turn_events, event, self.assembler)
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_runtime_stream_event_to_sinks(
    events: &dyn EventSink,
    turn_events: &dyn TurnActivitySink,
    event: RuntimeStreamEvent,
    assembler: &mut TurnAssembler,
) {
    match event {
        RuntimeStreamEvent::Session(event) => {
            assembler.push(&event);
            emit_session_event_to_sink(events, event).await;
        }
        RuntimeStreamEvent::Turn(activity) => {
            emit_turn_activity_to_sink(turn_events, activity).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{
        ActiveTurnControl, TURN_CANCEL_START_GATE_ATTEMPTS, TURN_CANCEL_WATCH_MAX_ATTEMPTS,
        agent_frame_follow_turn_id, await_turn_cancellation_start_gate,
        await_turn_cancellation_with_retry, publish_terminal_after_commit,
    };
    use crate::{
        AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, ExecutionScope,
        NativeRuntimeEffectController, Resolution, ResolveOutcome, RuntimeError, TurnAddress,
        TurnCancellationEvidence, TurnFinish, TurnOutcome, TurnTerminal,
    };

    #[derive(Debug)]
    struct RecordingTestClock {
        inner: crate::testing::TestClock,
        sleeps: Mutex<Vec<std::time::Duration>>,
    }

    impl RecordingTestClock {
        fn new() -> Self {
            Self {
                inner: crate::testing::TestClock::new(0),
                sleeps: Mutex::new(Vec::new()),
            }
        }

        fn sleeps(&self) -> Vec<std::time::Duration> {
            self.sleeps.lock().expect("recording clock sleeps").clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::Clock for RecordingTestClock {
        fn now(&self) -> std::time::Instant {
            self.inner.now()
        }

        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            self.inner.timestamp_datetime()
        }

        async fn sleep(&self, duration: std::time::Duration) {
            self.sleeps
                .lock()
                .expect("record cancellation-watch sleep")
                .push(duration);
        }

        async fn sleep_until(&self, deadline: std::time::Instant) {
            self.inner.sleep_until(deadline).await;
        }
    }

    #[test]
    fn recording_test_clock_wall_clock_faces_agree() {
        let clock = RecordingTestClock::new();
        let clock: &dyn crate::Clock = &clock;
        let milliseconds = clock.timestamp_ms();
        let datetime = clock.timestamp_datetime();
        let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
            .expect("clock emits RFC 3339");
        assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
        assert_eq!(text.timestamp_millis() as u64, milliseconds);
    }

    #[derive(Default)]
    struct RejectTerminalPublication {
        attempts: AtomicUsize,
        native: NativeRuntimeEffectController,
    }

    #[async_trait::async_trait]
    impl AwaitEventResolver for RejectTerminalPublication {
        async fn await_event_key(
            &self,
            scope: &ExecutionScope,
            wait: AwaitEventWaitIdentity,
        ) -> Result<AwaitEventKey, RuntimeError> {
            self.native.await_event_key(scope, wait).await
        }

        async fn resolve_await_event(
            &self,
            _key: &AwaitEventKey,
            _resolution: Resolution,
        ) -> Result<ResolveOutcome, RuntimeError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(RuntimeError::new(
                crate::RuntimeErrorCode::TransientTerminalPublication,
                "terminal backend unavailable",
            ))
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
            cancel: tokio_util::sync::CancellationToken,
            deadline: Option<std::time::Instant>,
        ) -> Result<Resolution, RuntimeError> {
            self.native.await_await_event(key, cancel, deadline).await
        }

        async fn revoke_await_events_for_session(
            &self,
            session_id: &str,
        ) -> Result<(), RuntimeError> {
            self.native
                .revoke_await_events_for_session(session_id)
                .await
        }

        async fn cancel_await_events_for_session(
            &self,
            session_id: &str,
        ) -> Result<(), RuntimeError> {
            self.native
                .cancel_await_events_for_session(session_id)
                .await
        }
    }

    #[test]
    fn agent_frame_follow_turn_ids_are_distinct_and_deterministic() {
        assert_eq!(agent_frame_follow_turn_id("root-turn", 0), "root-turn");
        assert_eq!(
            agent_frame_follow_turn_id("root-turn", 1),
            "root-turn:agent-frame:1"
        );
        assert_eq!(
            agent_frame_follow_turn_id("root-turn", 2),
            "root-turn:agent-frame:2"
        );
    }

    #[tokio::test]
    async fn cancellation_watch_retries_transient_errors_until_evidence_arrives() {
        let clock = RecordingTestClock::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed_attempts = Arc::clone(&attempts);
        let evidence = await_turn_cancellation_with_retry(&clock, move || {
            let attempt = observed_attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 2 {
                    Err(RuntimeError::new(
                        crate::RuntimeErrorCode::TransientCancelWatch,
                        "temporary ingress failure",
                    ))
                } else {
                    Ok(Some(TurnCancellationEvidence {
                        request_id: "retry-request".to_string(),
                        origin: Some("test-user".to_string()),
                        reason: None,
                        undelivered: crate::TurnCancelDisposition::Defer,
                        mode: crate::TurnCancelMode::Immediate,
                        honoured_after_step: None,
                    }))
                }
            }
        })
        .await
        .expect("cancellation watch succeeds")
        .expect("cancellation evidence after retries");

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(evidence.request_id, "retry-request");
        assert_eq!(
            clock.sleeps(),
            vec![
                std::time::Duration::from_millis(25),
                std::time::Duration::from_millis(50),
            ],
            "watcher retries must sleep on the injected clock"
        );
    }

    #[tokio::test]
    async fn cancellation_watch_fails_after_its_error_budget() {
        let clock = RecordingTestClock::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed_attempts = Arc::clone(&attempts);
        let err = await_turn_cancellation_with_retry(&clock, move || {
            observed_attempts.fetch_add(1, Ordering::SeqCst);
            async {
                Err(RuntimeError::new(
                    crate::RuntimeErrorCode::TransientCancelWatch,
                    "cancel resolver remains unavailable",
                ))
            }
        })
        .await
        .expect_err("the live cancellation watcher must fail closed after its retry budget");

        assert_eq!(err.code, crate::RuntimeErrorCode::TransientCancelWatch);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            TURN_CANCEL_WATCH_MAX_ATTEMPTS,
            "the live cancellation watcher must exhaust its retry budget before teardown"
        );
        assert_eq!(clock.sleeps().len(), TURN_CANCEL_WATCH_MAX_ATTEMPTS - 1);
    }

    #[tokio::test]
    async fn cancellation_start_gate_fails_after_bounded_retries() {
        let clock = RecordingTestClock::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed_attempts = Arc::clone(&attempts);
        let err = await_turn_cancellation_start_gate(&clock, move || {
            observed_attempts.fetch_add(1, Ordering::SeqCst);
            async {
                Err(RuntimeError::new(
                    crate::RuntimeErrorCode::CancelStartGateUnavailable,
                    "temporary ingress failure",
                ))
            }
        })
        .await
        .expect_err("start gate must fail closed after its retry budget");

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            TURN_CANCEL_START_GATE_ATTEMPTS
        );
        assert_eq!(err.code.to_string(), "cancel_start_gate_unavailable");
        assert_eq!(
            clock.sleeps(),
            vec![
                std::time::Duration::from_millis(25),
                std::time::Duration::from_millis(50),
            ],
            "start-gate retries must sleep on the injected clock"
        );
    }

    #[tokio::test]
    async fn terminal_publication_failure_is_non_fatal_after_commit() {
        let resolver = RejectTerminalPublication::default();
        let control = ActiveTurnControl::new(
            &resolver,
            TurnAddress::new("committed-session", "committed-turn"),
        )
        .await
        .expect("active turn control");
        publish_terminal_after_commit(
            &control,
            &resolver,
            &TurnTerminal::Committed {
                outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                    text: "committed".to_string(),
                }),
                session_revision: Some(1),
            },
            "committed-session",
            "committed-turn",
        )
        .await;
        assert_eq!(resolver.attempts.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
#[path = "turn_loop/panic_tests.rs"]
mod panic_tests;
