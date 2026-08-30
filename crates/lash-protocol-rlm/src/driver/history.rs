//! RLM history rendering: turns the chronological turn view into the LLM
//! message sequence the model sees each iteration.
//!
//! Contract:
//! - **History == emission.** A prior executed step renders as an `Assistant`
//!   message holding the canonical cell `{prose}\n<lashlang>\n{code}\n</lashlang>`
//!   (`render_lashlang_cell_text`), followed by a `User` message holding that
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
//!   omitted; intermediate trajectory entries remain available. This is
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
//!   input and the volatile `=== CURRENT ITERATION ===` tail — iteration number,
//!   turn events, bound variables, finalization, required-output schema, context
//!   budget — are appended uncached.
//! - **Re-fetch handle.** A lossy-projected step output is tagged
//!   `full: history[N].output[M]` on its user message; the `history` projected
//!   binding (`projection/context.rs`) carries the full untruncated value, keyed
//!   by the step's `entry.index`, so the model can recover it by re-printing the
//!   reference. Proven by
//!   `projection::context::tests::history_step_output_resolves_full_untruncated_value`.
//!   `history[N]` uses compact canonical semantic indices, so omitted internal
//!   entries consume no index and rendered re-fetch handles use the remap.
//! - **Variables.** The live variable namespace is rendered into the volatile
//!   current-iteration tail. It is deliberately outside the stable system and
//!   history prefix while remaining adjacent to the work it describes.

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use lash_core::llm::types::{AttachmentSource, LlmContentBlock, LlmMessage, LlmRole};
use lash_core::{
    facade_support::BorrowedChronologicalEntry, facade_support::BorrowedChronologicalPayload,
};
use lash_rlm_types::{RlmAttachmentRef, RlmImageRef};
use lashlang::{Value as FlowValue, ValueProjectionContext};

use crate::dialect::RlmDialect;
use crate::projection::{decode_rlm_protocol_event, json_to_flow_value, rlm_history_projection};

pub(super) struct RlmHistoryRenderInput<'a> {
    pub(super) dialect: &'a dyn RlmDialect,
    pub(super) events: &'a [lash_core::SessionHistoryRecord],
    pub(super) turn_messages: &'a lash_core::facade_support::MessageSequence,
    pub(super) turn_causes: &'a [lash_core::TurnCause],
    pub(super) max_output_chars: usize,
    pub(super) protocol_iteration: usize,
    pub(super) finalization: &'a str,
    pub(super) required_output: Option<&'a str>,
    pub(super) final_answer_format: Option<&'a str>,
    pub(super) budget_suffix: Option<&'a str>,
    pub(super) bound_variables: &'a str,
}

#[derive(Clone, Copy)]
pub(super) struct CurrentIterationMessageInput<'a> {
    pub(super) history_len: usize,
    pub(super) protocol_iteration: usize,
    pub(super) turn_causes: &'a [lash_core::TurnCause],
    pub(super) finalization: &'a str,
    pub(super) required_output: Option<&'a str>,
    pub(super) final_answer_format: Option<&'a str>,
    pub(super) budget_suffix: Option<&'a str>,
    pub(super) bound_variables: &'a str,
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
    attachments: &mut Vec<AttachmentSource>,
) -> Vec<LlmMessage> {
    let mut messages = render_history_messages(&input, attachments);
    let saw_history = !messages.is_empty();
    let history_len = rlm_history_projection(
        &lash_core::facade_support::ChronologicalProjection::from_turn_view(
            input.events,
            input.turn_messages,
        ),
    )
    .len();
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
            history_len,
            protocol_iteration: input.protocol_iteration,
            turn_causes: input.turn_causes,
            finalization: input.finalization,
            required_output: input.required_output,
            final_answer_format: input.final_answer_format,
            budget_suffix: input.budget_suffix,
            bound_variables: input.bound_variables,
        },
    );
    messages
}

