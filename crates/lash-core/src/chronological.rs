use crate::session_model::{ConversationRecord, ProtocolEvent, SessionHistoryRecord};
use crate::{Message, MessageOrigin, MessageRole, MessageSequence, Part};
use lash_sansio::core_support::*;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ChronologicalProjection {
    entries: Vec<ChronologicalEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ChronologicalEntry {
    pub index: usize,
    pub payload: ChronologicalPayload,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// justification: this public owned projection preserves the inline session-history API and wire shape.
#[allow(clippy::large_enum_variant)]
pub enum ChronologicalPayload {
    Message(Message),
    ProtocolEvent(crate::session_model::ProtocolEvent),
}

#[derive(Clone, Copy, Debug)]
pub struct BorrowedChronologicalEntry<'a> {
    pub index: usize,
    pub payload: BorrowedChronologicalPayload<'a>,
}

#[derive(Clone, Copy, Debug)]
pub enum BorrowedChronologicalPayload<'a> {
    Message(BorrowedChronologicalMessage<'a>),
    ProtocolEvent(&'a ProtocolEvent),
}

#[derive(Clone, Copy, Debug)]
pub struct BorrowedChronologicalMessage<'a> {
    pub id: &'a str,
    pub role: MessageRole,
    pub parts: &'a [Part],
    pub origin: Option<&'a MessageOrigin>,
}

impl<'a> BorrowedChronologicalMessage<'a> {
    fn from_message(message: &'a Message) -> Self {
        Self {
            id: &message.id,
            role: message.role,
            parts: message.parts.as_slice(),
            origin: message.origin.as_ref(),
        }
    }

    fn from_record(record: &'a ConversationRecord) -> Self {
        Self {
            id: &record.id,
            role: record.role,
            parts: record.parts.as_slice(),
            origin: record.origin.as_ref(),
        }
    }

    pub fn is_transient(&self) -> bool {
        matches!(
            self.origin,
            Some(MessageOrigin::Plugin {
                transient: true,
                ..
            })
        )
    }

    fn to_owned(self) -> Message {
        Message {
            id: self.id.to_string(),
            role: self.role,
            parts: std::sync::Arc::new(self.parts.to_vec()),
            origin: self.origin.cloned(),
        }
    }

    fn structurally_matches(self, other: Self) -> bool {
        self.id == other.id
            && self.role == other.role
            && self.parts == other.parts
            && self.origin == other.origin
    }
}

impl ChronologicalProjection {
    pub(crate) fn from_read_model(read_model: &crate::session_graph::SessionReadModel) -> Self {
        Self::from_active_read(
            read_model.active_events.as_slice(),
            read_model.messages.as_slice(),
        )
    }

    pub fn from_turn_view(events: &[SessionHistoryRecord], messages: &MessageSequence) -> Self {
        Self::from_active_read(events, messages.as_slice())
    }

    fn from_active_read(active_events: &[SessionHistoryRecord], messages: &[Message]) -> Self {
        let mut projection = Self::default();
        projection
            .entries
            .reserve(active_events.len().saturating_add(messages.len()));
        visit_active_read(active_events, messages, |entry| {
            projection.push(match entry.payload {
                BorrowedChronologicalPayload::Message(message) => {
                    ChronologicalPayload::Message(message.to_owned())
                }
                BorrowedChronologicalPayload::ProtocolEvent(event) => {
                    ChronologicalPayload::ProtocolEvent(event.clone())
                }
            });
        });
        projection
    }

    fn push(&mut self, payload: ChronologicalPayload) {
        let index = self.entries.len();
        self.entries.push(ChronologicalEntry { index, payload });
    }

    pub fn entries(&self) -> &[ChronologicalEntry] {
        self.entries.as_slice()
    }

    pub fn into_entries(self) -> Vec<ChronologicalEntry> {
        self.entries
    }
}

pub fn visit_turn_view<'a>(
    events: &'a [SessionHistoryRecord],
    messages: &'a MessageSequence,
    visit: impl FnMut(BorrowedChronologicalEntry<'a>),
) {
    visit_active_read(events, messages.as_slice(), visit);
}

fn visit_active_read<'a>(
    active_events: &'a [SessionHistoryRecord],
    messages: &'a [Message],
    mut visit: impl FnMut(BorrowedChronologicalEntry<'a>),
) {
    if active_events.is_empty() {
        visit_transcript(messages, visit);
        return;
    }

    let mut index = 0;
    let mut represented_messages = Vec::new();
    for event in active_events {
        match event {
            SessionHistoryRecord::Conversation(record) => {
                let message = BorrowedChronologicalMessage::from_record(record);
                if !message.is_transient() {
                    represented_messages.push(message);
                    visit(BorrowedChronologicalEntry {
                        index,
                        payload: BorrowedChronologicalPayload::Message(message),
                    });
                    index += 1;
                }
            }
            SessionHistoryRecord::Protocol(event) => {
                visit(BorrowedChronologicalEntry {
                    index,
                    payload: BorrowedChronologicalPayload::ProtocolEvent(event),
                });
                index += 1;
            }
        }
    }

    for message in messages {
        let message = BorrowedChronologicalMessage::from_message(message);
        if message.is_transient() {
            continue;
        }
        if let Some(position) = represented_messages
            .iter()
            .position(|represented| represented.structurally_matches(message))
        {
            represented_messages.swap_remove(position);
            continue;
        }
        visit(BorrowedChronologicalEntry {
            index,
            payload: BorrowedChronologicalPayload::Message(message),
        });
        index += 1;
    }
}

fn visit_transcript<'a>(
    messages: &'a [Message],
    mut visit: impl FnMut(BorrowedChronologicalEntry<'a>),
) {
    for (index, message) in messages
        .iter()
        .filter(|message| !message.is_transient())
        .enumerate()
    {
        visit(BorrowedChronologicalEntry {
            index,
            payload: BorrowedChronologicalPayload::Message(
                BorrowedChronologicalMessage::from_message(message),
            ),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_model::ConversationRecord;
    use crate::{PartKind, PruneState, shared_parts};

    fn text_message(id: &str, role: MessageRole, text: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: shared_parts(vec![Part {
                id: format!("{id}.p0"),
                kind: PartKind::Text,
                content: text.to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        }
    }

    fn tool_result_message(id: &str, call_id: &str) -> Message {
        let mut message = text_message(id, MessageRole::User, "tool result");
        std::sync::Arc::make_mut(&mut message.parts)[0].tool_call_id = Some(call_id.to_string());
        message
    }

    fn transient_message(id: &str) -> Message {
        let mut message = text_message(id, MessageRole::System, "transient");
        message.origin = Some(MessageOrigin::Plugin {
            plugin_id: "test".to_string(),
            transient: true,
        });
        message
    }

    fn protocol_event(value: &str) -> ProtocolEvent {
        ProtocolEvent {
            plugin_id: "test_protocol".to_string(),
            payload: serde_json::json!({ "value": value }),
        }
    }

    fn owned_summary(projection: &ChronologicalProjection) -> Vec<String> {
        projection
            .entries()
            .iter()
            .map(|entry| match &entry.payload {
                ChronologicalPayload::Message(message) => {
                    format!("{}:message:{}", entry.index, message.id)
                }
                ChronologicalPayload::ProtocolEvent(event) => {
                    format!("{}:protocol:{}", entry.index, event.payload)
                }
            })
            .collect()
    }

    fn borrowed_summary(
        events: &[SessionHistoryRecord],
        messages: &MessageSequence,
    ) -> Vec<String> {
        let mut summary = Vec::new();
        visit_turn_view(events, messages, |entry| {
            summary.push(match entry.payload {
                BorrowedChronologicalPayload::Message(message) => {
                    format!("{}:message:{}", entry.index, message.id)
                }
                BorrowedChronologicalPayload::ProtocolEvent(event) => {
                    format!("{}:protocol:{}", entry.index, event.payload)
                }
            });
        });
        summary
    }

    #[test]
    fn borrowed_turn_view_matches_owned_active_event_projection() {
        let m1 = text_message("m1", MessageRole::User, "first");
        let m2 = text_message("m2", MessageRole::Assistant, "second");
        let events = vec![
            SessionHistoryRecord::Conversation(ConversationRecord::from_message(m1.clone())),
            SessionHistoryRecord::Protocol(protocol_event("step")),
            SessionHistoryRecord::Conversation(ConversationRecord::from_message(m1.clone())),
        ];
        let messages = MessageSequence::from_owned(vec![m1, m2]);
        let projection = ChronologicalProjection::from_turn_view(&events, &messages);

        assert_eq!(
            borrowed_summary(&events, &messages),
            owned_summary(&projection)
        );
        assert_eq!(
            owned_summary(&projection),
            vec![
                "0:message:m1",
                r#"1:protocol:{"value":"step"}"#,
                "2:message:m1",
                "3:message:m2",
            ]
        );
    }

    #[test]
    fn active_event_occurrences_do_not_hide_a_live_same_id_tail() {
        let first = text_message("same", MessageRole::User, "first");
        let second = text_message("same", MessageRole::User, "second");
        let events = vec![SessionHistoryRecord::Conversation(
            ConversationRecord::from_message(first),
        )];
        let messages = MessageSequence::from_owned(vec![second]);

        let projection = ChronologicalProjection::from_turn_view(&events, &messages);

        assert_eq!(
            owned_summary(&projection),
            vec!["0:message:same", "1:message:same"]
        );
        let ChronologicalPayload::Message(live_tail) = &projection.entries()[1].payload else {
            panic!("expected the rewritten live message");
        };
        assert_eq!(live_tail.parts[0].content, "second");
    }

    #[test]
    fn borrowed_turn_view_matches_owned_transcript_fallback_projection() {
        let messages = MessageSequence::from_owned(vec![
            tool_result_message("m1", "call-1"),
            transient_message("transient"),
            text_message("m2", MessageRole::Assistant, "second"),
        ]);
        let events = Vec::new();
        let projection = ChronologicalProjection::from_turn_view(&events, &messages);

        assert_eq!(
            borrowed_summary(&events, &messages),
            owned_summary(&projection)
        );
    }
}
