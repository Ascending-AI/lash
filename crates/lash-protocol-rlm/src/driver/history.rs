//! RLM prompt history preserves complete provider call/result exchanges and replay metadata.

#[cfg(test)]
mod tests;

use crate::dialect::SessionDialect;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use lash_core::facade_support::{
    BorrowedChronologicalEntry, BorrowedChronologicalMessage, BorrowedChronologicalPayload,
};
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_rlm_types::{RlmAttachmentRef, RlmImageRef};

/// The chronological facts rendered into provider history.
pub(crate) struct RlmHistoryRenderInput<'a> {
    pub(crate) dialect: &'a SessionDialect,
    pub(crate) events: &'a [lash_core::SessionHistoryRecord],
    pub(crate) turn_messages: &'a lash_core::facade_support::MessageSequence,
    pub(crate) max_output_chars: usize,
    pub(crate) protocol_iteration: usize,
}

/// Assistant prose awaiting the next chronological boundary. Buffered because
/// `visit_turn_view` is a push visitor with no lookahead: the renderer flushes it at the next chronological boundary.
pub(crate) struct PendingProse {
    text: String,
    reasoning_blocks: Vec<LlmContentBlock>,
    image_blocks: Vec<LlmContentBlock>,
}

impl PendingProse {
    /// An assistant message's prose, its replayable reasoning and its images.
    pub(crate) fn of(
        entry: BorrowedChronologicalEntry<'_>,
        message: BorrowedChronologicalMessage<'_>,
    ) -> Self {
        let mut image_blocks = Vec::new();
        append_borrowed_entry_image_blocks(entry, &mut image_blocks);
        Self {
            text: message_history_text_parts(message.parts),
            reasoning_blocks: message_history_reasoning_blocks(message.parts),
            image_blocks,
        }
    }
}

pub(crate) fn build_rlm_history_messages_from_turn(
    input: RlmHistoryRenderInput<'_>,
) -> Result<Vec<LlmMessage>, lash_core::StoredDataCorruption> {
    Ok(complete_history_messages(
        render_history_messages(&input)?,
        input.protocol_iteration,
    ))
}

/// A channel's rendered history closed for a request: a placeholder when it
/// is empty, else a cache fence on its last text, then the current
/// iteration's prefix.
pub(crate) fn complete_history_messages(
    mut messages: Vec<LlmMessage>,
    protocol_iteration: usize,
) -> Vec<LlmMessage> {
    if messages.is_empty() {
        messages.push(LlmMessage::new(
            LlmRole::User,
            vec![text_block(
                "=== HISTORY ===\n\nNo chronological history is available.",
                false,
            )],
        ));
    } else {
        mark_last_history_text_cache_breakpoint(&mut messages);
    }
    append_current_iteration_message(&mut messages, protocol_iteration);
    messages
}

/// The history portion only (no current-iteration tail): each prior step as an
/// assistant tool-call message + matching tool results, projected atomically.
#[expect(
    clippy::expect_used,
    reason = "the fallible suppression pass validates every event before the visitors run"
)]
pub(crate) fn render_history_messages(
    input: &RlmHistoryRenderInput<'_>,
) -> Result<Vec<LlmMessage>, lash_core::StoredDataCorruption> {
    let mut messages = Vec::new();
    let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
        input.events,
        input.turn_messages,
    );
    let exchanges = crate::native::transport::NativeExchangeIndex::new(&chronological);
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
                crate::native::transport::append_pair(&mut messages, parts, correction);
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
                    crate::native::transport::append_pair(&mut messages, parts, &observation);
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

/// A user, system or event turn, rendered verbatim by role. Only real turn
/// input starts a cache segment.
pub(crate) fn ordinary_message(
    input: &RlmHistoryRenderInput<'_>,
    entry: BorrowedChronologicalEntry<'_>,
    message: BorrowedChronologicalMessage<'_>,
) -> LlmMessage {
    let text = message_text(
        &message_history_text_parts(message.parts),
        &message_attachment_refs(message.parts),
        input.max_output_chars,
    );
    let mut blocks = vec![text_block(text, false)];
    append_borrowed_entry_image_blocks(entry, &mut blocks);
    let role = match message.role {
        lash_core::MessageRole::User | lash_core::MessageRole::Event => LlmRole::User,
        lash_core::MessageRole::System => LlmRole::System,
        lash_core::MessageRole::Assistant => LlmRole::Assistant,
    };
    let mut projected = LlmMessage::new(role, blocks);
    projected.starts_user_segment = matches!(
        (message.role, message.origin),
        (
            lash_core::MessageRole::User,
            Some(lash_core::MessageOrigin::TurnInput { .. })
        )
    );
    projected
}