/// The history portion only (no current-iteration tail): each prior step as an
/// assistant cell message + a user observation message, with prose folded in.
pub(super) fn render_history_messages(
    input: &RlmHistoryRenderInput<'_>,
    attachments: &mut Vec<AttachmentSource>,
) -> Vec<LlmMessage> {
    let mut messages = Vec::new();
    let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
        input.events,
        input.turn_messages,
    );
    let history_projection = rlm_history_projection(&chronological);
    let active_cause_ids = input
        .turn_causes
        .iter()
        .map(|cause| cause.id.as_str())
        .collect::<HashSet<_>>();
    let mut pending: Option<PendingProse> = None;
    let superseded = superseded_failure_indices(input.events, input.turn_messages);

    lash_core::facade_support::visit_turn_view(input.events, input.turn_messages, |entry| {
        if borrowed_entry_is_active_cause(entry, &active_cause_ids) {
            return;
        }
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
                append_borrowed_entry_image_blocks(entry, attachments, &mut image_blocks);
                pending = Some(PendingProse {
                    text: message_history_text_parts(message.parts),
                    reasoning_blocks: message_history_reasoning_blocks(message.parts),
                    image_blocks,
                });
            }
            BorrowedChronologicalPayload::ProtocolEvent(event) => {
                let Some(event) = decode_rlm_protocol_event(event) else {
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
                let image_refs = step
                    .images
                    .iter()
                    .map(|image| {
                        let (width, height) = image_dimensions(image);
                        RlmImageRef {
                            id: image.id.to_string(),
                            media_type: image.media_type.clone(),
                            width,
                            height,
                            bytes: image.byte_len as usize,
                            label: image.label.clone(),
                        }
                    })
                    .collect::<Vec<_>>();
                let obs_text = step_output_text(StepOutputInput {
                    vocabulary: input.dialect.prompt_vocabulary(),
                    index: history_projection
                        .projected_index_for_chronological(entry.index)
                        .unwrap_or(entry.index),
                    output: &step.output,
                    images: &image_refs,
                    calls: &step.calls,
                    calls_omitted: step.calls_omitted,
                    error: step.error.as_deref(),
                    final_output: step.final_output.as_ref(),
                });
                let mut obs_blocks = vec![text_block(obs_text, false)];
                append_borrowed_entry_image_blocks(entry, attachments, &mut obs_blocks);
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
                append_borrowed_entry_image_blocks(entry, attachments, &mut blocks);
                let role = match message.role {
                    lash_core::MessageRole::User | lash_core::MessageRole::Event => LlmRole::User,
                    lash_core::MessageRole::System => LlmRole::System,
                    lash_core::MessageRole::Assistant => LlmRole::Assistant,
                };
                messages.push(LlmMessage::new(role, blocks));
            }
        }
    });
    flush_pending_prose(&mut messages, &mut pending);
    messages
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
/// nothing.
///
/// Every one of those four is identified by *provenance*, never by role or
/// position. Only this plugin's own output is the plugin's to delete.
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
                    if any_failure_pending && is_rlm_protocol_message(&message) {
                        pending_failure_entries.push(entry.index);
                    }
                }
            },
            BorrowedChronologicalPayload::ProtocolEvent(event) => {
                match decode_rlm_protocol_event(event) {
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(_)) => {
                        prose_entries.push(entry.index);
                    }
                    Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                        if step.error.is_some() {
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

/// Whether this message is the RLM protocol's own durable output.
///
/// The one question the scrub is allowed to ask about a `System` message. A host
/// or another plugin can write on the same channel, and their messages carry
/// their own provenance.
fn is_rlm_protocol_message(
    message: &lash_core::facade_support::BorrowedChronologicalMessage<'_>,
) -> bool {
    matches!(
        message.origin,
        Some(lash_core::MessageOrigin::Plugin {
            plugin_id,
            transient: false,
        }) if plugin_id == crate::plugin::RLM_PROTOCOL_PLUGIN_ID
    )
}

/// Emit a buffered prose as a standalone assistant message (a prose-only
/// finish). Carries nothing for empty prose with no images.
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

fn append_current_iteration_message(
    messages: &mut Vec<LlmMessage>,
    input: CurrentIterationMessageInput<'_>,
) {
    let mut current_prompt = format!(
        "\n\n\n=== CURRENT ITERATION: {} ===",
        input.protocol_iteration
    );
    if let Some(turn_events) =
        lash_core::facade_support::render_turn_causes_prompt(input.turn_causes)
    {
        current_prompt.push_str("\n\n");
        current_prompt.push_str(&turn_events);
    }
    current_prompt.push_str("\n\n\n=== BOUND VARIABLES ===\n\n");
    current_prompt.push_str(input.bound_variables);
    current_prompt.push_str("\n\nRuntime notes:\n");
    let _ = write!(
        current_prompt,
        "- `history` currently has {} {}",
        input.history_len,
        if input.history_len == 1 {
            "entry"
        } else {
            "entries"
        }
    );
    current_prompt.push_str("\n\n\n=== FINALIZATION ===\n\n");
    current_prompt.push_str(input.finalization);
    if let Some(block) = input.required_output {
        current_prompt.push_str("\n\n=== REQUIRED OUTPUT ===\n\n");
        current_prompt.push_str(block);
    }
    if let Some(guidance) = input.final_answer_format {
        current_prompt.push_str("\n\n=== FINAL ANSWER FORMAT ===\n\n");
        current_prompt.push_str(guidance);
    }
    if let Some(suffix) = input.budget_suffix {
        current_prompt.push_str("\n\n=== CONTEXT BUDGET ===\n\n");
        current_prompt.push_str(suffix);
    }
    messages.push(LlmMessage::new(
        LlmRole::User,
        vec![text_block(current_prompt, false)],
    ));
}

fn borrowed_entry_is_active_cause(
    entry: BorrowedChronologicalEntry<'_>,
    active_cause_ids: &HashSet<&str>,
) -> bool {
    matches!(
        entry.payload,
        BorrowedChronologicalPayload::Message(message)
            if matches!(message.role, lash_core::MessageRole::Event)
                && active_cause_ids.contains(message.id)
    )
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
    attachments: &mut Vec<AttachmentSource>,
    blocks: &mut Vec<LlmContentBlock>,
) {
    match entry.payload {
        BorrowedChronologicalPayload::Message(message) => {
            for part in message.parts {
                let Some(attachment) = part.attachment.as_ref() else {
                    continue;
                };
                let attachment_idx = attachments.len();
                attachments.push(attachment.source.clone());
                blocks.push(LlmContentBlock::Attachment { attachment_idx });
            }
        }
        BorrowedChronologicalPayload::ProtocolEvent(event) => {
            if let Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(entry)) =
                decode_rlm_protocol_event(event)
            {
                for image in &entry.images {
                    let attachment_idx = attachments.len();
                    attachments.push(AttachmentSource::stored(image.clone()));
                    blocks.push(LlmContentBlock::Attachment { attachment_idx });
                }
            }
        }
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

/// The user observation message for a step: printed outputs (with re-fetch
/// handles), images, executed calls, error, and final value. Calls intentionally
/// render even on success: the model pays the token cost to distinguish work
/// that ran from work that failed before dispatch. Never empty.
struct StepOutputInput<'a> {
    vocabulary: crate::dialect::DialectPromptVocabulary,
    index: usize,
    output: &'a [String],
    images: &'a [RlmImageRef],
    calls: &'a [lash_rlm_types::RlmExecutedCall],
    calls_omitted: usize,
    error: Option<&'a str>,
    final_output: Option<&'a serde_json::Value>,
}

fn step_output_text(input: StepOutputInput<'_>) -> String {
    let StepOutputInput {
        vocabulary,
        index,
        output,
        images,
        calls,
        calls_omitted,
        error,
        final_output,
    } = input;
    let mut out = String::new();
    for (output_index, item) in output.iter().enumerate() {
        let (preview, projected_lossy) = project_history_output(item);
        let raw_len = item.chars().count();
        let full_ref = projected_ref(
            vocabulary,
            projected_lossy,
            &format!("history[{index}].output[{output_index}]"),
        );
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let _ = write!(
            out,
            "history[{index}].output[{output_index}] ({raw_len} chars{full_ref}):\n{preview}"
        );
    }
    if !images.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Images:");
        for (image_index, image) in images.iter().enumerate() {
            let rendered = serde_json::to_string(image)
                .unwrap_or_else(|_| "{\"error\":\"unrenderable image\"}".to_string());
            let _ = write!(
                out,
                "\n- history[{index}].images[{image_index}]: {rendered}"
            );
        }
    }
    if !calls.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Calls:");
        if calls_omitted > 0 {
            let _ = write!(out, "\n- … {calls_omitted} earlier executed calls omitted");
        }
        for call in calls {
            let _ = write!(out, "\n- {} → {}", call.operation, call.outcome.as_str());
        }
    }
    if let Some(error) = error {
        // Two texts, not one paragraph: the evidence answers "what happened",
        // the imperative answers "what do I write next", and the tag says which
        // of the two failure shapes this is. A model that has already read the
        // diagnostic needs only the second half.
        let (kind, evidence) = crate::feedback::RlmFeedbackKind::split(error);
        if let Some(imperative) = kind.imperative(vocabulary.cell_noun) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(kind.tag());
            out.push('\n');
            out.push_str(evidence);
            out.push_str("\n\n");
            out.push_str(&imperative);
        }
    }
    if let Some(final_output) = final_output {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Final output:\n");
        out.push_str(
            &serde_json::to_string_pretty(final_output)
                .unwrap_or_else(|_| final_output.to_string()),
        );
    }
    if out.is_empty() {
        out.push_str("(no printed output)");
    }
    out
}

