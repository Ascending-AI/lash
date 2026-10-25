//! Native-channel prompt history: each prior cell as its captured provider
//! exchange pair, a refused call's pair where the call stands, and a frame
//! seed's step as user context. The ordinary-message and prose helpers and
//! the repaired-failure scrub are the cell channel's
//! (`crate::driver::history`); only the exchange rendering is this
//! channel's own.

use std::sync::Arc;

use lash_core::facade_support::BorrowedChronologicalPayload;
use lash_core::llm::types::{LlmMessage, LlmRole};

pub(super) use crate::driver::history::RlmHistoryRenderInput;
use crate::driver::history::{
    ExchangeMessage, PendingProse, append_step_image_blocks, complete_history_messages,
    completed_turn_suppressions, flush_pending_prose, ordinary_message, step_output_text,
    superseded_failure_indices,
};

pub(super) fn build_rlm_history_messages_from_turn(
    input: RlmHistoryRenderInput<'_>,
) -> Result<Vec<LlmMessage>, lash_core::StoredDataCorruption> {
    Ok(complete_history_messages(
        render_history_messages(&input)?,
        input.protocol_iteration,
    ))
}

/// The history portion only (no current-iteration tail): each prior step as an
/// assistant tool-call message + matching tool results, projected atomically.
#[expect(
    clippy::expect_used,
    reason = "the fallible suppression pass validates every event before the visitors run"
)]
pub(super) fn render_history_messages(
    input: &RlmHistoryRenderInput<'_>,
) -> Result<Vec<LlmMessage>, lash_core::StoredDataCorruption> {
    let mut messages = Vec::new();
    let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
        input.events,
        input.turn_messages,
    );
    let exchanges = super::transport::NativeExchangeIndex::new(&chronological);
    let suppressed = completed_turn_suppressions(input.dialect, input.events, input.turn_messages)?;
    let mut pending: Option<PendingProse> = None;
    let superseded =
        superseded_failure_indices(input.dialect, input.events, input.turn_messages, |index| {
            exchanges.contains(index).then(|| {
                if exchanges.refusal(index).is_some() {
                    ExchangeMessage::Refused
                } else {
                    ExchangeMessage::Executed
                }
            })
        });

    lash_core::facade_support::visit_turn_view(input.events, input.turn_messages, |entry| {
        // An exchange message renders only as its pair: a cell's at the
        // cell's semantic step, a refused call's where the call stands.
        if exchanges.contains(entry.index) {
            if superseded.contains(&entry.index) {
                pending = None;
            } else if let Some((parts, correction)) = exchanges.refusal(entry.index) {
                flush_pending_prose(&mut messages, &mut pending);
                super::transport::append_pair(&mut messages, parts, correction);
            }
            return;
        }
        if suppressed.contains(&entry.index) || superseded.contains(&entry.index) {
            pending = None;
            return;
        }
        match entry.payload {
            BorrowedChronologicalPayload::Message(message)
                if matches!(message.role, lash_core::MessageRole::Assistant) =>
            {
                // Keep standalone assistant prose in chronological order.
                flush_pending_prose(&mut messages, &mut pending);
                pending = Some(PendingProse::of(entry, message));
            }
            BorrowedChronologicalPayload::ProtocolEvent(event) => {
                let Some(event) = input
                    .dialect
                    .decode_history_event(event)
                    .expect("the suppression pass validated every RLM event")
                else {
                    return;
                };
                let lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step) = event.as_ref()
                else {
                    return;
                };
                flush_pending_prose(&mut messages, &mut pending);
                let observation = step_output_text(
                    input.dialect.prompt_vocabulary(),
                    step,
                    input.dialect.presentation(),
                );
                if let Some(parts) = exchanges.execution_parts(&step.id) {
                    super::transport::append_pair(&mut messages, parts, &observation);
                } else {
                    // Frame seeds carry semantic history, not authority to mint
                    // provider calls. Keep the facts visible as user context.
                    messages.push(LlmMessage::text(
                        LlmRole::User,
                        format!("Earlier program:\n{}\n\n{observation}", step.code),
                    ));
                }
                if let Some(message) = messages.last_mut() {
                    append_step_image_blocks(step, Arc::make_mut(&mut message.blocks));
                }
            }
            BorrowedChronologicalPayload::Message(message) => {
                flush_pending_prose(&mut messages, &mut pending);
                messages.push(ordinary_message(input, entry, message));
            }
        }
    });
    flush_pending_prose(&mut messages, &mut pending);
    Ok(messages)
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
