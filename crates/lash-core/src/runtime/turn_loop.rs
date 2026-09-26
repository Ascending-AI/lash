#[cfg(test)]
use super::logical_turn::next_physical_turn_id;
use super::logical_turn::{
    LogicalTurnClaims, LogicalTurnStart, PhysicalTurnExecution, PreparedLogicalTurn,
};
use super::turn_control::ActiveTurnControl;
use super::*;
use crate::SessionId;
use crate::TurnId;
use crate::facade_support::{ProtocolTurnOptionsFacadeOps, RuntimeSessionStateFacadeOps};
use lash_sansio::core_support::*;

mod accept;
mod commit;
mod drain_end;
mod execute;
mod follow_on_recovery;
mod generation_fence;
mod initial_drive;
mod lease;
mod post_commit;
#[cfg(feature = "testing")]
pub mod prepare;
#[cfg(not(feature = "testing"))]
mod prepare;
mod queued_work;
mod resident_session;

pub(in crate::runtime) use commit::LogicalTurnErrorContext;
use commit::{CancelledTurnFinishContext, TurnCommitContext, TurnFinishInput};
pub(in crate::runtime) use execute::PreparedTurnExecuteContext;
use execute::TurnDriverRemainder;
use lease::DriveClaimToBind;
use post_commit::PostCommitDelivery;
pub(in crate::runtime) use prepare::TurnPrepareContext;
pub use queued_work::{
    EmptyQueuedDrainReason, QueuedTurnDrain, SelectedQueuedWorkBatchSatisfaction,
    SelectedQueuedWorkDrainError, SelectedQueuedWorkDrainOutcome,
    SelectedQueuedWorkDrainRefusalCause,
};
pub(in crate::runtime) use resident_session::ResidentSessionContinuity;
pub use resident_session::ResidentSessionState;

/// What every turn phase publishes through: the logical turn's observer,
/// whose host end [`drive_logical_turn`](LashRuntime::drive_logical_turn)
/// publishes to the host sinks outside the drive.
pub(in crate::runtime) struct TurnSinks<'sinks> {
    pub(in crate::runtime) observer: &'sinks TurnObserver,
}

/// The session-execution lease a turn phase runs under, together with the
/// policy that decides whether reaching the end of the phase releases it.
///
/// The guard and the policy are always passed together and are meaningless
/// apart, so they travel as one field on the phase contexts.
pub(in crate::runtime) struct TurnLeaseScope<'lease> {
    pub(in crate::runtime) guard: Option<&'lease SessionExecutionLeaseGuard>,
    pub(in crate::runtime) release_policy: SessionExecutionLeaseReleasePolicy,
}

/// Projects a terminal turn outcome onto the closed trace outcome.
///
/// Cancellation is its own trace variant carrying the evidence
/// [`TurnStop::Cancelled`] already holds, so a cancelled turn is never traced
/// as a failure.
fn trace_outcome(outcome: &TurnOutcome) -> Option<lash_trace::TraceTurnOutcome> {
    use lash_trace::{TraceTurnCompletionReason as Reason, TraceTurnOutcome as Outcome};
    Some(match outcome {
        // A queued call ran no turn, so there is no completed turn to trace.
        TurnOutcome::Queued { .. } => return None,
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
                TurnStop::ContextOverflow => Outcome::Failed {
                    done_reason: Failure::ContextOverflow,
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
    })
}

pub(super) fn post_commit_delivery_issue(
    code: crate::FailureCode,
    message: impl Into<String>,
) -> TurnIssue {
    TurnIssue {
        severity: crate::runtime::TurnIssueSeverity::Blocking,
        kind: crate::TurnFailureKind::Runtime,
        code: Some(code),
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
    fn should_release(self, outcome: &TurnOutcome, withheld_terminal_work: bool) -> bool {
        match self {
            // FIG-3157: a terminal finish that withheld claimed work is still
            // mid-run. The follow-on turn drives that claim, and a claim stays
            // generation-valid only while the lease that fenced it is held
            // (ADR 0029), so the guard travels with the work.
            Self::KeepOnAgentFrameSwitch => {
                !matches!(outcome, TurnOutcome::AgentFrameSwitch { .. }) && !withheld_terminal_work
            }
        }
    }
}

fn queued_work_payload_type(payload: &crate::QueuedWorkPayload) -> &'static str {
    match payload {
        crate::QueuedWorkPayload::ProcessWake { .. } => "process_wake",
        crate::QueuedWorkPayload::SessionCommand { command } => command.kind(),
    }
}

fn queued_work_batch_ids(claim: &crate::QueuedWorkClaim) -> Vec<crate::BatchId> {
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

fn turn_phase_id(parent_turn_id: &TurnId, phase: &str) -> TurnId {
    TurnId::from(format!("{parent_turn_id}:{phase}"))
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

/// A fresh observation cursor for one turn-level emission lane of the
/// physical turn `turn_id` admitted under `controller`'s scope (ADR 0105 §1:
/// `(replay key, ordinal)` is the identity). Frames of one logical turn share
/// the scope's journal key, so `turn_id` and `lane` keep each physical turn's
/// lanes distinct.
pub(in crate::runtime) fn turn_observation_cursor(
    scoped_effect_controller: &ScopedEffectController<'_>,
    turn_id: &TurnId,
    lane: &str,
) -> crate::engine::ObservationCursor {
    let execution_scope = scoped_effect_controller.execution_scope();
    debug_assert!(
        execution_scope.journal_identity().is_ok(),
        "turn observation lanes require the scope's journal identity, but scope `{}` names none",
        execution_scope.id(),
    );
    let scope = execution_scope
        .journal_identity()
        .map(|identity| identity.key().to_owned())
        .unwrap_or_else(|_| format!("turn:{}", execution_scope.id()));
    crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new(format!(
        "{scope}:{turn_id}:{lane}"
    )))
}

pub(in crate::runtime) fn emit_turn_started(
    observer: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    turn_id: &TurnId,
) {
    cursor.observe(
        &observer.for_turn(turn_id),
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
        },
    );
}

pub(in crate::runtime) fn emit_queued_work_started(
    observer: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    turn_id: &TurnId,
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: Vec<crate::TurnCause>,
) {
    cursor.observe(
        &observer.for_turn(turn_id),
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::QueuedWorkStarted {
                boundary,
                batch_ids: queued_work_batch_ids(claim)
                    .into_iter()
                    .map(crate::BatchId::into_inner)
                    .collect(),
                causes,
            },
        },
    );
}