fn message_attachment_refs(parts: &[lash_core::Part]) -> Vec<RlmAttachmentRef> {
    parts
        .iter()
        .filter_map(|part| {
            let attachment = part.attachment.as_ref()?;
            let (media_type, label, source, reference) = attachment_summary(&attachment.source);
            Some(RlmAttachmentRef {
                id: part.id.clone(),
                media_type,
                label,
                source,
                reference,
            })
        })
        .collect()
}

fn attachment_summary(
    source: &AttachmentSource,
) -> (Option<lash_core::MediaType>, Option<String>, String, String) {
    match source {
        AttachmentSource::Inline { media_type, .. } => (
            Some(media_type.clone()),
            None,
            "inline".to_string(),
            "transient".to_string(),
        ),
        AttachmentSource::Stored { attachment_ref } => (
            Some(attachment_ref.media_type.clone()),
            attachment_ref.label.clone(),
            "stored".to_string(),
            attachment_ref.id.to_string(),
        ),
        AttachmentSource::ExternalUrl { media_type, url } => (
            Some(media_type.clone()),
            None,
            "external_url".to_string(),
            url.clone(),
        ),
        AttachmentSource::ProviderFile { id, .. } => {
            (None, None, "provider_file".to_string(), id.clone())
        }
    }
}

