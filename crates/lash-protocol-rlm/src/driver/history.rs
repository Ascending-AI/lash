//! RLM history rendering: turns the chronological turn view into the LLM
//! message sequence the model sees each iteration.
//!
//! Contract:
//! - **History == emission.** A prior executed step renders as an `Assistant`
//!   message holding the canonical cell `{prose}\n{open}\n{code}\n{close}`
//!   (`render_cell_text_for_tests`), followed by a `User` message holding that
//!   step's printed output, images, error, and final value. A plain user turn
//!   renders its content verbatim as a `User` message. There is no
//!   `--- history[N] ---` meta-format: what the model sees as history is exactly
//!   the grammar it must emit, so a continuation lands in that grammar.
//! - **Folding.** A step is stored as two consecutive entries — an assistant
//!   prose `Message` then a `RlmTrajectoryEntry`. They fold into one assistant
//!   message. `visit_turn_view` is a push visitor with no lookahead, so the
//!   prose is buffered (`PendingProse`) and either folded into the next step or
//!   flushed as a standalone assistant message (a prose-only finish).
//! - **Completed-turn precedence.** When a successful terminal step is
//!   followed by a committed assistant transcript message in the same turn,
//!   the transcript is canonical for assistant prose. Assistant-content events,
//!   the terminal emission cell, and its never-observed output echo are
//!   omitted unless it holds a print archive; those values remain addressable.
//!   Intermediate trajectory entries remain available. This is
//!   derived from event/turn ordering, never message content. Without a
//!   committed transcript, the trajectory renders unchanged.
//! - **Repaired-failure scrub.** A failed cell stays in the transcript for the
//!   repair turn — that is the whole point of showing it — but once a later
//!   cell in the same turn runs clean, the failure has done its job. It, its
//!   `Error:` observation, the prose folded into it, and the protocol feedback
//!   written to repair it are omitted from every subsequent render
//!   (`superseded_failure_indices`). A model re-reading its own dead ends
//!   re-attempts them. The scrub is transcript-only: `history[N]` still carries
//!   the failed step with its error and its index, so nothing is destroyed and
//!   the re-fetch handles above stay stable.
//! - **Cache fence.** The last history message is marked with a
//!   `cache_breakpoint` (`mark_last_history_text_cache_breakpoint`) so the
//!   provider can reuse the stable history prefix across iterations. Active-turn
//!   input and the volatile iteration prefix (iteration number and turn causes)
//!   are appended uncached. The host continues that User message with the
//!   call's CurrentContext sections, outside stored history.
//! - **Re-fetch handle.** Inline `history[N].output[M]` is the complete value.
//!   Archived steps expose `history[N].output_archive.attachment`; an explicit
//!   `control.read_output` reads all of the step's typed values in order.
//!   Rendering never reads archive bytes. Proven by
//!   `projection::context::tests::history_step_output_resolves_full_untruncated_value`.
//!   `history[N]` uses compact canonical semantic indices, so omitted internal
//!   entries consume no index and rendered re-fetch handles use the remap.
//! - **Variables.** The `bound_variables` prompt section renders the built-in
//!   history binding, its schema and the live variable namespace from protocol
//!   facts. Projectors render none of those instruction lines.

#[cfg(test)]
mod tests;

use crate::dialect::SessionDialect;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_core::{
    facade_support::BorrowedChronologicalEntry, facade_support::BorrowedChronologicalPayload,
};
use lash_rlm_types::{RlmAttachmentRef, RlmImageRef};

use crate::projection::{decode_rlm_protocol_event, rlm_history_projection};

pub(super) struct RlmHistoryRenderInput<'a> {
    pub(super) dialect: &'a SessionDialect,
    pub(super) events: &'a [lash_core::SessionHistoryRecord],
    pub(super) turn_messages: &'a lash_core::facade_support::MessageSequence,
    pub(super) max_output_chars: usize,
    pub(super) protocol_iteration: usize,
}

#[derive(Clone, Copy)]
pub(super) struct CurrentIterationMessageInput {
    pub(super) protocol_iteration: usize,
}

/// Assistant prose awaiting a fold into the next lashlang step. Buffered because
/// `visit_turn_view` is a push visitor with no lookahead.
struct PendingProse {
    text: String,
    reasoning_blocks: Vec<LlmContentBlock>,
    image_blocks: Vec<LlmContentBlock>,
}

