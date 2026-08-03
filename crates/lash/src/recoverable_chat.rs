//! Host-facing recoverable-chat observation seam.
//!
//! Lash owns the observation cursor, replay-gap, redelivery identity, and
//! terminal-replacement contract. Hosts own authorization, product events,
//! transcript presentation, and cancellation controls.

use std::collections::{BTreeSet, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use lash_core::{
    SessionCursor, SessionObservationEvent, SessionObservationEventPayload, SessionReadView,
    facade_support::LiveReplayGap,
};

use crate::Result;
use crate::session::{ObservableSession, SessionObservationStream, SessionObservationStreamItem};

/// At-least-once delivery identity for one Lash observation event.
///
/// The replay-store incarnation makes this identity safe to persist across
/// process restarts: a newly constructed store may reuse a cursor, but it
/// cannot reproduce the old identity. Re-delivery of the same identity must
/// not create another row. A [`RecoverableChatUpdate::ReplayGap`] still makes
/// the replacement snapshot authoritative and clears the subscription's
/// bounded applied-identity window.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecoverableChatEventId {
    pub session_id: String,
    pub replay_incarnation_id: String,
    pub cursor: String,
}

impl RecoverableChatEventId {
    fn from_event(event: &SessionObservationEvent) -> Self {
        Self {
            session_id: event.session_id.clone(),
            replay_incarnation_id: event.replay_incarnation_id.clone(),
            cursor: event.cursor.to_string(),
        }
    }
}

const MAX_APPLIED_EVENT_IDS: usize = 4096;

#[derive(Default)]
struct AppliedEventIds {
    ids: BTreeSet<RecoverableChatEventId>,
    order: VecDeque<RecoverableChatEventId>,
}

impl AppliedEventIds {
    fn insert(&mut self, id: RecoverableChatEventId) -> bool {
        if !self.ids.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > MAX_APPLIED_EVENT_IDS {
            if let Some(expired) = self.order.pop_front() {
                self.ids.remove(&expired);
            }
        }
        true
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.order.clear();
    }

    fn extend(&mut self, ids: impl IntoIterator<Item = RecoverableChatEventId>) {
        for id in ids {
            self.insert(id);
        }
    }
}

/// One authoritative materialization paired with the cursor at which it was
/// captured.
#[derive(Clone, Debug)]
pub struct RecoverableChatSnapshot {
    pub read_view: SessionReadView,
    pub cursor: SessionCursor,
}

impl RecoverableChatSnapshot {
    pub fn capture(observable: &ObservableSession) -> Self {
        let observation = observable.current_observation();
        Self {
            read_view: observation.read_view,
            cursor: observation.cursor,
        }
    }
}

/// Update yielded by [`RecoverableChatSubscription`].
#[derive(Clone, Debug)]
pub enum RecoverableChatUpdate {
    /// A provisional or lifecycle observation with stable redelivery identity.
    Event {
        id: RecoverableChatEventId,
        event: std::sync::Arc<SessionObservationEvent>,
    },
    /// The requested cursor fell outside bounded replay. Replace the
    /// projection from `snapshot`, persist `gap.latest_cursor`, and continue
    /// consuming this same stream.
    ReplayGap {
        snapshot: RecoverableChatSnapshot,
        gap: LiveReplayGap,
    },
    /// A terminal commit replaces provisional state with this authoritative
    /// snapshot. The committed event is retained for remote encoding and
    /// tracing, but the snapshot is the transcript authority.
    TerminalReplacement {
        id: RecoverableChatEventId,
        event: std::sync::Arc<SessionObservationEvent>,
        snapshot: RecoverableChatSnapshot,
    },
}

/// Recoverable Lash observation stream for chat hosts.
///
/// Dropping this stream only disconnects observation. It never requests
/// cancellation; cancellation remains an explicit call through
/// [`TurnWorkDriver`](crate::TurnWorkDriver).
pub struct RecoverableChatSubscription {
    inner: SessionObservationStream,
    applied: AppliedEventIds,
}

impl RecoverableChatSubscription {
    pub(crate) fn new(inner: SessionObservationStream) -> Self {
        Self {
            inner,
            applied: AppliedEventIds::default(),
        }
    }

    /// Seed identities already applied by the host projection.
    ///
    /// This makes reconnect redelivery idempotent even when the projection
    /// cursor intentionally trails individual applied events. Persisting the
    /// identities across a process restart is safe because the replay-store
    /// incarnation distinguishes newly emitted events from old ones. The
    /// subscription retains only a bounded recent window and still clears it
    /// when it emits [`RecoverableChatUpdate::ReplayGap`], whose replacement
    /// snapshot is authoritative.
    pub fn with_applied_event_ids(
        mut self,
        ids: impl IntoIterator<Item = RecoverableChatEventId>,
    ) -> Self {
        self.applied.extend(ids);
        self
    }

    pub fn cursor(&self) -> &SessionCursor {
        self.inner.cursor()
    }
}

impl Stream for RecoverableChatSubscription {
    type Item = Result<RecoverableChatUpdate>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let item = match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err))),
                Poll::Ready(Some(Ok(item))) => item,
            };
            match item {
                SessionObservationStreamItem::Gap { observation, gap } => {
                    self.applied.clear();
                    return Poll::Ready(Some(Ok(RecoverableChatUpdate::ReplayGap {
                        snapshot: RecoverableChatSnapshot {
                            read_view: observation.read_view,
                            cursor: observation.cursor,
                        },
                        gap,
                    })));
                }
                SessionObservationStreamItem::Event(event) => {
                    let id = RecoverableChatEventId::from_event(&event);
                    if !self.applied.insert(id.clone()) {
                        continue;
                    }
                    if let SessionObservationEventPayload::Committed { read_view } = &event.payload
                    {
                        self.applied.clear();
                        self.applied.insert(id.clone());
                        return Poll::Ready(Some(Ok(RecoverableChatUpdate::TerminalReplacement {
                            id,
                            snapshot: RecoverableChatSnapshot {
                                read_view: read_view.clone(),
                                cursor: event.cursor.clone(),
                            },
                            event,
                        })));
                    }
                    return Poll::Ready(Some(Ok(RecoverableChatUpdate::Event { id, event })));
                }
            }
        }
    }
}

impl ObservableSession {
    /// Capture one authoritative snapshot before attaching live observation.
    pub fn recoverable_chat_snapshot(&self) -> RecoverableChatSnapshot {
        RecoverableChatSnapshot::capture(self)
    }

    /// Resume a recoverable chat observation from an authoritative snapshot's
    /// cursor.
    pub fn subscribe_recoverable_chat(&self, cursor: SessionCursor) -> RecoverableChatSubscription {
        RecoverableChatSubscription::new(self.subscribe_and_recover(cursor))
    }
}
