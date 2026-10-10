//! A turn's logical trace records (FIG-5658): `turn_started` once its
//! admission commits and `turn_completed` once its terminal does.
//!
//! Each is emitted by the owner whose commit was acknowledged: `turn.admit`
//! refuses a second open turn and a terminal refuses a turn no longer open,
//! so the acknowledged commit is the transition's first writer. An owner
//! that takes the turn over, a resend of its input and a reader of its ended
//! run commit neither and emit nothing. An owner that loses the
//! acknowledgement, or its life before the emission, loses the record:
//! trace delivery is best effort and has no outbox.

use lash_trace::{
    DurableTraceScope, EmissionPermit, TraceAgentFrameSwitch, TraceContext, TraceEvent,
    TraceTransitionKind, TraceTurnCancellationEvidence, TraceTurnCompletionReason,
    TraceTurnFailureReason, TraceTurnOutcome,
};

use super::session::TurnRow;
use crate::store::{RunCommittedOutcome, RunTerminalCause};
use crate::trace::TraceRuntime;
use crate::{SessionId, TurnFinish, TurnId, TurnStop};

fn context(session: &SessionId, run: &TurnId) -> TraceContext {
    TraceContext::default()
        .for_session(session.clone())
        .for_turn(run.clone())
}

/// Emit `turn_started` for `run`, whose `turn.admit` commit was just
/// acknowledged, under the trace scope that commit retained. It took
/// `inputs` session inputs. Its time is the scope's retained start.
pub(super) fn started(
    tracing: Option<&TraceRuntime>,
    session: &SessionId,
    run: &TurnId,
    scope: Option<&DurableTraceScope>,
    inputs: usize,
) {
    let (Some(tracing), Some(scope)) = (tracing, scope) else {
        return;
    };
    tracing.unreplayed(Some(scope.clone())).transition(
        Some(&EmissionPermit::new_transition()),
        scope.started_at_ms,
        TraceTransitionKind::Started,
        0,
        || {
            (
                context(session, run),
                TraceEvent::TurnStarted {
                    metadata: [("input_count".to_owned(), serde_json::json!(inputs))].into(),
                },
            )
        },
    );
}

/// What `turn_completed` reports for a turn that ends with `cause`; `None`
/// when nothing observes the runtime, or for a cause no turn terminal
/// writes.
pub(super) fn outcome(
    tracing: Option<&TraceRuntime>,
    cause: &RunTerminalCause,
) -> Option<TraceTurnOutcome> {
    if !tracing?.is_observed() {
        return None;
    }
    let cancelled =
        |evidence: &crate::runtime::TurnCancellationEvidence| TraceTurnOutcome::Cancelled {
            evidence: TraceTurnCancellationEvidence {
                request_id: evidence.request_id.clone(),
                origin: evidence.origin.clone(),
                reason: evidence.reason.clone(),
            },
        };
    let failed = |done_reason| TraceTurnOutcome::Failed { done_reason };
    Some(match cause {
        RunTerminalCause::Committed { outcome, .. } => match outcome {
            RunCommittedOutcome::Finished(finish) => TraceTurnOutcome::Completed {
                done_reason: match finish {
                    TurnFinish::AssistantMessage { .. } => {
                        TraceTurnCompletionReason::AssistantMessage
                    }
                    TurnFinish::Finished { .. } => TraceTurnCompletionReason::Finished,
                },
            },
            RunCommittedOutcome::AgentFrameSwitch { frame_key, .. } => {
                TraceTurnOutcome::AgentFrameSwitch {
                    frame_switch: TraceAgentFrameSwitch {
                        frame_key: frame_key.as_str().to_owned(),
                    },
                }
            }
            RunCommittedOutcome::Stopped(stop) => match stop {
                TurnStop::Cancelled { evidence } => cancelled(evidence),
                TurnStop::Incomplete => failed(TraceTurnFailureReason::Incomplete),
                TurnStop::InvalidInput => failed(TraceTurnFailureReason::InvalidInput),
                TurnStop::MaxTurns => failed(TraceTurnFailureReason::MaxTurns),
                TurnStop::ToolFailure | TurnStop::ToolPanicked { .. } => {
                    failed(TraceTurnFailureReason::ToolFailure)
                }
                TurnStop::ProviderError => failed(TraceTurnFailureReason::ProviderError),
                TurnStop::ContextOverflow => failed(TraceTurnFailureReason::ContextOverflow),
                TurnStop::PluginAbort => failed(TraceTurnFailureReason::PluginAbort),
                TurnStop::RuntimeError => failed(TraceTurnFailureReason::RuntimeError),
                TurnStop::AgentFrameSwitchLimit => {
                    failed(TraceTurnFailureReason::AgentFrameSwitchLimit)
                }
                TurnStop::SubmittedError { .. } => failed(TraceTurnFailureReason::SubmittedError),
                TurnStop::ToolError { .. } => failed(TraceTurnFailureReason::ToolError),
            },
        },
        RunTerminalCause::Cancelled { evidence } => cancelled(evidence),
        RunTerminalCause::Refused { .. } => failed(TraceTurnFailureReason::RuntimeError),
        RunTerminalCause::OperatorCancelled { .. }
        | RunTerminalCause::Forked { .. }
        | RunTerminalCause::SessionDeleted { .. }
        | RunTerminalCause::CommandsApplied => return None,
    })
}

/// Emit `turn_completed` with `outcome` for `row`'s turn, whose terminal
/// commit was just acknowledged, under the trace scope its admission
/// retained. Its time is this owner's clock at the acknowledgement: the
/// commit's receipt carries none.
pub(super) fn ended(
    tracing: Option<&TraceRuntime>,
    row: &TurnRow,
    outcome: Option<TraceTurnOutcome>,
) {
    let (Some(tracing), Some(scope), Some(outcome)) = (tracing, row.admission.trace(), outcome)
    else {
        return;
    };
    tracing.unreplayed(Some(scope.clone())).transition(
        Some(&EmissionPermit::new_transition()),
        tracing.clock().timestamp_ms(),
        TraceTransitionKind::Terminal,
        0,
        || {
            (
                context(&row.session, &row.run),
                TraceEvent::TurnCompleted { outcome },
            )
        },
    );
}