/// Chronological indices a committed assistant transcript makes redundant:
/// a successful terminal step followed, in the same turn, by an assistant
/// transcript message, and the protocol's own prose since the turn began.
/// The transcript is canonical for the completed turn's assistant prose, so
/// the cell that finished it, its folded prose and its never-observed output
/// echo are omitted. A terminal step with no committed transcript after it
/// stays. Judged by event and turn ordering and by origin, never by content.
///
/// The pass decodes every RLM event of the view, so a corrupt one refuses the
/// render before any visitor relies on it.
pub(crate) fn completed_turn_suppressions(
    dialect: &SessionDialect,
    events: &[lash_core::SessionHistoryRecord],
    turn_messages: &lash_core::facade_support::MessageSequence,
) -> Result<HashSet<usize>, lash_core::StoredDataCorruption> {
    let mut suppressed = HashSet::new();
    // The protocol's prose since the last turn boundary, and the successful
    // terminal step a transcript message would make redundant.
    let mut prose = Vec::new();
    let mut terminal = None;
    let mut corruption = None;
    lash_core::facade_support::visit_turn_view(events, turn_messages, |entry| {
        if corruption.is_some() {
            return;
        }
        match entry.payload {
            BorrowedChronologicalPayload::Message(message) => {
                let exchange =
                    crate::native::transport::is_exchange_message(message.origin, message.parts);
                match message.role {
                    lash_core::MessageRole::User if exchange => {}
                    lash_core::MessageRole::User | lash_core::MessageRole::Event => {
                        prose.clear();
                        terminal = None;
                    }
                    lash_core::MessageRole::Assistant => {
                        if crate::projection::is_rlm_protocol_output(message.origin) {
                            prose.push(entry.index);
                        } else if !exchange && is_transcript_content(message.parts) {
                            if let Some(step) = terminal.take() {
                                suppressed.insert(step);
                            }
                            suppressed.extend(prose.drain(..));
                        }
                    }
                    lash_core::MessageRole::System => {}
                }
            }
            BorrowedChronologicalPayload::ProtocolEvent(event) => {
                match dialect.decode_history_event(event) {
                    Ok(Some(event)) => {
                        if let lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step) =
                            event.as_ref()
                        {
                            terminal = step.result.finish().map(|_| entry.index);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => corruption = Some(error),
                }
            }
        }
    });
    corruption.map_or(Ok(suppressed), Err)
}

/// Whether a message carries text or attachments a reader would see.
fn is_transcript_content(parts: &[lash_core::Part]) -> bool {
    parts.iter().any(|part| {
        !part.identified_attachments().is_empty()
            || (matches!(
                part.kind(),
                lash_core::PartKind::Text | lash_core::PartKind::Prose
            ) && part
                .text_content()
                .is_some_and(|text| !text.trim().is_empty()))
    })
}

/// What a captured native provider exchange message is to the repair scrub.
#[derive(Clone, Copy)]
pub(crate) enum ExchangeMessage {
    /// Half of a refused call's pair: a failure of its own.
    Refused,
    /// Half of an executed cell's pair: it goes with the cell's step.
    Executed,
}

/// Chronological indices of failed cells a later success has made obsolete,
/// together with everything written to repair them.
///
/// A failed cell has to stay visible for the turn that repairs it — the model
/// cannot fix a program it cannot see, and stripping it immediately is the
/// classic way to get the same mistake twice. What it must not do is stay
/// forever. Once a subsequent cell runs clean, the failure and its feedback are
/// a worked-and-discarded branch, and a model re-reading its own dead ends
/// re-attempts them: the transcript reads as if the broken approach were still
/// live context rather than a closed question.
///
/// "Later" is scoped to the run of cells between turn boundaries. A user or
/// event message opens new work, so a success after it repairs nothing that
/// came before — those failures stay, because the model may still need them.
///
/// The scrub covers three entry kinds because a cell is stored as several
/// entries: the trajectory entry itself, the assistant prose folded into it,
/// and the protocol feedback message written after it. Dropping only the
/// trajectory entry would leave its prose to fold into the *next* cell and its
/// repair instruction pointing at nothing. Feedback is identified by
/// *provenance*, never by role or position: only this plugin's own output is
/// the plugin's to delete.
///
/// The native channel names its captured provider exchange messages through
/// `exchange`. A refused call's pair is a failure of its own and goes whole;
/// an exchange's result message is not a turn boundary and goes with its
/// call; an executed cell's exchange goes with the cell's step, so removing
/// that step removes the whole exchange.
#[expect(
    clippy::expect_used,
    reason = "the caller validated every event with the fallible suppression pass"
)]
pub(crate) fn superseded_failure_indices(
    dialect: &SessionDialect,
    events: &[lash_core::SessionHistoryRecord],
    turn_messages: &lash_core::facade_support::MessageSequence,
    exchange: impl Fn(usize) -> Option<ExchangeMessage>,
) -> HashSet<usize> {
    let mut superseded = HashSet::new();
    // Entries belonging to failures not yet repaired, oldest first.
    let mut pending_failure_entries: Vec<usize> = Vec::new();
    // Prose entries seen since the last trajectory entry: they belong to
    // whichever cell comes next, and only join the pending set if it failed.
    let mut prose_entries: Vec<usize> = Vec::new();
    let mut any_failure_pending = false;

    lash_core::facade_support::visit_turn_view(events, turn_messages, |entry| {
        match entry.payload {
            BorrowedChronologicalPayload::Message(message) => match message.role {
                lash_core::MessageRole::User if exchange(entry.index).is_some() => {
                    if any_failure_pending {
                        pending_failure_entries.push(entry.index);
                    }
                }
                lash_core::MessageRole::User | lash_core::MessageRole::Event => {
                    pending_failure_entries.clear();
                    prose_entries.clear();
                    any_failure_pending = false;
                }
                lash_core::MessageRole::Assistant => match exchange(entry.index) {
                    Some(ExchangeMessage::Refused) => {
                        pending_failure_entries.push(entry.index);
                        any_failure_pending = true;
                    }
                    Some(ExchangeMessage::Executed) => {}
                    None => prose_entries.push(entry.index),
                },
                // Protocol feedback is repair instruction for the failure it
                // follows; it goes when the failure goes. Identity, not role:
                // `System` is a shared channel, and the host puts its own
                // messages on it — a plugin directive enqueued at a mid-turn
                // checkpoint lands here too. Scrubbing by role would delete a
                // policy reminder the host injected between a failed cell and
                // the next good one, which is not ours to delete and not
                // recoverable from anywhere.
                lash_core::MessageRole::System => {
                    if any_failure_pending
                        && crate::projection::is_rlm_protocol_output(message.origin)
                    {
                        pending_failure_entries.push(entry.index);
                    }
                }
            },
            BorrowedChronologicalPayload::ProtocolEvent(event) => {
                if let Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step)) = dialect
                    .decode_history_event(event)
                    .expect("the suppression pass validated every RLM event")
                    .as_deref()
                {
                    if step.result.is_failed() {
                        pending_failure_entries.append(&mut prose_entries);
                        pending_failure_entries.push(entry.index);
                        any_failure_pending = true;
                    } else {
                        prose_entries.clear();
                        superseded.extend(pending_failure_entries.drain(..));
                        any_failure_pending = false;
                    }
                }
            }
        }
    });

    superseded
}

