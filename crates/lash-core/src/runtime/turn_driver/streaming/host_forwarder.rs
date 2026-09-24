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
}

impl<'a> ProviderHostForwarder<'a> {
    pub(super) fn new(event_tx: &'a TurnObserver) -> Self {
        Self { event_tx }
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
        self.event_tx.publish(RuntimeStreamEvent::Session(
            class.session_event(content, block.clone()),
        ));
        self.event_tx
            .activity(correlation_id, class.turn_event(text, block));
    }

    /// A provider-minted block opened: emit the boundary on both projections.
    pub(super) fn forward_block_start(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
    ) {
        let kind = class.block_kind();
        self.event_tx
            .session(SessionStreamEvent::StreamBlockStarted {
                kind,
                block: block.clone(),
            });
        self.event_tx.activity(
            TurnActivityId::new(block.id.clone()),
            TurnEvent::StreamBlockStarted { kind, block },
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
        self.event_tx
            .session(SessionStreamEvent::StreamBlockCompleted {
                kind,
                block: block.clone(),
                content: text.clone(),
            });
        self.event_tx.activity(
            TurnActivityId::new(block.id.clone()),
            TurnEvent::StreamBlockCompleted {
                kind,
                block,
                text: text.into(),
            },
        );
    }

    pub(super) fn send_semantic_session_event(&mut self, event: SessionStreamEvent) {
        self.event_tx.session(event);
    }

    pub(super) fn send_semantic_turn_activity(
        &mut self,
        correlation_id: TurnActivityId,
        event: TurnEvent,
    ) {
        self.event_tx.activity(correlation_id, event);
    }
}
