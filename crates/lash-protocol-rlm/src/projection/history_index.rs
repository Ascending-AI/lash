//! History positions and provenance, without materializing history values.
use std::collections::{BTreeMap, BTreeSet};

use lash_core::facade_support::{
    BorrowedChronologicalEntry, BorrowedChronologicalMessage, BorrowedChronologicalPayload,
    ChronologicalPayload, ChronologicalProjection, MessageSequence, visit_turn_view,
};
use lash_core::{MessageRole, PartKind, SessionHistoryRecord, StoredDataCorruption};
use lash_rlm_types::RlmProtocolEvent;

use super::EventCache;

#[derive(Default)]
pub(crate) struct HistoryIndex {
    indices: BTreeMap<usize, usize>,
    suppressed: BTreeSet<usize>,
    pub(crate) provenance: BTreeMap<String, String>,
    pub(crate) has_schema: bool,
}

impl HistoryIndex {
    pub(crate) fn from_turn(
        events: &[SessionHistoryRecord],
        messages: &MessageSequence,
        decoder: &EventCache,
    ) -> Result<Self, StoredDataCorruption> {
        let mut entries = Vec::new();
        visit_turn_view(events, messages, |entry| entries.push(entry));
        Self::from_entries(&entries, decoder)
    }

    pub(crate) fn from_context(
        context: &lash_core::RuntimeExecutionContext<'_>,
        decoder: &EventCache,
    ) -> Result<Self, StoredDataCorruption> {
        let mut entries = Vec::new();
        context.visit_chronological(|entry| entries.push(entry));
        Self::from_entries(&entries, decoder)
    }

    pub(crate) fn from_projection(
        projection: &ChronologicalProjection,
        decoder: &EventCache,
    ) -> Result<Self, StoredDataCorruption> {
        Self::from_owned_entries(projection.entries(), decoder)
    }

    pub(crate) fn from_owned_entries(
        source: &[lash_core::facade_support::ChronologicalEntry],
        decoder: &EventCache,
    ) -> Result<Self, StoredDataCorruption> {
        let entries = source
            .iter()
            .map(|entry| BorrowedChronologicalEntry {
                index: entry.index,
                payload: match &entry.payload {
                    ChronologicalPayload::Message(message) => {
                        BorrowedChronologicalPayload::Message(BorrowedChronologicalMessage {
                            id: &message.id,
                            role: message.role,
                            parts: &message.parts,
                            origin: message.origin.as_ref(),
                            reply_marker: message.reply_marker.as_ref(),
                        })
                    }
                    ChronologicalPayload::ProtocolEvent(event) => {
                        BorrowedChronologicalPayload::ProtocolEvent(event)
                    }
                },
            })
            .collect::<Vec<_>>();
        Self::from_entries(&entries, decoder)
    }

    fn from_entries(
        entries: &[BorrowedChronologicalEntry<'_>],
        decoder: &EventCache,
    ) -> Result<Self, StoredDataCorruption> {
        let mut index = Self::default();
        let mut prose = Vec::new();
        let mut terminal = None;
        let mut items = Vec::new();
        // Validate in chronological order before suppressing anything, as
        // the full value projection does. A suppressed corrupt event refuses.
        for entry in entries {
            match entry.payload {
                BorrowedChronologicalPayload::Message(message) => {
                    let exchange = crate::native::transport::is_exchange_message(
                        message.origin,
                        message.parts,
                    );
                    let has_attachments = message
                        .parts
                        .iter()
                        .any(|part| !part.identified_attachments().is_empty());
                    let has_text = message.parts.iter().any(|part| {
                        matches!(part.kind(), PartKind::Text | PartKind::Prose)
                            && part
                                .text_content()
                                .is_some_and(|text| !text.trim().is_empty())
                    });
                    let is_item = !exchange && (has_text || has_attachments);
                    if is_item {
                        items.push((entry.index, has_attachments));
                    }
                    match message.role {
                        MessageRole::User if exchange => {}
                        MessageRole::User | MessageRole::Event => {
                            prose.clear();
                            terminal = None;
                        }
                        MessageRole::Assistant => {
                            if super::is_rlm_protocol_output(message.origin) {
                                prose.push(entry.index);
                            } else if is_item {
                                if let Some(step) = terminal.take() {
                                    index.suppressed.insert(step);
                                }
                                index.suppressed.extend(prose.drain(..));
                            }
                        }
                        MessageRole::System => {}
                    }
                }
                BorrowedChronologicalPayload::ProtocolEvent(event) => {
                    match decoder.decode(event)?.as_deref() {
                        Some(RlmProtocolEvent::RlmAssistantContent(_)) => {
                            prose.push(entry.index);
                            items.push((entry.index, false));
                        }
                        Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                            terminal = step
                                .result
                                .finish()
                                .filter(|value| {
                                    value.retained().is_none() && step.prints_retained.is_none()
                                })
                                .map(|_| entry.index);
                            items.push((entry.index, true));
                            for name in step
                                .bindings
                                .added
                                .iter()
                                .chain(&step.bindings.changed)
                                .chain(&step.bindings.not_carried)
                            {
                                index.provenance.insert(name.clone(), step.id.clone());
                            }
                            for name in &step.bindings.removed {
                                index.provenance.remove(name);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        for (source, schema) in items {
            if !index.suppressed.contains(&source) {
                index.indices.insert(source, index.indices.len());
                index.has_schema |= schema;
            }
        }
        Ok(index)
    }

    pub(crate) fn len(&self) -> usize {
        self.indices.len()
    }
    pub(crate) fn projected_index_for_chronological(&self, index: usize) -> Option<usize> {
        self.indices.get(&index).copied()
    }
    pub(crate) fn suppresses_chronological(&self, index: usize) -> bool {
        self.suppressed.contains(&index)
    }
}