pub(super) fn build_rlm_history_messages_from_turn(
    input: RlmHistoryRenderInput<'_>,
) -> Result<Vec<LlmMessage>, lash_core::StoredDataCorruption> {
    let mut messages = render_history_messages(&input)?;
    let saw_history = !messages.is_empty();
    if !saw_history {
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
    append_current_iteration_message(
        &mut messages,
        CurrentIterationMessageInput {
            protocol_iteration: input.protocol_iteration,
        },
    );
    Ok(messages)
}

/// The history portion only (no current-iteration tail): each prior step as an
/// assistant cell message + a user observation message, with prose folded in.
#[expect(
    clippy::expect_used,
    reason = "the fallible history projection validates every event before the visitors run"
)]
pub(super) fn render_history_messages(
    input: &RlmHistoryRenderInput<'_>,
) -> Result<Vec<LlmMessage>, lash_core::StoredDataCorruption> {
    let mut messages = Vec::new();
    let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
        input.events,
        input.turn_messages,
    );
    let history_projection = rlm_history_projection(&chronological)?;
    let mut pending: Option<PendingProse> = None;
    let superseded = superseded_failure_indices(input.events, input.turn_messages);

    lash_core::facade_support::visit_turn_view(input.events, input.turn_messages, |entry| {
        if history_projection.suppresses_chronological(entry.index)
            || superseded.contains(&entry.index)
        {
            pending = None;
            return;
        }
        match entry.payload {
            BorrowedChronologicalPayload::Message(message)
                if matches!(message.role, lash_core::MessageRole::Assistant) =>
            {
                // Assistant prose: buffer to fold into the next lashlang step.
                flush_pending_prose(&mut messages, &mut pending);
                let mut image_blocks = Vec::new();
                append_borrowed_entry_image_blocks(entry, &mut image_blocks);
                pending = Some(PendingProse {
                    text: message_history_text_parts(message.parts),
                    reasoning_blocks: message_history_reasoning_blocks(message.parts),
                    image_blocks,
                });
            }
            BorrowedChronologicalPayload::ProtocolEvent(event) => {
                let Some(event) = decode_rlm_protocol_event(event)
                    .expect("history projection validated every RLM event")
                else {
                    return;
                };
                let step = match event {
                    lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content) => {
                        flush_pending_prose(&mut messages, &mut pending);
                        pending = Some(PendingProse {
                            text: content.prose,
                            reasoning_blocks: Vec::new(),
                            image_blocks: Vec::new(),
                        });
                        return;
                    }
                    lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step) => step,
                    _ => return,
                };
                // Fold buffered prose into one assistant message: prose + cell,
                // byte-identical to the model's own emission.
                let prose = pending.take();
                let prose_text = prose.as_ref().map(|p| p.text.as_str()).unwrap_or("");
                let cell = input
                    .dialect
                    .render_history_cell(prose_text, step.code.trim());
                let mut cell_blocks = prose
                    .as_ref()
                    .map(|prose| prose.reasoning_blocks.clone())
                    .unwrap_or_default();
                cell_blocks.push(text_block(cell, false));
                if let Some(prose) = prose {
                    cell_blocks.extend(prose.image_blocks);
                }
                messages.push(LlmMessage::new(LlmRole::Assistant, cell_blocks));

                // The step's printed outputs become a user observation message.
                let obs_text = step_output_text(
                    input.dialect.prompt_vocabulary(),
                    history_projection
                        .projected_index_for_chronological(entry.index)
                        .unwrap_or(entry.index),
                    &step,
                );
                let mut obs_blocks = vec![text_block(obs_text, false)];
                append_step_image_blocks(&step, &mut obs_blocks);
                messages.push(LlmMessage::new(LlmRole::User, obs_blocks));
            }
            BorrowedChronologicalPayload::Message(message) => {
                // User / system / event turn: rendered verbatim by role.
                flush_pending_prose(&mut messages, &mut pending);
                let text = message_text(
                    input.dialect.prompt_vocabulary(),
                    history_projection
                        .projected_index_for_chronological(entry.index)
                        .unwrap_or(entry.index),
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
                messages.push(projected);
            }
        }
    });
    flush_pending_prose(&mut messages, &mut pending);
    Ok(messages)
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
/// The scrub covers four entry kinds because a cell is stored as several
/// entries: the trajectory entry itself, the assistant prose folded into it,
/// the assistant-content event carrying that prose, and the protocol feedback
/// message written after it. Dropping only the trajectory entry would leave its
/// prose to fold into the *next* cell and its repair instruction pointing at
/// Every one of those four is identified by *provenance*, never by role or
/// position. Only this plugin's own output is the plugin's to delete.
#[expect(
    clippy::expect_used,
    reason = "the caller validated every event with the fallible history projection"
)]
fn superseded_failure_indices(
    events: &[lash_core::SessionHistoryRecord],
    turn_messages: &lash_core::facade_support::MessageSequence,
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
                lash_core::MessageRole::User | lash_core::MessageRole::Event => {
                    pending_failure_entries.clear();
                    prose_entries.clear();
                    any_failure_pending = false;
                }
                lash_core::MessageRole::Assistant => prose_entries.push(entry.index),
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
                match decode_rlm_protocol_event(event)
                    .expect("history projection validated every RLM event")
                {
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(_)) => {
                        prose_entries.push(entry.index);
                    }
                    Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                        if step.outcome.is_failed() {
                            pending_failure_entries.append(&mut prose_entries);
                            pending_failure_entries.push(entry.index);
                            any_failure_pending = true;
                        } else {
                            prose_entries.clear();
                            superseded.extend(pending_failure_entries.drain(..));
                            any_failure_pending = false;
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    superseded
}

/// Carries nothing for empty prose with no images.
fn flush_pending_prose(messages: &mut Vec<LlmMessage>, pending: &mut Option<PendingProse>) {
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
fn append_current_iteration_message(
    messages: &mut Vec<LlmMessage>,
    input: CurrentIterationMessageInput,
) {
    let current_prompt = format!(
        "\n\n\n=== CURRENT ITERATION: {} ===",
        input.protocol_iteration
    );
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

fn append_step_image_blocks(
    step: &lash_rlm_types::RlmTrajectoryEntry,
    blocks: &mut Vec<LlmContentBlock>,
) {
    for image in &step.images {
        blocks.push(LlmContentBlock::Attachment {
            reference: Box::new(image.clone()),
        });
    }
}

/// Verbatim content for a plain (non-step) message. No `--- history[N] ---`
/// wrapper; keeps the truncation re-fetch handle and the attachment listing.
fn message_text(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    index: usize,
    content: &str,
    attachments: &[RlmAttachmentRef],
    max_output_chars: usize,
) -> String {
    let (preview, raw_len) =
        lash_core::facade_support::head_tail_truncate(content, max_output_chars);
    let mut out = preview.to_string();
    if raw_len > max_output_chars {
        let _ = write!(
            out,
            "\n\n({})",
            preview_retained_copy(vocabulary, &format!("history[{index}].content"))
        );
    }
    if !attachments.is_empty() {
        out.push_str("\n\nAttachments:");
        for (attachment_index, attachment) in attachments.iter().enumerate() {
            let rendered = serde_json::to_string(attachment)
                .unwrap_or_else(|_| "{\"error\":\"unrenderable attachment\"}".to_string());
            let _ = write!(
                out,
                "\n- history[{index}].attachments[{attachment_index}]: {rendered}"
            );
        }
    }
    out
}

/// The user observation message for a step: printed outputs (with re-fetch handles), images,
/// executed calls, error, and final value.
/// Never empty.
pub(crate) fn step_output_text(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    index: usize,
    entry: &lash_rlm_types::RlmTrajectoryEntry,
) -> String {
    let mut out = String::new();
    for (output_index, item) in entry.output.iter().enumerate() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let _ = write!(
            out,
            "history[{index}].output[{output_index}]:\n{}",
            item.text
        );
    }
    if let Some(archive) = &entry.output_archive {
        let _ = write!(
            out,
            "{}\nFull outputs: await control.read_output({{ archive: history[{index}].output_archive.attachment }})",
            archive.witness,
        );
    }
    if !entry.images.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Images:");
        for (image_index, image) in entry
            .images
            .iter()
            .map(RlmImageRef::from_attachment)
            .enumerate()
        {
            let rendered = serde_json::to_string(&image)
                .unwrap_or_else(|_| "{\"error\":\"unrenderable image\"}".to_string());
            let _ = write!(
                out,
                "\n- history[{index}].images[{image_index}]: {rendered}"
            );
        }
    }
    if !entry.calls.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Calls:");
        if entry.calls_omitted > 0 {
            let _ = write!(
                out,
                "\n- … {} earlier executed calls omitted",
                entry.calls_omitted
            );
        }
        for call in &entry.calls {
            let _ = write!(out, "\n- {} → {}", call.operation, call.outcome.as_str());
        }
    }
    match &entry.outcome {
        // The entry records the typed failure; its recovery guidance is
        // prompt text, rendered here in this dialect's words.
        lash_rlm_types::CellOutcome::Failed(failure) => {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&crate::feedback::render(failure, vocabulary.cell_noun));
        }
        lash_rlm_types::CellOutcome::Finished(lash_core::OutputValue::Inline(final_output)) => {
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
        lash_rlm_types::CellOutcome::Finished(lash_core::OutputValue::Retained(retained)) => {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("Final output:\n");
            out.push_str(&retained.witness);
        }
        lash_rlm_types::CellOutcome::Running => {}
    }
    if out.is_empty() {
        out.push_str("(no printed output)");
    }
    out
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

/// State retention, stated explicitly: a truncated preview is display-only, the
/// full value is still live and re-readable. Models otherwise misread a short
/// preview as lost state and stop mid-task.
pub(crate) fn preview_retained_copy(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    reference: &str,
) -> String {
    format!(
        "preview only — full value retained; re-run `{}` for the rest",
        vocabulary.print_statement(reference)
    )
}
