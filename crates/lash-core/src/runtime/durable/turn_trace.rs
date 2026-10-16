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

use lash_core_execution::runtime::actor::round::ExecutionDraft;
use lash_trace::{
    DurableTraceScope, EmissionPermit, TraceAgentFrameSwitch, TraceContext, TraceEvent,
    TraceToolCallOutcome, TraceToolCallOutput, TraceTransitionKind, TraceTurnCancellationEvidence,
    TraceTurnCompletionReason, TraceTurnFailureReason, TraceTurnOutcome,
};

use super::session::TurnRow;
use crate::store::{RunCommittedOutcome, RunTerminalCause};
use crate::trace::TraceRuntime;
use crate::{SessionId, TurnFinish, TurnId, TurnStop};

/// The ids of a turn's records: its session, itself, and the send its head
/// input names, the one whose producer parents it.
fn context(session: &SessionId, run: &TurnId, send: Option<&crate::InputId>) -> TraceContext {
    let mut context = TraceContext::default()
        .for_session(session.clone())
        .for_turn(run.clone());
    context.send_id = send.cloned();
    context
}

/// Emit `turn_started` for `run`, whose `turn.admit` commit was just
/// acknowledged, under the trace scope that commit retained. It took
/// `inputs` session inputs, the first `send`. Its time is the scope's
/// retained start.
pub(super) fn started(
    tracing: Option<&TraceRuntime>,
    session: &SessionId,
    run: &TurnId,
    scope: Option<&DurableTraceScope>,
    inputs: usize,
    send: Option<&crate::InputId>,
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
                context(session, run, send),
                TraceEvent::TurnStarted {
                    metadata: [("input_count".to_owned(), serde_json::json!(inputs))].into(),
                },
            )
        },
    );
}

/// What `turn_completed` reports for a turn that ends with `cause`: total
/// over every cause that can end an admitted turn, so no terminal leaves a
/// turn's children without their parent. `None` when nothing observes the
/// runtime, and for the two causes that end no admitted turn: a deletion
/// tombstone's read answer and a command run's end.
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
    // An operator's control intent ended the turn: cancelled, its origin
    // the intent's kind.
    let operator = |intent: u64, origin: &str| TraceTurnOutcome::Cancelled {
        evidence: TraceTurnCancellationEvidence {
            request_id: intent.to_string(),
            origin: Some(origin.to_owned()),
            reason: None,
        },
    };
    Some(match cause {
        RunTerminalCause::Committed { outcome, .. } => match outcome {
            RunCommittedOutcome::Finished { finish, .. } => TraceTurnOutcome::Completed {
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
                TurnStop::ProviderError { .. } => failed(TraceTurnFailureReason::ProviderError),
                TurnStop::ContextOverflow => failed(TraceTurnFailureReason::ContextOverflow),
                TurnStop::PluginAbort => failed(TraceTurnFailureReason::PluginAbort),
                TurnStop::RuntimeError { .. } => failed(TraceTurnFailureReason::RuntimeError),
                TurnStop::AgentFrameSwitchLimit => {
                    failed(TraceTurnFailureReason::AgentFrameSwitchLimit)
                }
                TurnStop::SubmittedError { .. } => failed(TraceTurnFailureReason::SubmittedError),
                TurnStop::ToolError { .. } => failed(TraceTurnFailureReason::ToolError),
            },
        },
        RunTerminalCause::Cancelled { evidence, .. } => cancelled(evidence),
        RunTerminalCause::Refused { .. } => failed(TraceTurnFailureReason::RuntimeError),
        RunTerminalCause::OperatorCancelled { intent } => operator(intent.sequence(), "operator"),
        RunTerminalCause::Forked { intent, .. } => operator(intent.sequence(), "fork"),
        RunTerminalCause::SessionDeleted { .. } | RunTerminalCause::CommandsApplied => {
            return None;
        }
    })
}