fn image_dimensions(image: &lash_core::AttachmentRef) -> (Option<u32>, Option<u32>) {
    match image.type_metadata.as_ref() {
        Some(lash_core::AttachmentTypeMetadata::Image { width, height }) => (*width, *height),
        None => (None, None),
    }
}

fn message_history_text_parts(parts: &[lash_core::Part]) -> String {
    let chunks = parts
        .iter()
        .filter(|part| {
            matches!(
                part.kind,
                lash_core::PartKind::Text | lash_core::PartKind::Prose
            )
        })
        .map(|part| part.content.trim())
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
            if !matches!(part.kind, lash_core::PartKind::Reasoning) {
                return None;
            }
            let replay = part.reasoning_meta.as_ref()?;
            (!replay.is_empty()).then(|| LlmContentBlock::Reasoning {
                text: part.content.clone(),
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

fn projected_ref(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    projected_lossy: bool,
    reference: &str,
) -> String {
    if projected_lossy {
        format!(" — {}", preview_retained_copy(vocabulary, reference))
    } else {
        String::new()
    }
}

fn project_history_output(item: &str) -> (String, bool) {
    let value = history_output_value(item);
    let projected = crate::rlm_support::print_history_projector()
        .project_blocking(ValueProjectionContext::new(&value));
    let lossy = crate::rlm_support::projection_is_lossy(item, &projected);
    (projected, lossy)
}

fn history_output_value(item: &str) -> FlowValue {
    let trimmed = item.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(item)
    {
        return json_to_flow_value(value);
    }
    FlowValue::String(item.into())
}