pub(in crate::runtime) fn send_queued_work_started_event(
    event_tx: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: Vec<crate::TurnCause>,
) {
    cursor.observe(
        event_tx,
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::QueuedWorkStarted {
                boundary,
                batch_ids: queued_work_batch_ids(claim)
                    .into_iter()
                    .map(crate::BatchId::into_inner)
                    .collect(),
                causes,
            },
        },
    );
}

trait TypedTurnPhase {
    const RUNTIME_PHASE: RuntimeTurnPhase;
}

impl LashRuntime {
    pub(super) fn max_context_tokens(&self) -> usize {
        self.state.effective_policy().context_window_tokens()
    }

    pub fn set_turn_phase_probe(&mut self, probe: Arc<dyn RuntimeTurnPhaseProbe>) {
        self.turn_phase_probe = Some(probe);
    }

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
    turn_id: &TurnId,
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
    fn as_envelope_kind(self) -> crate::TurnFailureKind {
        match self {
            Self::Runtime => crate::TurnFailureKind::Runtime,
            Self::InputValidation => crate::TurnFailureKind::InputValidation,
            Self::Plugin => crate::TurnFailureKind::Plugin,
        }
    }
}

/// How a terminal diagnostic's turn activity is addressed on the observer.
enum TerminalActivityTarget<'a> {
    /// The observer is already turn-scoped, so the activity publishes as is.
    TurnScoped(&'a TurnObserver),
    /// The observer is unscoped, so the activity is addressed to `turn_id`.
    ForTurn {
        observer: &'a TurnObserver,
        turn_id: &'a TurnId,
    },
}

/// Typed diagnostic emitted immediately ahead of a terminal `TurnOutcome`.
struct TerminalDiagnostic<'a> {
    kind: TerminalDiagnosticKind,
    code: Option<crate::FailureCode>,
    message: String,
    retryable: Option<bool>,
    activity: TerminalActivityTarget<'a>,
}

/// Emit the canonical terminal sequence for a stopped turn.
///
/// The order is fixed and load-bearing for host transcripts: the optional
/// diagnostic's session `Error` event (with its turn activity emitted in
/// between), then `TurnOutcome::Stopped(stop)`, then `Done`. Every session
/// event is recorded on `recorded_assembly` as it is written, so the committed
/// turn carries the same terminal facts the host streamed.
fn emit_terminal_sequence(
    recorded_assembly: &mut RecordedTurnAssembly,
    observer: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    diagnostic: Option<TerminalDiagnostic<'_>>,
    stop: TurnStop,
) {
    if let Some(diagnostic) = diagnostic {
        let error_event = SessionStreamEvent::Error {
            message: diagnostic.message.clone(),
            envelope: Some(crate::session_model::ErrorEnvelope {
                kind: diagnostic.kind.as_envelope_kind(),
                code: diagnostic.code,
                terminal_reason: None,
                user_message: diagnostic.message.clone(),
                raw: None,
                retryable: diagnostic.retryable,
                provider_failure_kind: None,
            }),
        };
        recorded_assembly.record(&error_event);
        // The diagnostic activity publishes ahead of the session error it
        // belongs to; the session events stay verbatim — `observe` would
        // project a second `TurnEvent::Error`.
        let activity = crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::Error {
                message: diagnostic.message,
            },
        };
        match diagnostic.activity {
            TerminalActivityTarget::TurnScoped(sink) => {
                cursor.observe(sink, activity);
            }
            TerminalActivityTarget::ForTurn { observer, turn_id } => {
                cursor.observe(&observer.for_turn(turn_id), activity);
            }
        }
        observer.publish(crate::runtime::RuntimeStreamEvent::Session(error_event));
    }
    let outcome_event = SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Stopped(stop),
    };
    recorded_assembly.record(&outcome_event);
    observer.publish(crate::runtime::RuntimeStreamEvent::Session(outcome_event));
    recorded_assembly.record(&SessionStreamEvent::Done);
    observer.publish(crate::runtime::RuntimeStreamEvent::Session(
        SessionStreamEvent::Done,
    ));
}