/// Capture the admitted input before the terminal settles its rows. Every
/// closing owner reads the same durable inputs; no opener or resident drive
/// is needed. Trace read failures omit content without changing the answer.
pub(super) async fn input(
    cx: &crate::ActorContext,
    tracing: Option<&TraceRuntime>,
    row: &TurnRow,
    delivered: &[crate::AdmittedTurnInputs],
) -> Vec<lash_trace::TraceLlmMessage> {
    let Some(tracing) = tracing.filter(|tracing| tracing.is_observed()) else {
        return Vec::new();
    };
    let standing = tracing.unreplayed(row.admission.trace().cloned()).selected(
        lash_trace::TraceSpanKind::Agent,
        &context(&row.session, &row.run, row.admission.input_ids().first()).ids(),
    );
    if !standing
        .content()
        .captures(lash_trace::ContentCategory::InputMessages)
    {
        return Vec::new();
    }
    let store = cx.backend().session_store_factory();
    let mut inputs = Vec::new();
    for id in row.admission.input_ids() {
        match store.pending_turn_input(&row.session, id).await {
            Ok(Some(read)) => inputs.push(read.input),
            result => {
                tracing::warn!(session = %row.session, input = %id, ?result,
                    "turn trace input could not be read");
                return Vec::new();
            }
        }
    }
    for batch in delivered {
        for input in &batch.inputs {
            if !inputs.iter().any(|prior| prior.input_id == input.input_id) {
                inputs.push(input.clone());
            }
        }
    }
    inputs
        .iter()
        .map(|pending| lash_trace::TraceLlmMessage {
            role: "user".to_owned(),
            blocks: pending
                .input
                .items
                .iter()
                .map(|item| match item {
                    crate::InputItem::Text { text } => lash_trace::TraceContentBlock::Text {
                        text: text.clone(),
                        cache_breakpoint: false,
                    },
                    crate::InputItem::Attachment { reference } => {
                        lash_trace::TraceContentBlock::Attachment {
                            reference: Box::new(lash_trace::TraceAttachment {
                                id: reference.id.to_string(),
                                media_type: reference.media_type.to_string(),
                                byte_len: reference.byte_len,
                                position: "message".to_owned(),
                                delivery_form: None,
                            }),
                        }
                    }
                })
                .collect(),
        })
        .collect()
}

/// The answer comes from the terminal being committed, never from the last
/// model call (which may have ended in a tool call or a failure).
pub(super) fn output(
    tracing: Option<&TraceRuntime>,
    row: &TurnRow,
    cause: &RunTerminalCause,
) -> Vec<lash_trace::TraceLlmMessage> {
    let Some(tracing) = tracing.filter(|tracing| tracing.is_observed()) else {
        return Vec::new();
    };
    let standing = tracing.unreplayed(row.admission.trace().cloned()).selected(
        lash_trace::TraceSpanKind::Agent,
        &context(&row.session, &row.run, row.admission.input_ids().first()).ids(),
    );
    if !standing
        .content()
        .captures(lash_trace::ContentCategory::OutputMessages)
    {
        return Vec::new();
    }
    let RunTerminalCause::Committed {
        outcome: RunCommittedOutcome::Finished { finish, .. },
        ..
    } = cause
    else {
        return Vec::new();
    };
    let text = match finish {
        TurnFinish::AssistantMessage { text } => text.clone(),
        TurnFinish::Finished { value, .. } => match value {
            serde_json::Value::String(text) => text.clone(),
            value => value.to_string(),
        },
    };
    vec![lash_trace::TraceLlmMessage {
        role: "assistant".into(),
        blocks: vec![lash_trace::TraceContentBlock::Text {
            text,
            cache_breakpoint: false,
        }],
    }]
}

/// Emit `turn_completed` with `outcome` for `row`'s turn, whose terminal
/// commit was just acknowledged, under the trace scope its admission
/// retained, linking what it consumed: the producers of `delivered`, the
/// inputs its checkpoints delivered while it ran, which its checkpoint
/// retains with their causes. Its time is this owner's clock at the
/// acknowledgement: the commit's receipt carries none.
pub(super) fn ended(
    tracing: Option<&TraceRuntime>,
    row: &TurnRow,
    outcome: Option<TraceTurnOutcome>,
    delivered: &[crate::AdmittedTurnInputs],
    content: (
        Vec<lash_trace::TraceLlmMessage>,
        Vec<lash_trace::TraceLlmMessage>,
    ),
    counts: (u32, Option<u64>),
) {
    let (Some(tracing), Some(scope), Some(outcome)) = (tracing, row.admission.trace(), outcome)
    else {
        return;
    };
    tracing
        .unreplayed(Some(consumed(tracing, scope, delivered)))
        .transition(
            Some(&EmissionPermit::new_transition()),
            tracing.clock().timestamp_ms(),
            TraceTransitionKind::Terminal,
            0,
            || {
                (
                    context(&row.session, &row.run, row.admission.input_ids().first()),
                    TraceEvent::TurnCompleted {
                        input: content.0,
                        output: content.1,
                        inference_calls: Some(counts.0),
                        tool_calls: counts.1,
                        outcome,
                        inputs: row.admission.input_ids().len(),
                    },
                )
            },
        );
}