/// Carries nothing for empty prose with no images.
pub(crate) fn flush_pending_prose(
    messages: &mut Vec<LlmMessage>,
    pending: &mut Option<PendingProse>,
) {
    if let Some(prose) = pending.take() {
        if prose.text.trim().is_empty()
            && prose.reasoning_blocks.is_empty()
            && prose.image_blocks.is_empty()
        {
            return;
        }
        let mut blocks = prose.reasoning_blocks;
        if !prose.text.trim().is_empty() {
            blocks.push(text_block(prose.text, false));
        }
        blocks.extend(prose.image_blocks);
        messages.push(LlmMessage::new(LlmRole::Assistant, blocks));
    }
}

/// The current iteration's history prefix: its number only.
/// All instruction text is supplied by prompt sections (ADR 0133).
fn append_current_iteration_message(messages: &mut Vec<LlmMessage>, protocol_iteration: usize) {
    let current_prompt = format!("\n\n\n=== CURRENT ITERATION: {protocol_iteration} ===");
    messages.push(LlmMessage::new(
        LlmRole::User,
        vec![text_block(current_prompt, false)],
    ));
}

fn text_block(text: impl Into<Arc<str>>, cache_breakpoint: bool) -> LlmContentBlock {
    LlmContentBlock::Text {
        text: text.into(),
        response_meta: None,
        cache_breakpoint,
    }
}

