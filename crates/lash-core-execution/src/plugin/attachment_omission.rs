//! The attachment-omission history policy (ADR 0133): the one way a plugin
//! narrows the history a turn's model request projects.
//!
//! A policy names attachment parts of the projected history by their
//! existing identity. Core replaces each named attachment with
//! the policy's replacement text in the request's view and nothing
//! else: a policy can choose that replacement text but cannot change a role, drop or reorder a
//! message, or touch a part that carries no attachment. The stored history
//! is never changed, and every tool call keeps its result.

use std::collections::BTreeMap;

use super::PluginTraceEmitter;
use super::SessionReadView;
use crate::{Message, SessionId};
#[cfg(test)]
use crate::{Part, PartKind};
use lash_execution_contracts::plugin_error::PluginError;

/// Part identities and plugin-authored replacement text, applied in history order.
pub type AttachmentOmissions = BTreeMap<HistoryPartId, String>;

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
/// request omits. It returns identities and their replacement text.
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
    ) -> Result<AttachmentOmissions, PluginError>;
}

pub use lash_protocol_types::session_model::message::omit_part_attachments;

/// Apply `omissions` to the request's view of `messages`. A named part that
/// carries no attachment, or names no part, is left alone. The count of
/// parts whose attachments were omitted.
pub fn apply_attachment_omissions(
    messages: &mut [Message],
    omissions: &AttachmentOmissions,
) -> Result<usize, lash_protocol_types::allocation::AllocationError> {
    if omissions.is_empty() {
        return Ok(0);
    }
    let mut omitted = 0;
    for message in messages {
        let named = || omissions.iter().filter(|(id, _)| id.message == message.id);
        if named().next().is_none() {
            continue;
        }
        omitted += lash_protocol_types::MessageParts::edit(&mut message.parts, |parts| {
            let mut count = 0;
            for (id, placeholder) in named() {
                count += usize::from(parts.omit_attachments(id.part, placeholder)?);
            }
            Ok(count)
        })?;
    }
    Ok(omitted)
}

#[cfg(test)]
#[path = "attachment_omission_tests.rs"]
mod tests;
