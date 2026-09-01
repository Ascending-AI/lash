//! Projection of a sans-io [`Effect`] stream into the
//! shared behavior-transcript vocabulary.
//!
//! The protocol scenario harnesses each drive a `TurnMachine` and drain effects,
//! so they render from one shared projection instead of growing private ones.
//! Every line comes from an effect the machine really yielded.
//!
//! Deliberate omissions, so a reviewer knows what a diff here cannot say:
//!
//! - Text and reasoning deltas, token usage, `LlmRequest`/`LlmResponse`
//!   observation events, `ToolCallStart`, plugin events and `Done` are dropped as
//!   provider-wire and bookkeeping volume. The outbound
//!   [`Effect::LlmCall`] and
//!   [`Effect::ToolCalls`] lines already carry
//!   the model and dispatch facts, and cannot be faked away by an observation
//!   regression.
//! - `Progress` / `Done` history deltas are not rendered. They are one append per
//!   protocol iteration and would double every transcript's length while saying
//!   nothing the checkpoint and outcome lines do not.

use lash_sansio::{Effect, TurnProtocol};

use super::behavior_transcript::{Actor, Attr, Entry, IdKind, Kind, Transcript, Usage};
use crate::{SessionStreamEvent, TurnFinish, TurnOutcome, TurnStop};

/// Event name for a protocol-requested checkpoint at the sans-io seam.
///
/// This is a *request* for a checkpoint, not an accepted store commit, so it
/// carries no revision transition. Where the protocol asks for a checkpoint is
/// still durable semantics, so the name is listed in
/// [`super::behavior_transcript::DURABLE_WRITE_EVENTS`].
pub const CHECKPOINT_REQUEST_EVENT: &str = "checkpoint.request";

/// Record every reviewable line for one drained batch of effects.
///
/// Call once per drain, in drain order, so the transcript's line order is the
/// machine's effect order.
pub fn record_effects<M: TurnProtocol>(
    transcript: &mut Transcript,
    actor: &str,
    effects: &[Effect<M>],
) {
    for effect in effects {
        record_effect(transcript, actor, effect);
    }
}

fn record_effect<M: TurnProtocol>(transcript: &mut Transcript, actor: &str, effect: &Effect<M>) {
    let session = || Actor::session(actor.to_string());
    match effect {
        Effect::LlmCall { request, .. } => {
            transcript.record(
                Entry::new(Kind::Provider, session(), "model.request")
                    .attr(Attr::int("messages", request.messages.len() as u64))
                    .attr(Attr::int("tools", request.tools.len() as u64)),
            );
        }
        Effect::ToolCalls { calls, .. } => {
            for call in calls {
                transcript.record(
                    Entry::new(Kind::Tool, session(), "tool.call")
                        .attr(Attr::text("name", &call.tool_name))
                        .attr(Attr::id("call", IdKind::Call, &call.call_id)),
                );
            }
        }
        Effect::ExecCode { language, .. } => {
            transcript.record(
                Entry::new(Kind::Exec, session(), "cell.start").attr(Attr::text("lang", language)),
            );
        }
        Effect::Checkpoint { checkpoint, .. } => {
            // This is a protocol request, not an accepted runtime commit; the
            // effect carries no submitted usage envelope to observe.
            transcript.record(
                Entry::new(Kind::Commit, session(), CHECKPOINT_REQUEST_EVENT)
                    .attr(Attr::debug_token("checkpoint", checkpoint))
                    .usage(Usage::none()),
            );
        }
        Effect::Emit(event) => record_stream_event(transcript, actor, event),
        Effect::SyncExecutionEnvironment { .. }
        | Effect::Log { .. }
        | Effect::Progress { .. }
        | Effect::Done { .. } => {}
    }
}

