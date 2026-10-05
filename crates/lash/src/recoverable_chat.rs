//! Host-facing recoverable-chat observation seam.
//!
//! Lash owns the observation cursor, replay-gap, redelivery identity, and
//! terminal-replacement contract. Hosts own authorization, product events,
//! transcript presentation, and cancellation controls.
//!
//! The feed is durable-anchored (FIG-5090): its snapshot is the session's
//! durable head, its terminal replacements are the durable commits past the
//! snapshot, and a replay gap rebuilds from the durable head. Any process
//! that shares the session's store therefore serves a consistent feed; this
//! process's live replay adds only the provisional events published here.
//!
//! A commit arrives by reference (FIG-5100): its revision and the transcript
//! rows it added, never the session's read view. The host advances its
//! projection by applying those rows. A commit whose rows extend a revision
//! the host does not hold arrives as a [`RecoverableChatUpdate::ReplayGap`]
//! with the durable head instead.

use lash_sansio::SessionId;
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
    /// Session that produced the observation event.
    pub session_id: SessionId,
    /// Replay-store incarnation that produced the observation event.
    pub replay_incarnation_id: String,
    /// Replay cursor of the observation event.
    pub cursor: String,
}

impl RecoverableChatEventId {
    fn from_event(event: &SessionObservationEvent) -> Self {
        Self {
            session_id: event.session_id(),
            replay_incarnation_id: event.replay_incarnation_id().to_string(),
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
    /// Authoritative session read model captured by the snapshot.
    pub read_view: SessionReadView,
    /// Replay cursor at which the read model was captured.
    pub cursor: SessionCursor,
}

#[derive(Clone, Debug)]
pub enum RecoverableChatUpdate {
    /// A provisional or lifecycle observation with stable redelivery identity.
    Event {
        /// Stable replay identity for this observation event.
        id: RecoverableChatEventId,
        /// Observation event delivered by this update.
        event: std::sync::Arc<SessionObservationEvent>,
    },
    /// The requested cursor could not be continued: it fell outside bounded
    /// replay, another replay store minted it, or it is past the durable
    /// head. `snapshot` is the durable head: replace the projection from it,
    /// persist `gap.latest_cursor`, and continue consuming this same stream.
    ReplayGap {
        /// Authoritative session snapshot for recovering the read model.
        snapshot: RecoverableChatSnapshot,
        /// Replay gap that required snapshot replacement.
        gap: LiveReplayGap,
    },
    /// A terminal commit settles provisional state. `event`'s payload is
    /// [`SessionObservationEventPayload::Committed`]: its `rows` are the
    /// transcript rows the commit added to the revision the host holds, so
    /// the host advances its projection by applying them, and persists
    /// `event.cursor`. The subscription delivers a commit only to a host
    /// holding the revision it extends; otherwise it yields a
    /// [`Self::ReplayGap`] with the durable head.
    TerminalReplacement {
        /// Stable replay identity for this observation event.
        id: RecoverableChatEventId,
        /// The commit, carrying its revision and rows delta.
        event: std::sync::Arc<SessionObservationEvent>,
    },
    /// Resident authority changed without a commit. The event is a
    /// reference: a host that projects resident state reads it again
    /// ([`ObservableSession::recoverable_chat_snapshot`]); provisional
    /// transcript rows stay unsettled.
    ResidentReplacement {
        /// Stable replay identity for this observation event.
        id: RecoverableChatEventId,
        /// Observation event delivered by this update.
        event: std::sync::Arc<SessionObservationEvent>,
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

    /// Returns the stream's current replay cursor.
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
                    let update = match &event.payload {
                        SessionObservationEventPayload::Committed { .. } => {
                            self.applied.clear();
                            self.applied.insert(id.clone());
                            RecoverableChatUpdate::TerminalReplacement { id, event }
                        }
                        SessionObservationEventPayload::ResidentChanged => {
                            RecoverableChatUpdate::ResidentReplacement { id, event }
                        }
                        _ => RecoverableChatUpdate::Event { id, event },
                    };
                    return Poll::Ready(Some(Ok(update)));
                }
            }
        }
    }
}

impl ObservableSession {
    /// Capture one authoritative snapshot before attaching live observation:
    /// the session's durable head and the cursor bound to its revision
    /// ([`ObservableSession::snapshot`]).
    pub async fn recoverable_chat_snapshot(&self) -> Result<RecoverableChatSnapshot> {
        let observation = self.snapshot().await?;
        Ok(RecoverableChatSnapshot {
            read_view: observation.read_view,
            cursor: observation.cursor,
        })
    }

    /// Resume a recoverable chat observation from an authoritative snapshot's
    /// cursor. Every durable commit past the cursor's revision arrives once,
    /// in order, as a [`RecoverableChatUpdate::TerminalReplacement`]
    /// carrying its rows delta, whichever process made it, or, when its
    /// rows extend a revision the host does not hold, as a
    /// [`RecoverableChatUpdate::ReplayGap`] with the durable head.
    pub fn subscribe_recoverable_chat(&self, cursor: SessionCursor) -> RecoverableChatSubscription {
        RecoverableChatSubscription::new(self.subscribe_and_recover(cursor))
    }
}