fn mark_last_history_text_cache_breakpoint(messages: &mut [LlmMessage]) {
    for message in messages.iter_mut().rev() {
        let Some(blocks) = Arc::get_mut(&mut message.blocks) else {
            continue;
        };
        for block in blocks.iter_mut().rev() {
            if let LlmContentBlock::Text {
                text,
                cache_breakpoint,
                ..
            } = block
                && !text.trim().is_empty()
            {
                *cache_breakpoint = true;
                return;
            }
        }
    }
}

fn append_borrowed_entry_image_blocks(
    entry: BorrowedChronologicalEntry<'_>,
    blocks: &mut Vec<LlmContentBlock>,
) {
    if let BorrowedChronologicalPayload::Message(message) = entry.payload {
        for source in message.parts.iter().flat_map(|part| part.attachments()) {
            blocks.push(LlmContentBlock::Attachment {
                reference: Box::new(source.clone()),
            });
        }
    }
}

pub(crate) fn append_step_image_blocks(
    step: &lash_core::CellRecord,
    blocks: &mut Vec<LlmContentBlock>,
) {
    for image in &step.images {
        blocks.push(LlmContentBlock::Attachment {
            reference: Box::new(image.clone()),
        });
    }
}

/// Verbatim content for a plain (non-step) message: a bounded preview that
/// states the cut, and the attachment listing.
fn message_text(
    content: &str,
    attachments: &[RlmAttachmentRef],
    max_output_chars: usize,
) -> String {
    let (preview, raw_len) = crate::head_tail_truncate(content, max_output_chars);
    let mut out = preview.to_string();
    if raw_len > max_output_chars {
        let _ = write!(
            out,
            "\n\n(cut: {max_output_chars} of {raw_len} characters shown)"
        );
    }
    if !attachments.is_empty() {
        out.push_str("\n\nAttachments:");
        for attachment in attachments {
            let rendered = serde_json::to_string(attachment)
                .unwrap_or_else(|_| "{\"error\":\"unrenderable attachment\"}".to_string());
            let _ = write!(out, "\n- {rendered}");
        }
    }
    out
}

