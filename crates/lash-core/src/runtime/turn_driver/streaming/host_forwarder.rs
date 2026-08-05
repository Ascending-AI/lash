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
    fn session_event(
        self,
        content: String,
        _payload_constructions: &mut PayloadConstructionCounter,
    ) -> SessionStreamEvent {
        #[cfg(test)]
        {
            _payload_constructions.count += 1;
        }
        match self {
            Self::AssistantProse => SessionStreamEvent::TextDelta { content },
            Self::Reasoning => SessionStreamEvent::ReasoningDelta { content },
        }
    }

    fn turn_event(
        self,
        text: Arc<str>,
        _payload_constructions: &mut PayloadConstructionCounter,
    ) -> TurnEvent {
        #[cfg(test)]
        {
            _payload_constructions.count += 1;
        }
        match self {
            Self::AssistantProse => TurnEvent::AssistantProseDelta { text },
            Self::Reasoning => TurnEvent::ReasoningDelta { text },
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
    correlation_id: TurnActivityId,
    content: String,
    session_forwarded: bool,
}

impl PendingHostDelta {
    fn new(class: ProviderDeltaClass, correlation_id: TurnActivityId, content: String) -> Self {
        Self {
            class,
            correlation_id,
            content,
            session_forwarded: false,
        }
    }

    fn can_merge(&self, class: ProviderDeltaClass, correlation_id: &TurnActivityId) -> bool {
        !self.session_forwarded && self.class == class && &self.correlation_id == correlation_id
    }
}

/// Elastic delta lane owned by one provider call.
///
/// Fast hosts receive the original session + turn projections event-for-event.
/// Once the bounded host channel fills, only adjacent deltas for the same
/// correlation merge. The queue has no hard cap: it retains exactly the
/// unforwarded provider content until a reliable semantic flush catches up.
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
        correlation_id: TurnActivityId,
        content: String,
    ) {
        if content.is_empty() || self.event_tx.is_closed() {
            return;
        }

        self.try_drain();
        if let Some(pending) = self.pending.back_mut()
            && pending.can_merge(class, &correlation_id)
        {
            pending.content.push_str(&content);
        } else {
            self.pending
                .push_back(PendingHostDelta::new(class, correlation_id, content));
        }
        self.try_drain();
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
                    &mut self.payload_constructions,
                )));
                pending.session_forwarded = true;
                continue;
            }
            permit.send(RuntimeStreamEvent::Turn(TurnActivity::new(
                pending.correlation_id.clone(),
                pending.class.turn_event(
                    Arc::from(pending.content.as_str()),
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

    async fn flush(&mut self) {
        while let Some(pending) = self.pending.front() {
            if !pending.session_forwarded {
                let event = RuntimeStreamEvent::Session(
                    pending
                        .class
                        .session_event(pending.content.clone(), &mut self.payload_constructions),
                );
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
                pending.correlation_id.clone(),
                pending.class.turn_event(
                    Arc::from(pending.content.as_str()),
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