fn record_stream_event(transcript: &mut Transcript, actor: &str, event: &SessionStreamEvent) {
    let session = || Actor::session(actor.to_string());
    match event {
        SessionStreamEvent::ToolCall {
            call_id,
            name,
            output,
            ..
        } => {
            let mut entry = Entry::new(Kind::Tool, session(), "tool.result")
                .attr(Attr::text("name", name))
                .attr(Attr::debug_token("outcome", &output.status()));
            if let Some(call_id) = call_id {
                entry = entry.attr(Attr::id("call", IdKind::Call, call_id));
            }
            transcript.record(entry);
        }
        SessionStreamEvent::ToolCallsOmitted { summary } => {
            transcript.record(
                Entry::new(Kind::Tool, session(), "tool.omitted")
                    .attr(Attr::int("count", summary.count as u64))
                    .attr(Attr::int("failures", summary.failures as u64))
                    .attr(Attr::int("attachments", summary.attachments.len() as u64)),
            );
        }
        SessionStreamEvent::Message { text, kind } if kind == "final" => {
            transcript.record(
                Entry::new(Kind::Outcome, session(), "turn.final_message")
                    .attr(Attr::text("text", text)),
            );
        }
        SessionStreamEvent::Message { text, kind } => {
            transcript.record(
                Entry::new(Kind::Observe, session(), format!("message.{kind}"))
                    .attr(Attr::text("text", text)),
            );
        }
        SessionStreamEvent::RetryStatus {
            attempt,
            max_attempts,
            reason,
            ..
        } => {
            transcript.record(
                Entry::new(Kind::Fault, session(), "provider.retry")
                    .attr(Attr::int("attempt", *attempt as u64))
                    .attr(Attr::int("of", *max_attempts as u64))
                    .attr(Attr::text("reason", reason)),
            );
        }
        SessionStreamEvent::Error { message, .. } => {
            transcript.record(
                Entry::new(Kind::Fault, session(), "turn.error").attr(Attr::text("error", message)),
            );
        }
        SessionStreamEvent::InjectedTurnInputAccepted { inputs, checkpoint } => {
            transcript.record(
                Entry::new(Kind::Ingress, session(), "queued_input.accepted")
                    .attr(Attr::int("inputs", inputs.len() as u64))
                    .attr(Attr::debug_token("checkpoint", checkpoint)),
            );
        }
        SessionStreamEvent::InjectedMessagesCommitted {
            messages,
            checkpoint,
        } => {
            // Injected messages are a host-authored commit and never contain a
            // provider response, so this event genuinely submits no usage.
            transcript.record(
                Entry::new(Kind::Commit, session(), "injected_messages.committed")
                    .attr(Attr::int("messages", messages.len() as u64))
                    .attr(Attr::debug_token("checkpoint", checkpoint))
                    .usage(Usage::none()),
            );
        }
        SessionStreamEvent::TurnOutcome { outcome } => {
            transcript.record(outcome_entry(session(), outcome));
        }
        SessionStreamEvent::TextDelta { .. }
        | SessionStreamEvent::ReasoningDelta { .. }
        | SessionStreamEvent::ToolCallStart { .. }
        | SessionStreamEvent::LlmRequest { .. }
        | SessionStreamEvent::LlmResponse { .. }
        | SessionStreamEvent::TokenUsage { .. }
        | SessionStreamEvent::ChildTokenUsage { .. }
        | SessionStreamEvent::PluginEvent { .. }
        | SessionStreamEvent::Done => {}
    }
}

fn outcome_entry(actor: Actor, outcome: &TurnOutcome) -> Entry {
    match outcome {
        TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) => {
            Entry::new(Kind::Outcome, actor, "turn.assistant_message")
                .attr(Attr::text("text", text))
        }
        TurnOutcome::Finished(TurnFinish::FinalValue { value }) => {
            Entry::new(Kind::Outcome, actor, "turn.final_value").attr(Attr::json("value", value))
        }
        TurnOutcome::Finished(TurnFinish::ToolValue { tool_name, value }) => {
            Entry::new(Kind::Outcome, actor, "turn.tool_value")
                .attr(Attr::text("name", tool_name))
                .attr(Attr::json("value", value))
        }
        TurnOutcome::AgentFrameSwitch {
            frame_key,
            initial_nodes,
            ..
        } => Entry::new(Kind::Outcome, actor, "turn.frame_switch")
            .attr(Attr::id("frame_key", IdKind::Node, frame_key.as_str()))
            .attr(Attr::int("nodes", initial_nodes.len() as u64)),
        TurnOutcome::Stopped(stop) => {
            let mut entry = Entry::new(Kind::Outcome, actor, "turn.stopped")
                .attr(Attr::token("reason", stop_reason(stop)));
            match stop {
                TurnStop::SubmittedError { value } => {
                    entry = entry.attr(Attr::json("value", value));
                }
                TurnStop::ToolError { tool_name, value } => {
                    entry = entry
                        .attr(Attr::text("name", tool_name))
                        .attr(Attr::json("value", value));
                }
                _ => {}
            }
            entry
        }
    }
}

fn stop_reason(stop: &TurnStop) -> &'static str {
    match stop {
        TurnStop::Cancelled { .. } => "cancelled",
        TurnStop::Incomplete => "incomplete",
        TurnStop::InvalidInput => "invalid_input",
        TurnStop::MaxTurns => "max_turns",
        TurnStop::ToolFailure => "tool_failure",
        TurnStop::ProviderError => "provider_error",
        TurnStop::PluginAbort => "plugin_abort",
        TurnStop::RuntimeError => "runtime_error",
        TurnStop::SubmittedError { .. } => "submitted_error",
        TurnStop::ToolError { .. } => "tool_error",
    }
}
