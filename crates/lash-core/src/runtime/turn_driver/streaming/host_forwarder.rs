use std::collections::VecDeque;
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

    fn session_event(
        self,
        content: String,
        block: StreamBlockIdentity,
        _payload_constructions: &mut PayloadConstructionCounter,
    ) -> SessionStreamEvent {
        #[cfg(test)]
        {
            _payload_constructions.count += 1;
        }
        match self {
            Self::AssistantProse => SessionStreamEvent::TextDelta { content, block },
            Self::Reasoning => SessionStreamEvent::ReasoningDelta { content, block },
        }
    }

    fn turn_event(
        self,
        text: Arc<str>,
        block: StreamBlockIdentity,
        _payload_constructions: &mut PayloadConstructionCounter,
    ) -> TurnEvent {
        #[cfg(test)]
        {
            _payload_constructions.count += 1;
        }
        match self {
            Self::AssistantProse => TurnEvent::AssistantProseDelta { text, block },
            Self::Reasoning => TurnEvent::ReasoningDelta { text, block },
        }
    }
}

struct PayloadConstructionCounter {
    #[cfg(test)]
    count: usize,
}

#[derive(Debug)]
struct PendingHostDelta {
    class: ProviderDeltaClass,
    block: StreamBlockIdentity,
    content: String,
    session_forwarded: bool,
}

impl PendingHostDelta {
    fn new(class: ProviderDeltaClass, block: StreamBlockIdentity, content: String) -> Self {
        Self {
            class,
            block,
            content,
            session_forwarded: false,
        }
    }

    fn can_merge(&self, class: ProviderDeltaClass, block: &StreamBlockIdentity) -> bool {
        !self.session_forwarded && self.class == class && self.block.id == block.id
    }

    fn correlation_id(&self) -> TurnActivityId {
        TurnActivityId::new(self.block.id.clone())
    }
}

/// Elastic delta lane owned by one provider call.
///
/// Fast hosts receive the original session + turn projections event-for-event.
/// Once the bounded host channel fills, only adjacent deltas for the same
/// provider-minted block merge — block boundaries are never crossed. The queue
/// has no hard cap: it retains exactly the unforwarded provider content until
/// a reliable semantic flush catches up.
pub(super) struct ProviderHostForwarder<'a> {
    event_tx: &'a mpsc::Sender<RuntimeStreamEvent>,
    pending: VecDeque<PendingHostDelta>,
    payload_constructions: PayloadConstructionCounter,
}

impl<'a> ProviderHostForwarder<'a> {
    pub(super) fn new(event_tx: &'a mpsc::Sender<RuntimeStreamEvent>) -> Self {
        Self {
            event_tx,
            pending: VecDeque::new(),
            payload_constructions: PayloadConstructionCounter {
                #[cfg(test)]
                count: 0,
            },
        }
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

        self.try_drain();
        if let Some(pending) = self.pending.back_mut()
            && pending.can_merge(class, &block)
        {
            pending.content.push_str(&content);
        } else {
            self.pending
                .push_back(PendingHostDelta::new(class, block, content));
        }
        self.try_drain();
    }

    /// A provider-minted block opened: flush any pending deltas, then emit the
    /// boundary on both projections.
    pub(super) async fn forward_block_start(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
    ) {
        let kind = class.block_kind();
        self.flush().await;
        send_session_event(
            self.event_tx,
            SessionStreamEvent::StreamBlockStarted {
                kind,
                block: block.clone(),
            },
        )
        .await;
        send_turn_activity(
            self.event_tx,
            TurnActivityId::new(block.id.clone()),
            TurnEvent::StreamBlockStarted { kind, block },
        )
        .await;
    }

    /// A provider-minted block closed; `text` is its authoritative text.
    pub(super) async fn forward_block_end(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
        text: String,
    ) {
        let kind = class.block_kind();
        self.flush().await;
        send_session_event(
            self.event_tx,
            SessionStreamEvent::StreamBlockCompleted {
                kind,
                block: block.clone(),
                content: text.clone(),
            },
        )
        .await;
        send_turn_activity(
            self.event_tx,
            TurnActivityId::new(block.id.clone()),
            TurnEvent::StreamBlockCompleted {
                kind,
                block,
                text: text.into(),
            },
        )
        .await;
    }

    fn try_drain(&mut self) {
        loop {
            let permit = match self.event_tx.try_reserve() {
                Ok(permit) => permit,
                Err(mpsc::error::TrySendError::Full(())) => return,
                Err(mpsc::error::TrySendError::Closed(())) => {
                    self.pending.clear();
                    return;
                }
            };
            let Some(pending) = self.pending.front_mut() else {
                return;
            };
            if !pending.session_forwarded {
                permit.send(RuntimeStreamEvent::Session(pending.class.session_event(
                    pending.content.clone(),
                    pending.block.clone(),
                    &mut self.payload_constructions,
                )));
                pending.session_forwarded = true;
                continue;
            }
            permit.send(RuntimeStreamEvent::Turn(TurnActivity::new(
                pending.correlation_id(),
                pending.class.turn_event(
                    Arc::from(pending.content.as_str()),
                    pending.block.clone(),
                    &mut self.payload_constructions,
                ),
            )));
            self.pending.pop_front();
        }
    }

    pub(super) async fn finish(&mut self, cancelled: bool) {
        if cancelled {
            // Deltas are non-authoritative provider-wire volume. Semantic
            // events never live in `pending`, so cancellation may discard the
            // delta backlog without delaying cancellation on host throughput.
            self.try_drain();
            self.pending.clear();
        } else {
            self.flush().await;
        }
    }

    #[expect(clippy::expect_used, reason = "the pending delta is still queued here")]
    async fn flush(&mut self) {
        while let Some(pending) = self.pending.front() {
            if !pending.session_forwarded {
                let event = RuntimeStreamEvent::Session(pending.class.session_event(
                    pending.content.clone(),
                    pending.block.clone(),
                    &mut self.payload_constructions,
                ));
                if self.event_tx.send(event).await.is_err() {
                    self.pending.clear();
                    return;
                }
                self.pending
                    .front_mut()
                    .expect("pending delta remains after its session projection")
                    .session_forwarded = true;
                continue;
            }

            let activity = TurnActivity::new(
                pending.correlation_id(),
                pending.class.turn_event(
                    Arc::from(pending.content.as_str()),
                    pending.block.clone(),
                    &mut self.payload_constructions,
                ),
            );
            if self
                .event_tx
                .send(RuntimeStreamEvent::Turn(activity))
                .await
                .is_err()
            {
                self.pending.clear();
                return;
            }
            self.pending.pop_front();
        }
    }

    pub(super) async fn send_semantic_session_event(&mut self, event: SessionStreamEvent) {
        self.flush().await;
        send_session_event(self.event_tx, event).await;
    }

    pub(super) async fn send_semantic_turn_activity(
        &mut self,
        correlation_id: TurnActivityId,
        event: TurnEvent,
    ) {
        self.flush().await;
        send_turn_activity(self.event_tx, correlation_id, event).await;
    }

    #[cfg(test)]
    pub(super) fn payload_constructions(&self) -> usize {
        self.payload_constructions.count
    }
}
