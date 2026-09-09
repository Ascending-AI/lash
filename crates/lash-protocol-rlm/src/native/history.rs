use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use lash_core::llm::types::{AttachmentSource, LlmContentBlock, LlmMessage, LlmRole};
use lash_core::{
    facade_support::BorrowedChronologicalEntry, facade_support::BorrowedChronologicalPayload,
};
use lash_rlm_types::RlmAttachmentRef;

use crate::dialect::RlmDialect;
use crate::projection::{decode_rlm_protocol_event, rlm_history_projection};

pub(super) struct RlmHistoryRenderInput<'a> {
    pub(super) images: bool,
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
    pub(super) history_type: &'static str,
    pub(super) images: bool,
    pub(super) history_len: usize,
    pub(super) history_has_structure: bool,
    pub(super) protocol_iteration: usize,
    pub(super) turn_causes: &'a [lash_core::TurnCause],
    pub(super) finalization: &'a str,
    pub(super) required_output: Option<&'a str>,
    pub(super) final_answer_format: Option<&'a str>,
    pub(super) budget_suffix: Option<&'a str>,
    pub(super) bound_variables: &'a str,
}

/// Standalone assistant prose buffered until the next chronological boundary.
struct PendingProse {
    text: String,
    reasoning_blocks: Vec<LlmContentBlock>,
    image_blocks: Vec<LlmContentBlock>,
}

pub(super) fn build_rlm_history_messages_from_turn(
    input: RlmHistoryRenderInput<'_>,
) -> Vec<LlmMessage> {
    let mut messages = render_history_messages(&input);
    let saw_history = !messages.is_empty();
    let history = rlm_history_projection(
        &lash_core::facade_support::ChronologicalProjection::from_turn_view(
            input.events,
            input.turn_messages,
        ),
    );
    let history_len = history.len();
    let history_has_structure = history.history().iter().any(|item| match item {
        lash_rlm_types::RlmHistoryItem::LashlangStep { .. } => true,
        lash_rlm_types::RlmHistoryItem::Message { attachments, .. } => !attachments.is_empty(),
    });
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
            history_type: if input.dialect.language_id() == "typescript" {
                "HistoryItem[]"
            } else {
                "list[HistoryItem]"
            },
            images: input.images,
            history_len,
            history_has_structure,
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
/// assistant tool-call message + matching tool results, projected atomically.
pub(super) fn render_history_messages(input: &RlmHistoryRenderInput<'_>) -> Vec<LlmMessage> {
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
                // Keep standalone assistant prose in chronological order.
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
                let repair = match super::transport::repair_parts(event) {
                    Ok(repair) => repair,
                    Err(error) => {
                        flush_pending_prose(&mut messages, &mut pending);
                        append_decode_failure(&mut messages, error);
                        return;
                    }
                };
                if let Some((parts, repair)) = repair {
                    flush_pending_prose(&mut messages, &mut pending);
                    super::transport::append_pair(&mut messages, &parts, &repair);
                    return;
                }
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
                flush_pending_prose(&mut messages, &mut pending);
                let observation = crate::driver::history::step_output_text(
                    input.dialect.prompt_vocabulary(),
                    history_projection
                        .projected_index_for_chronological(entry.index)
                        .unwrap_or(entry.index),
                    &step,
                );
                let parts = match super::transport::execution_parts(input.events, &step.id) {
                    Ok(parts) => parts,
                    Err(error) => {
                        append_decode_failure(&mut messages, error);
                        return;
                    }
                };
                if let Some(parts) = parts {
                    super::transport::append_pair(&mut messages, &parts, &observation);
                } else {
                    // Frame seeds carry semantic history, not authority to mint
                    // provider calls. Keep the facts visible as user context.
                    messages.push(LlmMessage::text(
                        LlmRole::User,
                        format!("Earlier program:\n{}\n\n{observation}", step.code),
                    ));
                }
                if let Some(message) = messages.last_mut() {
                    append_borrowed_entry_image_blocks(entry, Arc::make_mut(&mut message.blocks));
                }
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
                messages.push(LlmMessage::new(role, blocks));
            }
        }
    });
    flush_pending_prose(&mut messages, &mut pending);
    messages
}

