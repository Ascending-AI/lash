//! The attachment-omission history policy (ADR 0133): the one way a plugin
//! narrows the history a turn's model request projects.
//!
//! A policy names attachment parts of the projected history by their
//! existing identity. Core replaces each named attachment with
//! [`OMITTED_ATTACHMENT_PLACEHOLDER`] in the request's view and nothing
//! else: a policy cannot add text, change a role, drop or reorder a
//! message, or touch a part that carries no attachment. The stored history
//! is never changed, and every tool call keeps its result.

use std::collections::BTreeSet;

use super::{PluginError, PluginTraceEmitter, SessionReadView};
use crate::{Message, Part, PartKind, SessionId};

/// The text an omitted attachment's slot carries in the request.
pub const OMITTED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment omitted from older context]";

/// One part of the projected history, by the id of its message and its
/// index within that message.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistoryPartId {
    pub message: String,
    pub part: usize,
}

/// What an attachment-omission policy reads: the turn's committed state and
/// the session's committed usage.
#[derive(Clone)]
pub struct AttachmentOmissionContext {
    pub session_id: SessionId,
    /// The running run's admitted plugin configuration.
    pub plugin_config: super::AdmittedPluginConfig,
    pub state: SessionReadView,
    pub prompt_usage: Option<crate::LlmUsage>,
    pub max_context_tokens: Option<usize>,
    pub traces: PluginTraceEmitter,
    /// The trace context of the turn the policy decides for.
    pub trace_context: lash_trace::TraceContext,
}

/// Decides which attachments of a turn's projected history its model
/// request omits. It returns decisions only.
pub trait AttachmentOmissionPolicy: Send + Sync {
    fn id(&self) -> &'static str;

    /// The parts of `history` whose attachments the request omits.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the policy cannot decide; the turn fails.
    fn omissions(
        &self,
        ctx: &AttachmentOmissionContext,
        history: &[Message],
    ) -> Result<BTreeSet<HistoryPartId>, PluginError>;
}

/// Replace `part`'s attachments with `placeholder`: an attachment part keeps
/// its slot, empty, with the placeholder as its text, and a tool result's
/// attachment blocks become placeholder text in place, so the result stays
/// one part in its original order. Whether anything was replaced.
pub fn omit_part_attachments(part: &mut Part, placeholder: &str) -> bool {
    if let Some(blocks) = part.tool_result_content_mut() {
        let mut changed = false;
        for block in blocks.iter_mut() {
            if block.attachment().is_some() {
                *block = crate::ModelToolReturnPart::text(placeholder);
                changed = true;
            }
        }
        return changed;
    }
    if !matches!(part.kind(), PartKind::Attachment) || part.attachment().is_none() {
        return false;
    }
    if let Some(slot) = part.attachment_mut() {
        *slot = None;
    }
    if let Some(content) = part.content_mut() {
        *content = placeholder.to_string();
    }
    true
}

/// Apply `omissions` to the request's view of `messages`. A named part that
/// carries no attachment, or names no part, is left alone. The count of
/// parts whose attachments were omitted.
pub fn apply_attachment_omissions(
    messages: &mut [Message],
    omissions: &BTreeSet<HistoryPartId>,
) -> usize {
    if omissions.is_empty() {
        return 0;
    }
    let mut omitted = 0;
    for message in messages {
        let named = omissions
            .iter()
            .filter(|id| id.message == message.id)
            .map(|id| id.part)
            .collect::<Vec<_>>();
        if named.is_empty() {
            continue;
        }
        let parts = std::sync::Arc::make_mut(&mut message.parts);
        for index in named {
            if let Some(part) = parts.get_mut(index) {
                omitted += usize::from(omit_part_attachments(part, OMITTED_ATTACHMENT_PLACEHOLDER));
            }
        }
    }
    omitted
}

#[cfg(test)]
#[path = "attachment_omission_tests.rs"]
mod tests;
