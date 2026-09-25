use std::sync::Arc;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// Tool-argument wire fragments are assembled inside provider transports and
// reach this module only as semantic `Part(ToolCall)` events.
pub(super) enum ProviderDeltaClass {
    AssistantProse,
    Reasoning,
}

impl ProviderDeltaClass {
    fn block_kind(self) -> StreamBlockKind {
        match self {
            Self::AssistantProse => StreamBlockKind::AssistantText,
            Self::Reasoning => StreamBlockKind::Reasoning,
        }
    }

    fn session_event(self, content: String, block: StreamBlockIdentity) -> SessionStreamEvent {
        match self {
            Self::AssistantProse => SessionStreamEvent::TextDelta { content, block },
            Self::Reasoning => SessionStreamEvent::ReasoningDelta { content, block },
        }
    }

    fn turn_event(self, text: Arc<str>, block: StreamBlockIdentity) -> TurnEvent {
        match self {
            Self::AssistantProse => TurnEvent::AssistantProseDelta { text, block },
            Self::Reasoning => TurnEvent::ReasoningDelta { text, block },
        }
    }
}

/// One provider call's stream, projected onto both host lanes.
///
/// Every delta and block boundary is published at once, session projection
/// first, through the turn's observer, which never waits on the host. A slow
/// host is the observation queue's concern: it merges the deltas that queue up
/// behind a lagging host (see `turn_observer`), so the provider drain never
/// stalls on host throughput.
pub(super) struct ProviderHostForwarder<'a> {
    event_tx: &'a TurnObserver,
    /// The provider call's observation lane: every event the stream forwards
    /// sequences under the call's replay key (ADR 0105 §1).
    cursor: crate::engine::ObservationCursor,
}

impl<'a> ProviderHostForwarder<'a> {
    pub(super) fn new(
        event_tx: &'a TurnObserver,
        cursor: crate::engine::ObservationCursor,
    ) -> Self {
        Self { event_tx, cursor }
    }

    pub(super) fn forward_delta(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
        content: String,
    ) {
        if content.is_empty() || self.event_tx.is_closed() {
            return;
        }
        let correlation_id = TurnActivityId::new(block.id.clone());
        let text = Arc::from(content.as_str());
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Session(class.session_event(content, block.clone())),
        );
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(correlation_id),
                event: class.turn_event(text, block),
            },
        );
    }

    /// A provider-minted block opened: emit the boundary on both projections.
    pub(super) fn forward_block_start(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
    ) {
        let kind = class.block_kind();
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Session(SessionStreamEvent::StreamBlockStarted {
                kind,
                block: block.clone(),
            }),
        );
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(TurnActivityId::new(block.id.clone())),
                event: TurnEvent::StreamBlockStarted { kind, block },
            },
        );
    }

    /// A provider-minted block closed; `text` is its authoritative text.
    pub(super) fn forward_block_end(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
        text: String,
    ) {
        let kind = class.block_kind();
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Session(SessionStreamEvent::StreamBlockCompleted {
                kind,
                block: block.clone(),
                content: text.clone(),
            }),
        );
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(TurnActivityId::new(block.id.clone())),
                event: TurnEvent::StreamBlockCompleted {
                    kind,
                    block,
                    text: text.into(),
                },
            },
        );
    }

    pub(super) fn send_semantic_session_event(&mut self, event: SessionStreamEvent) {
        self.cursor
            .observe(self.event_tx, crate::engine::ObservedEvent::Session(event));
    }

    pub(super) fn send_semantic_turn_activity(
        &mut self,
        correlation_id: Option<TurnActivityId>,
        event: TurnEvent,
    ) {
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id,
                event,
            },
        );
    }
}