/// Publish one observation to its host sink, addressing an activity to its
/// physical turn when it has one.
pub(in crate::runtime) async fn publish_observation(
    events: &dyn EventSink,
    turn_events: &dyn TurnActivitySink,
    observation: Observation,
) {
    match observation.event {
        RuntimeStreamEvent::Session(event) => emit_session_event_to_sink(events, event).await,
        RuntimeStreamEvent::Turn(activity) => match observation.turn {
            Some(turn_id) => {
                emit_turn_activity_to_sink_for_turn(turn_events, &turn_id, activity).await;
            }
            None => emit_turn_activity_to_sink(turn_events, activity).await,
        },
    }
}

async fn publish_terminal_after_commit(
    turn_control: &ActiveTurnControl,
    resolver: &dyn AwaitEventResolver,
    terminal: &TurnTerminal,
    session_id: &SessionId,
    turn_id: &TurnId,
) {
    if let Err(err) = turn_control.publish_terminal(resolver, terminal).await {
        tracing::warn!(
            error = %err,
            session_id = session_id.as_str(),
            turn_id = turn_id.as_str(),
            "turn committed but terminal publication failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::SessionId;
    use crate::TurnId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ActiveTurnControl, next_physical_turn_id, publish_terminal_after_commit};
    use crate::{
        AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, ExecutionScope, Resolution,
        ResolveOutcome, RuntimeError, TurnAddress, TurnFinish, TurnOutcome, TurnTerminal,
    };

    /// Refuses every terminal publication and forwards the rest of the
    /// await-event surface to a backend host's controller for the turn.
    struct RejectTerminalPublication {
        attempts: AtomicUsize,
        inner: Arc<dyn crate::RuntimeEffectController>,
    }

    #[async_trait::async_trait]
    impl AwaitEventResolver for RejectTerminalPublication {
        fn await_event_authority_binding_id(&self) -> Option<String> {
            self.inner.await_event_authority_binding_id()
        }

        async fn await_event_key(
            &self,
            scope: &ExecutionScope,
            wait: AwaitEventWaitIdentity,
        ) -> Result<AwaitEventKey, RuntimeError> {
            self.inner.await_event_key(scope, wait).await
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
            self.inner.peek_await_event(key).await
        }

        async fn await_await_event(
            &self,
            key: &AwaitEventKey,
            cancel: tokio_util::sync::CancellationToken,
            deadline: Option<std::time::Instant>,
        ) -> Result<Resolution, RuntimeError> {
            self.inner.await_await_event(key, cancel, deadline).await
        }

        async fn revoke_await_events_for_session(
            &self,
            session_id: &SessionId,
        ) -> Result<(), RuntimeError> {
            self.inner.revoke_await_events_for_session(session_id).await
        }

        async fn cancel_await_events_for_session(
            &self,
            session_id: &SessionId,
        ) -> Result<(), RuntimeError> {
            self.inner.cancel_await_events_for_session(session_id).await
        }
    }

    #[test]
    fn physical_turn_ids_count_on_from_the_root_deterministically() {
        let first = next_physical_turn_id(&TurnId::from("root-turn")).expect("first");
        assert_eq!(first, "root-turn:agent-frame:1");
        let second = next_physical_turn_id(&first).expect("second");
        assert_eq!(second, "root-turn:agent-frame:2");
        assert_eq!(
            crate::store::QueuedRunPosition::split_turn_id(&second),
            (TurnId::from("root-turn"), 2)
        );
    }

    #[tokio::test]
    async fn terminal_publication_failure_is_non_fatal_after_commit() {
        let backend = crate::testing::memory_backend().await;
        let resolver = RejectTerminalPublication {
            attempts: AtomicUsize::new(0),
            inner: backend
                .effect_host()
                .scoped_static(crate::AdmittedScope::turn(
                    SessionId::from("committed-session"),
                    TurnId::from("committed-turn"),
                ))
                .expect("admit the turn scope")
                .expect("the backend host lends a static controller")
                .owned_controller()
                .expect("a static controller is shared"),
        };
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
            &SessionId::from("committed-session"),
            &TurnId::from("committed-turn"),
        )
        .await;
        assert_eq!(resolver.attempts.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
#[path = "turn_loop/panic_tests.rs"]
mod panic_tests;

#[cfg(test)]
#[path = "turn_loop/recovery_tests.rs"]
mod recovery_tests;