/// Failed semantic steps and repair envelopes superseded by a later success
/// in the same turn. Execution envelopes render only at their semantic step,
/// so removing that step removes its entire provider exchange atomically.
/// Host-authored system messages are preserved by provenance.
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
                if matches!(super::transport::repair_parts(event), Ok(Some(_))) {
                    pending_failure_entries.push(entry.index);
                    any_failure_pending = true;
                }
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
    ) || matches!(
        message.origin,
        Some(lash_core::MessageOrigin::TurnOutput {
            source: lash_core::TurnOutputSource::Plugin { plugin_id },
            ..
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
    let _ = write!(
        current_prompt,
        "- `history`: `{}`, read-only, {} {}",
        input.history_type,
        input.history_len,
        if input.history_len == 1 {
            "entry"
        } else {
            "entries"
        }
    );
    if input.history_has_structure {
        current_prompt.push_str("\n\nSchema:\n");
        current_prompt
            .push_str(&crate::rlm_support::history_item_type_definition(input.images).join("\n"));
    }
    if !input.bound_variables.is_empty() {
        current_prompt.push_str("\n\n");
        current_prompt.push_str(input.bound_variables);
    }
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
    blocks: &mut Vec<LlmContentBlock>,
) {
    match entry.payload {
        BorrowedChronologicalPayload::Message(message) => {
            for part in message.parts {
                let Some(attachment) = part.attachment.as_ref() else {
                    continue;
                };
                blocks.push(LlmContentBlock::Attachment {
                    source: Box::new(attachment.source.clone()),
                });
            }
        }
        BorrowedChronologicalPayload::ProtocolEvent(event) => {
            if let Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(entry)) =
                decode_rlm_protocol_event(event)
            {
                for image in &entry.images {
                    let source = AttachmentSource::stored(image.clone());
                    blocks.push(LlmContentBlock::Attachment {
                        source: Box::new(source),
                    });
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

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;

fn append_decode_failure(messages: &mut Vec<LlmMessage>, error: super::transport::DecodeError) {
    let binding = super::transport::degraded_binding(error);
    messages.push(LlmMessage::text(
        LlmRole::User,
        format!(
            "Projection rehydration degraded binding `{}`: {}",
            binding.name, binding.reason
        ),
    ));
}

#[cfg(test)]
mod finalization_contract {
    use super::*;
    #[test]
    fn every_native_round_has_one_finalization_policy() {
        for typescript in [false, true] {
            let surface = lash_lashlang_runtime::LashlangSurface::default();
            let dialect: Box<dyn RlmDialect> = if typescript {
                Box::new(crate::dialect::typescript::TypescriptDialect::prompt_only(
                    surface,
                ))
            } else {
                Box::new(crate::dialect::lashlang::LashlangDialect::prompt_only(
                    surface,
                ))
            };
            for protocol_iteration in [1, 2, 8] {
                let messages = build_rlm_history_messages_from_turn(RlmHistoryRenderInput {
                    images: false,
                    dialect: dialect.as_ref(),
                    events: &[],
                    turn_messages: &Default::default(),
                    turn_causes: &[],
                    max_output_chars: 1000,
                    protocol_iteration,
                    finalization: "finish-policy",
                    required_output: None,
                    final_answer_format: None,
                    budget_suffix: None,
                    bound_variables: "",
                });
                let text = format!("{messages:?}");
                assert_eq!(text.matches("=== FINALIZATION ===").count(), 1);
                assert_eq!(text.matches("finish-policy").count(), 1);
                assert!(text.contains(if typescript {
                    "HistoryItem[]"
                } else {
                    "list[HistoryItem]"
                }));
            }
        }
    }
}