/// The user observation message for a step: printed outputs, images,
/// executed calls (with ids and result summaries after failure), what the cell did to the session's
/// variables, error, and final value, each under a plain header. Never empty.
pub(crate) fn step_output_text(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    entry: &lash_core::CellRecord,
    presentation: crate::RlmPresentationConfig,
) -> String {
    let mut out = String::new();
    if !entry.prints.is_empty() {
        out.push_str("Output:");
        for item in &entry.prints {
            out.push('\n');
            out.push_str(&item.text);
        }
    }
    if let Some(archive) = &entry.prints_retained {
        let _ = write!(
            out,
            "Output:\n{}\n{}",
            archive.output.witness,
            retained_value_shape(archive),
        );
    }
    if !entry.images.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Images:");
        for image in entry.images.iter().map(RlmImageRef::from_attachment) {
            let rendered = serde_json::to_string(&image)
                .unwrap_or_else(|_| "{\"error\":\"unrenderable image\"}".to_string());
            let _ = write!(out, "\n- {rendered}");
        }
    }
    if entry.result.is_failed() && !entry.calls.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Calls:\n");
        crate::call_feedback::write_calls_block(
            &mut out,
            &entry.calls,
            presentation.prompt_max_tool_calls,
            presentation.call_feedback,
        );
    } else if !entry.result.is_failed() {
        let omitted = entry
            .calls
            .len()
            .saturating_sub(presentation.prompt_max_tool_calls);
        let calls = &entry.calls[omitted..];
        if !calls.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("Calls:");
            if omitted > 0 {
                let _ = write!(out, "\n- … {omitted} earlier executed calls omitted");
            }
            for call in calls {
                let _ = write!(out, "\n- {} → {}", call.operation, call.outcome.as_str());
            }
        }
    }
    if !entry.bindings.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Session variables:");
        let not_carried = format!(
            "not kept (a function or a task does not outlive its {})",
            vocabulary.cell_noun
        );
        for (change, names) in [
            ("added", &entry.bindings.added),
            ("changed", &entry.bindings.changed),
            ("removed", &entry.bindings.removed),
            (not_carried.as_str(), &entry.bindings.not_carried),
        ] {
            if !names.is_empty() {
                let _ = write!(out, "\n- {change}: {}", names.join(", "));
            }
        }
        if entry.bindings.omitted > 0 {
            let _ = write!(out, "\n- … {} more names omitted", entry.bindings.omitted);
        }
    }
    match &entry.result {
        // The entry records the typed failure; its recovery guidance is
        // prompt text, rendered here in this dialect's words.
        lash_core::CellOutcome::Failed(failure) => {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&crate::feedback::render(failure, vocabulary.cell_noun));
            if !entry.calls.is_empty() {
                out.push_str("\n\n");
                out.push_str(crate::call_feedback::FAILED_CELL_CALLS_GUIDANCE);
            }
        }
        lash_core::CellOutcome::Controlled {
            control:
                lash_core::CellControl::Finish {
                    value: lash_core::OutputValue::Inline(final_output),
                },
            ..
        } => {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("Final output:\n");
            out.push_str(
                &serde_json::to_string_pretty(final_output)
                    .unwrap_or_else(|_| final_output.to_string()),
            );
        }
        // A final value too long for history is shown as its witness, never
        // expanded (FIG-1643).
        lash_core::CellOutcome::Controlled {
            control:
                lash_core::CellControl::Finish {
                    value: lash_core::OutputValue::Retained(retained),
                },
            ..
        } => {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("Final output:\n");
            out.push_str(&retained.output.witness);
            out.push('\n');
            out.push_str(&retained_value_shape(retained));
        }
        lash_core::CellOutcome::Controlled {
            control: lash_core::CellControl::SwitchAgentFrame { .. },
            ..
        }
        | lash_core::CellOutcome::Completed => {}
    }
    if out.is_empty() {
        out.push_str("(no printed output)");
    }
    out
}

/// What a value retained out of history holds: its shape beside the witness.
fn retained_value_shape(retained: &lash_core::RetainedValue) -> String {
    let shape = serde_json::to_string(&retained.shape)
        .unwrap_or_else(|_| "{\"error\":\"unrenderable shape\"}".to_string());
    format!("Shape: {shape}")
}

fn message_attachment_refs(parts: &[lash_core::Part]) -> Vec<RlmAttachmentRef> {
    parts
        .iter()
        .flat_map(|part| {
            part.identified_attachments()
                .into_iter()
                .map(|(id, attachment)| RlmAttachmentRef {
                    id,
                    reference: attachment.clone(),
                })
        })
        .collect()
}

fn message_history_text_parts(parts: &[lash_core::Part]) -> String {
    let chunks = parts
        .iter()
        .filter(|part| {
            matches!(
                part.kind(),
                lash_core::PartKind::Text | lash_core::PartKind::Prose
            )
        })
        .filter_map(|part| part.text_content())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    chunks.join("\n\n")
}

fn message_history_reasoning_blocks(parts: &[lash_core::Part]) -> Vec<LlmContentBlock> {
    // Display-only reasoning without provider replay metadata remains durable
    // for hosts, but is deliberately not rendered back to providers. Only
    // replay-capable reasoning can safely cross the provider history seam.
    parts
        .iter()
        .filter_map(|part| {
            if !matches!(part.kind(), lash_core::PartKind::Reasoning) {
                return None;
            }
            let replay = part.reasoning_meta()?;
            (!replay.is_empty()).then(|| LlmContentBlock::Reasoning {
                text: part.content().to_string(),
                replay: Some(replay.clone()),
            })
        })
        .collect()
}
