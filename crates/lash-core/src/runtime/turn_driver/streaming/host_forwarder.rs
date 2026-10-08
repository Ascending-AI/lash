use super::*;

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

    pub(super) fn stream_key(&self) -> &str {
        self.cursor.key().as_str()
    }

    /// Publish one step of a streamed block's lifecycle: the same payload
    /// on both projections, correlated by the block's activity id. An empty
    /// delta carries nothing and is not published.
    pub(super) fn forward_block(&mut self, event: StreamBlockEvent) {
        if let Some(text) = event.delta_text()
            && (text.is_empty() || self.event_tx.is_closed())
        {
            return;
        }
        let correlation_id = TurnActivityId::stream_block(self.stream_key(), event.block());
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Session(SessionStreamEvent::StreamBlock(event.clone())),
        );
        self.cursor.observe(
            self.event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(correlation_id),
                event: TurnEvent::StreamBlock(event),
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