/// How a terminal that ended a turn left a round member that had not
/// settled: settled `Cancelled` in the terminal's commit, or discarded
/// with no outcome by one that settles nothing (the session close's
/// cancel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MemberEnd {
    Cancelled,
    Discarded,
}

/// Close the span of each of `members`, the round members of `row`'s turn
/// its terminal commit, just acknowledged, ended as `end`: no other owner
/// closes them, since none settles them again (P1). Each closes at this
/// owner's clock, under the scope its admission retained; a member
/// admitted untraced closes nothing.
pub(super) fn members_ended(
    tracing: Option<&TraceRuntime>,
    row: &TurnRow,
    members: &[ExecutionDraft],
    end: MemberEnd,
) {
    let Some(tracing) = tracing.filter(|tracing| tracing.is_observed()) else {
        return;
    };
    let ended_at_ms = tracing.clock().timestamp_ms();
    for member in members {
        let Some(scope) = member.trace() else {
            continue;
        };
        let outcome = match end {
            MemberEnd::Cancelled => TraceToolCallOutcome::Cancelled(
                crate::ToolCancellation::runtime("the turn ended before the call settled")
                    .with_origin(crate::CancelOrigin::TurnStopped)
                    .to_json_value(),
            ),
            MemberEnd::Discarded => TraceToolCallOutcome::Discarded,
        };
        tracing
            .unreplayed(Some(scope.clone()))
            .close_scope(ended_at_ms, || {
                (
                    context(&row.session, &row.run, row.admission.input_ids().first())
                        .for_call(member.call().clone()),
                    TraceEvent::ToolCallCompleted {
                        call_id: member.call().clone(),
                        provider_call_id: None,
                        name: member.tool().to_string(),
                        args: serde_json::Value::Null,
                        output: TraceToolCallOutput {
                            outcome,
                            control: None,
                        },
                        duration_ms: ended_at_ms.saturating_sub(scope.started_at_ms),
                        issuing_node_id: None,
                        attempts: None,
                        span_attributes: Default::default(),
                    },
                )
            });
    }
}

/// `scope` linking, after its admission links, the producer of every input
/// in `delivered` its turn consumed while it ran: a consumed input is
/// linked, never a second parent (D1). Each producer is linked once, the
/// turn's own parent never, and the overflow past the link limit is
/// counted.
fn consumed(
    tracing: &TraceRuntime,
    scope: &DurableTraceScope,
    delivered: &[crate::AdmittedTurnInputs],
) -> DurableTraceScope {
    let mut scope = scope.clone();
    let admitted = scope.links.omitted();
    scope.links = std::mem::take(&mut scope.links).limited(tracing.limits().links());
    let parent = scope.cause.as_parent();
    for input in delivered.iter().flat_map(|admitted| &admitted.inputs) {
        if let lash_trace::TraceCause::Parent(producer) = &input.trace_cause
            && !parent
                .as_ref()
                .is_some_and(|parent| parent.carrier().same_span(producer))
        {
            scope.links.push(producer.clone());
        }
    }
    let omitted = scope.links.omitted().saturating_sub(admitted);
    if omitted > 0 {
        let mut counts = lash_trace::TraceOmissionCounts::default();
        counts.add_n(lash_trace::TraceOmission::Links, omitted);
        tracing.omissions().record(
            &lash_trace::TraceOmissionSite::Admission {
                scope: scope.scope.clone(),
            },
            &counts,
        );
    }
    scope
}
