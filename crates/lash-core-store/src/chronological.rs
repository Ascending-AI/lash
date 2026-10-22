#[cfg(test)]
use crate::Part;
use crate::session_model::{ConversationRecord, ProtocolEvent, SessionHistoryRecord};
use crate::{Message, MessageOrigin, MessageRole, MessageSequence};
use lash_protocol_types::allocation::{
    Allocated, AllocationBudget, AllocationError, AllocationHold, SourceCapacity,
};
use lash_protocol_types::core_support::*;
use lash_protocol_types::{AppendVec, MessageParts};

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ChronologicalProjection {
    entries: AppendVec<ChronologicalEntry>,
    #[serde(skip)]
    _allocation: AllocationHold,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ChronologicalEntry {
    pub index: usize,
    pub payload: ChronologicalPayload,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChronologicalPayload {
    Message(std::sync::Arc<Allocated<Message>>),
    ProtocolEvent(std::sync::Arc<Allocated<crate::session_model::ProtocolEvent>>),
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
    pub parts: &'a std::sync::Arc<MessageParts>,
    pub origin: Option<&'a MessageOrigin>,
    pub reply_marker: Option<&'a crate::TurnReply>,
}

impl<'a> BorrowedChronologicalMessage<'a> {
    fn from_message(message: &'a Message) -> Self {
        Self {
            id: &message.id,
            role: message.role,
            parts: &message.parts,
            origin: message.origin.as_ref(),
            reply_marker: message.reply_marker.as_ref(),
        }
    }

    fn from_record(record: &'a ConversationRecord) -> Self {
        Self {
            id: &record.id,
            role: record.role,
            parts: &record.parts,
            origin: record.origin.as_ref(),
            reply_marker: record.reply_marker.as_ref(),
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

    fn to_owned_in(
        self,
        budget: &AllocationBudget,
    ) -> Result<std::sync::Arc<Allocated<Message>>, AllocationError> {
        let parts = MessageParts::adopt_in(self.parts, budget)?;
        // The immutable payload keeps this same obligation after its entry
        // or projection is cloned, detached or dropped.
        let allocation = budget.reserve((self.id.len() as u64).saturating_add(
            lash_protocol_types::allocation::payload_capacity(
                &(&self.origin, &self.reply_marker),
                std::mem::size_of::<Allocated<Message>>() + 64,
            ),
        ))?;
        Ok(std::sync::Arc::new(Allocated::new(
            Message {
                id: self.id.to_string(),
                role: self.role,
                parts,
                origin: self.origin.cloned(),
                reply_marker: self.reply_marker.cloned(),
            },
            allocation,
            budget.clone(),
        )))
    }
}

impl SourceCapacity for ChronologicalEntry {
    fn owned_capacity(&self) -> u64 {
        0
    }
    fn sources_admitted_in(&self, budget: &AllocationBudget) -> bool {
        match &self.payload {
            ChronologicalPayload::Message(message) => {
                message.allocation_budget().identity() == budget.identity()
                    && message.sources_admitted_in(budget)
            }
            ChronologicalPayload::ProtocolEvent(event) => {
                event.allocation_budget().identity() == budget.identity()
                    && event.sources_admitted_in(budget)
            }
        }
    }
}

impl ChronologicalProjection {
    #[expect(
        clippy::expect_used,
        reason = "the independent cold projection uses an unowned budget"
    )]
    pub fn from_read_model(read_model: &crate::session_graph::SessionReadModel) -> Self {
        Self::from_read_model_in(read_model, &AllocationBudget::unowned())
            .expect("independent chronological projection")
    }

    pub fn from_read_model_in(
        read_model: &crate::session_graph::SessionReadModel,
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        Self::from_active_read_in(
            read_model.active_events.as_slice(),
            read_model.messages.as_slice(),
            budget,
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "the independent cold projection uses an unowned budget"
    )]
    pub fn from_turn_view(events: &[SessionHistoryRecord], messages: &MessageSequence) -> Self {
        Self::from_turn_view_in(events, messages, &AllocationBudget::unowned())
            .expect("independent chronological projection")
    }

    pub fn from_turn_view_in(
        events: &[SessionHistoryRecord],
        messages: &MessageSequence,
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        Self::from_active_read_in(events, messages.as_slice(), budget)
    }

    fn from_active_read_in(
        active_events: &[SessionHistoryRecord],
        messages: &[Message],
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        let allocation = budget.reserve((std::mem::size_of::<Self>() + 64) as u64)?;
        // visit_active_read's identity-matching scratch stores references only;
        // geometrical Vec capacity is at most twice the source event count.
        let _scratch = budget.reserve(
            (active_events.len() as u64)
                .saturating_mul(2 * std::mem::size_of::<&ConversationRecord>() as u64)
                .saturating_add(128),
        )?;
        let capacity = active_events.len().saturating_add(messages.len());
        let mut entries = AppendVec::try_with_capacity(capacity, budget)?;
        let mut failure = None;
        visit_active_read(active_events, messages, |entry| {
            if failure.is_some() {
                return;
            }
            let result = (|| {
                let payload = match entry.payload {
                    BorrowedChronologicalPayload::Message(message) => {
                        ChronologicalPayload::Message(message.to_owned_in(budget)?)
                    }
                    BorrowedChronologicalPayload::ProtocolEvent(event) => {
                        let payload = event.payload.adopt_in(budget)?;
                        let allocation = budget.reserve(
                            event
                                .owned_capacity()
                                .saturating_add(
                                    std::mem::size_of::<Allocated<ProtocolEvent>>() as u64
                                )
                                .saturating_add(64),
                        )?;
                        ChronologicalPayload::ProtocolEvent(std::sync::Arc::new(Allocated::new(
                            ProtocolEvent {
                                plugin_id: event.plugin_id.clone(),
                                payload,
                            },
                            allocation,
                            budget.clone(),
                        )))
                    }
                };
                let index = entries.len();
                entries.push(ChronologicalEntry { index, payload })
            })();
            if let Err(error) = result {
                failure = Some(error);
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(Self {
            entries,
            _allocation: allocation,
        })
    }

    pub fn entries(&self) -> &[ChronologicalEntry] {
        self.entries.as_slice()
    }

    /// Keep the source buffer's obligation with entries that escape the view.
    pub fn into_entries(self) -> AppendVec<ChronologicalEntry> {
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
                    represented_messages.push(record);
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
        let borrowed = BorrowedChronologicalMessage::from_message(message);
        if borrowed.is_transient() {
            continue;
        }
        if let Some(position) = represented_messages
            .iter()
            .position(|represented| represented.content_equals(message))
        {
            represented_messages.swap_remove(position);
            continue;
        }
        visit(BorrowedChronologicalEntry {
            index,
            payload: BorrowedChronologicalPayload::Message(borrowed),
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

    /// LO-EVICT-CHRONO: an escaped projection's entry buffer and message
    /// parts retain the obligation admitted before source copying.
    #[test]
    fn chronological_projection_admits_before_copying_and_keeps_escaped_entries_charged() {
        use lash_protocol_types::allocation::{
            AllocationBudget, AllocationError, AllocationHold, AllocationLedger,
            AllocationReservation, ReserveError,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        };
        #[derive(Clone)]
        struct Ledger {
            used: Arc<AtomicU64>,
            refuse: Arc<AtomicBool>,
        }
        struct Hold {
            used: Arc<AtomicU64>,
            bytes: u64,
        }
        impl AllocationReservation for Hold {
            fn retain(&self) {}
        }
        impl Drop for Hold {
            fn drop(&mut self) {
                self.used.fetch_sub(self.bytes, Ordering::SeqCst);
            }
        }
        impl AllocationLedger for Ledger {
            fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
                if self.refuse.load(Ordering::SeqCst) {
                    return Err(AllocationError {
                        kind: ReserveError::Pressure,
                        actor: "chronological".into(),
                        requested: bytes,
                        capacity: 1 << 20,
                    });
                }
                self.used.fetch_add(bytes, Ordering::SeqCst);
                Ok(AllocationHold::new(Hold {
                    used: self.used.clone(),
                    bytes,
                }))
            }
            fn reserve_shared(
                &self,
                _: usize,
                bytes: u64,
            ) -> Result<AllocationHold, AllocationError> {
                self.reserve(bytes)
            }
        }
        let ledger = Ledger {
            used: Arc::new(AtomicU64::new(0)),
            refuse: Arc::new(AtomicBool::new(false)),
        };
        let budget = AllocationBudget::new(ledger.clone());
        let source = crate::SessionNodeRecord::new(
            crate::NodeId::fixture("source"),
            None,
            "2026-10-22T00:00:00.000000000Z".parse().unwrap(),
            crate::SessionNodePayload::Event {
                event: SessionHistoryRecord::Conversation(ConversationRecord::from_message(
                    text_message("source", MessageRole::User, &"payload".repeat(1024)),
                )),
            },
        );
        let fleet = crate::store::FleetFormat::current();
        let encoded = source.encode_storage_body(fleet).unwrap();
        let decoded = crate::SessionNodeRecord::decode_storage_body_in(
            "source", None, &encoded, fleet, &budget,
        )
        .unwrap();
        let graph = crate::SessionGraph::from_shared_nodes_in(
            [Arc::new(decoded)],
            Some(crate::NodeId::fixture("source")),
            &budget,
        )
        .unwrap();
        let mut state = crate::RuntimeSessionState::fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(16),
            crate::NoProgressBudget::bounded(12),
        ));
        state.session_graph = graph;
        let view = crate::SessionReadView::from_persisted_state(&state);
        drop(state);
        let before = ledger.used.load(Ordering::SeqCst);
        ledger.refuse.store(true, Ordering::SeqCst);
        assert!(matches!(
            view.shared_chronological_projection(),
            Err(AllocationError {
                kind: ReserveError::Pressure,
                ..
            })
        ));
        assert_eq!(ledger.used.load(Ordering::SeqCst), before);
        ledger.refuse.store(false, Ordering::SeqCst);
        let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
        let projection = view
            .shared_chronological_projection()
            .expect("projection is admitted");
        let counts = measurement.finish();
        let charged = ledger.used.load(Ordering::SeqCst) - before;
        assert!(
            charged >= counts.allocated as u64,
            "chronological source admission {charged} must bound its actual {} copied bytes",
            counts.allocated
        );
        let measured_read = crate::history_growth_law::allocation_counter::Measurement::start();
        let second = view
            .shared_chronological_projection()
            .expect("already admitted projection");
        let read_counts = measured_read.finish();
        assert!(Arc::ptr_eq(&projection, &second));
        assert_eq!(
            read_counts.allocated, 0,
            "a memoized chronological reader allocates nothing"
        );
        drop(second);
        drop(view);
        let entries = Arc::try_unwrap(projection)
            .expect("the escaped projection is its last owner")
            .into_entries();
        assert!(
            ledger.used.load(Ordering::SeqCst) > 0,
            "escaped entries keep their source allocation"
        );
        let ChronologicalPayload::Message(message) = &entries[0].payload else {
            panic!("message entry");
        };
        let parts = message.parts.clone();
        drop(entries);
        assert!(
            ledger.used.load(Ordering::SeqCst) > 0,
            "escaped parts keep their own allocation"
        );
        assert_eq!(parts[0].content(), "payload".repeat(1024));
        drop(parts);
        assert_eq!(ledger.used.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn chronological_projection_preserves_the_committed_reply_marker() {
        use lash_protocol_types::core_support::TurnReplyCoreSupport;
        let mut reply = text_message("reply", MessageRole::Assistant, "answer");
        reply.reply_marker = Some(crate::TurnReply::mint(
            crate::TurnId::parse("turn").unwrap(),
            reply.parts[0].id().to_owned(),
        ));
        let expected = reply.reply_marker.clone();
        let events = vec![SessionHistoryRecord::Conversation(
            ConversationRecord::from_message(reply),
        )];
        let projection =
            ChronologicalProjection::from_turn_view(&events, &MessageSequence::default());
        let ChronologicalPayload::Message(reply) = &projection.entries()[0].payload else {
            panic!("reply must remain a conversation record");
        };
        assert_eq!(reply.reply_marker, expected);
    }
    use crate::session_model::ConversationRecord;
    use crate::shared_parts;

    fn text_message(id: &str, role: MessageRole, text: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: shared_parts(vec![Part::text(format!("{id}.p0"), text.to_string(), None)]),
            origin: None,
            reply_marker: None,
        }
    }

    fn protocol_event(value: &str) -> ProtocolEvent {
        ProtocolEvent {
            plugin_id: "test_protocol".to_string(),
            payload: serde_json::json!({ "value": value }).into(),
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
        assert_eq!(live_tail.parts[0].content(), "second");
    }
}
