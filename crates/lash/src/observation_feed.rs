//! The session feed: observation whose snapshot is the session's durable
//! head and whose tail is the configured live replay store (FIG-5090).
//!
//! A feed's snapshot is the durable head, paired with a cursor bound to its
//! revision ([`ObservableSession::snapshot`](crate::ObservableSession::snapshot)).
//! Its tail is the session's live replay after that cursor. The durable head
//! is the authority a cursor is judged against: a cursor behind it is
//! continued only when the replay carries a `Committed` event bridging to
//! it, and every gap's replacement snapshot is the durable head, never a
//! resident projection that may trail a commit another process made.
//!
//! A `Committed` event carries the commit's rows delta, not the session's
//! read view: the feed delivers it only to a consumer that holds the
//! revision the delta extends, and rebuilds from the durable head when the
//! consumer holds any other.
//!
//! Which commits reach the tail is the live replay store's property. The
//! in-memory default holds this process's publications; a host whose
//! sessions run on several processes configures one shared store, and every
//! process's feed then carries every commit.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::BoxFuture;
use futures_util::{FutureExt as _, Stream, StreamExt as _};
use lash_core::facade_support::LiveReplayGap;
use lash_core::{
    LiveReplayGapReason, LiveReplayStore, LiveReplayStoreError, LiveReplaySubscribeOutcome,
    LiveReplaySubscription, SessionObservationEvent, SessionObservationEventPayload,
    SessionRevision,
};
use lash_sansio::SessionId;

use crate::session::SessionObservationStreamItem;
use crate::support::{
    Arc, EmbedError, Result, RuntimeErrorCode, RuntimeHandle, SessionCursor, SessionObservation,
    SessionReadView,
};

/// What a feed reads: the session's durable head, this process's live
/// replay, and the open session's resident runtime when there is one.
#[derive(Clone)]
pub(crate) struct FeedSource {
    session_id: SessionId,
    store: lash_core::store::SessionStore,
    live_replay: Arc<dyn LiveReplayStore>,
    transcript_options: crate::transcript::TranscriptProjectionOptions,
    resident: RuntimeHandle,
}

impl FeedSource {
    pub(crate) fn new(resident: RuntimeHandle, store: lash_core::store::SessionStore) -> Self {
        let observation = resident.observe();
        Self {
            session_id: observation.session_id().clone(),
            transcript_options: observation.read_view.transcript_options().clone(),
            live_replay: Arc::clone(&resident.live_replay_store),
            store,
            resident,
        }
    }

    /// The observation revision of the session's durable head.
    async fn durable_revision(&self) -> Result<SessionRevision> {
        Ok(self
            .store
            .load_session_head_meta()
            .await?
            .as_ref()
            .map_or(SessionRevision::new(0), SessionRevision::of_durable_head))
    }

    /// The durable head's revision and read view, from one window read.
    async fn durable_head(&self) -> Result<Option<(SessionRevision, SessionReadView)>> {
        Ok(
            lash_core::facade_support::load_durable_observation_head(&self.store)
                .await?
                .map(|(revision, view)| {
                    (
                        revision,
                        view.with_transcript_options(self.transcript_options.clone()),
                    )
                }),
        )
    }

    /// The durable head with a cursor bound to its revision.
    ///
    /// The resident runtime answers when it holds the head: its cursor sits
    /// right after its own last publication, so the activity a run in this
    /// process published since replays. A resident that trails is brought
    /// to the head first, unless a run holds it; the store answers then.
    pub(crate) async fn snapshot(&self) -> Result<SessionObservation> {
        let revision = self.durable_revision().await?;
        if let Some(observation) = self.resident_at(revision) {
            return Ok(observation);
        }
        let writer = self.resident.writer();
        if let Ok(mut resident) = writer.try_lock() {
            match adopt_committed_head(&mut resident).await {
                Ok(()) => self.resident.adopt_observation_from(&resident),
                Err(error) => tracing::warn!(
                    session_id = %self.session_id,
                    error = %error,
                    "the resident session could not adopt the durable head; the snapshot reads the store",
                ),
            }
            drop(resident);
            if let Some(observation) = self.resident_at(revision) {
                return Ok(observation);
            }
        }
        match self.durable_head().await? {
            Some((revision, read_view)) => Ok(SessionObservation {
                cursor: self.live_replay.current_cursor(&self.session_id, revision),
                read_view,
            }),
            None => Ok(self.resident.observe().session_observation()),
        }
    }

    fn resident_at(&self, revision: SessionRevision) -> Option<SessionObservation> {
        let observation = self.resident.observe();
        (observation.session_revision() >= revision).then(|| observation.session_observation())
    }
}

async fn adopt_committed_head(resident: &mut lash_core::facade_support::LashRuntime) -> Result<()> {
    resident.adopt_committed_head().await?;
    resident.reload_invalidated_resident_session_state().await?;
    Ok(())
}

/// Stream returned by [`ObservableSession::subscribe_and_recover`](crate::ObservableSession::subscribe_and_recover):
/// the session feed from a cursor.
///
/// It yields the live replay's events after the cursor, each durable commit
/// once, and [`SessionObservationStreamItem::Gap`] with the durable head
/// when the cursor cannot be continued. It keeps going after a gap from the
/// gap's cursor.
pub struct SessionObservationStream {
    cursor: SessionCursor,
    #[cfg(test)]
    live_installed: Arc<std::sync::atomic::AtomicBool>,
    state: Option<Box<FeedState>>,
    step: Option<FeedStep>,
}

/// One feed step in flight: it owns the feed's state and hands it back
/// with the item it produced.
type FeedStep = BoxFuture<'static, (Box<FeedState>, Option<Result<SessionObservationStreamItem>>)>;

impl SessionObservationStream {
    pub(crate) fn new(source: FeedSource, cursor: SessionCursor) -> Self {
        #[cfg(test)]
        let live_installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            cursor: cursor.clone(),
            #[cfg(test)]
            live_installed: Arc::clone(&live_installed),
            state: Some(Box::new(FeedState {
                source,
                cursor,
                done: false,
                delivered: None,
                live: None,
                #[cfg(test)]
                live_installed,
            })),
            step: None,
        }
    }

    /// Whether the feed holds a live subscription, also while a step that
    /// owns the feed's state is in flight.
    #[cfg(test)]
    pub(crate) fn live_receiver_installed(&self) -> bool {
        self.live_installed
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Returns the stream's current replay cursor.
    pub fn cursor(&self) -> &SessionCursor {
        &self.cursor
    }
}

impl Stream for SessionObservationStream {
    type Item = Result<SessionObservationStreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.step.is_none() {
            let Some(state) = self.state.take() else {
                return Poll::Ready(None);
            };
            self.step = Some(state.next().boxed());
        }
        let Some(step) = self.step.as_mut() else {
            return Poll::Ready(None);
        };
        let (state, item) = std::task::ready!(step.as_mut().poll(cx));
        self.step = None;
        self.cursor = state.cursor.clone();
        if !state.done {
            self.state = Some(state);
        }
        Poll::Ready(item)
    }
}

/// What the feed does with one live event.
enum Delivery {
    Item(SessionObservationStreamItem),
    /// A commit the consumer already holds.
    Skipped,
    /// A commit whose rows extend a revision the consumer does not hold:
    /// the consumer rebuilds from the durable head.
    Diverged,
}

struct FeedState {
    source: FeedSource,
    /// Where the feed stands: the last delivered or skipped event's live
    /// position, at the newest revision delivered.
    cursor: SessionCursor,
    done: bool,
    /// The newest durable revision the consumer holds, once a subscription
    /// established it: a `Committed` at or before it is a redelivery.
    delivered: Option<SessionRevision>,
    live: Option<LiveReplaySubscription>,
    #[cfg(test)]
    live_installed: Arc<std::sync::atomic::AtomicBool>,
}

impl FeedState {
    fn set_live(&mut self, live: Option<LiveReplaySubscription>) {
        #[cfg(test)]
        self.live_installed
            .store(live.is_some(), std::sync::atomic::Ordering::Release);
        self.live = live;
    }

    async fn next(
        mut self: Box<Self>,
    ) -> (Box<Self>, Option<Result<SessionObservationStreamItem>>) {
        let item = self.step().await;
        if matches!(item, None | Some(Err(_))) {
            self.done = true;
        }
        (self, item)
    }

    async fn step(&mut self) -> Option<Result<SessionObservationStreamItem>> {
        loop {
            if self.done {
                return None;
            }
            let Some(live) = self.live.as_mut() else {
                match self.subscribe().await {
                    Ok(Some(item)) => return Some(Ok(item)),
                    Ok(None) => continue,
                    Err(error) => return Some(Err(error)),
                }
            };
            let item = live.next().await;
            match item {
                None => return None,
                Some(Ok(event)) => match self.deliver(event) {
                    Delivery::Item(item) => return Some(Ok(item)),
                    Delivery::Skipped => {}
                    Delivery::Diverged => {
                        return Some(self.rebuild(LiveReplayGapReason::Unavailable).await);
                    }
                },
                Some(Err(
                    LiveReplayStoreError::SubscriberLagged(_) | LiveReplayStoreError::Closed,
                )) => self.set_live(None),
                Some(Err(error)) => return Some(Err(live_replay_error(error))),
            }
        }
    }

    /// Subscribe from the feed's cursor, judged against the durable head:
    /// a cursor past it, or behind it without a replayed `Committed`
    /// bridging to it, rebuilds from it.
    async fn subscribe(&mut self) -> Result<Option<SessionObservationStreamItem>> {
        let requested = self
            .cursor
            .parse_for_session(&self.source.session_id)
            .map_err(|error| live_replay_error(error.into()))?
            .revision;
        let durable = self.source.durable_revision().await?;
        if requested > durable {
            return self
                .rebuild(LiveReplayGapReason::Unavailable)
                .await
                .map(Some);
        }
        match self
            .source
            .live_replay
            .subscribe_after_cursor(&self.cursor)
            .map_err(live_replay_error)?
        {
            LiveReplaySubscribeOutcome::Subscribed(subscription)
                if requested == durable || subscription.bridges_to(durable) =>
            {
                self.delivered = Some(self.delivered.map_or(requested, |held| held.max(requested)));
                self.set_live(Some(subscription));
                Ok(None)
            }
            LiveReplaySubscribeOutcome::Subscribed(_) => self
                .rebuild(LiveReplayGapReason::Unavailable)
                .await
                .map(Some),
            LiveReplaySubscribeOutcome::Gap(reason) => self.rebuild(reason).await.map(Some),
        }
    }

    /// Deliver one live event. A `Committed` at or below the revision the
    /// consumer holds is a redelivery; one whose delta extends a revision
    /// the consumer does not hold diverged from it.
    fn deliver(&mut self, event: Arc<SessionObservationEvent>) -> Delivery {
        let delivered = self.delivered.unwrap_or(SessionRevision::new(0));
        if let SessionObservationEventPayload::Committed { base_revision, .. } = &event.payload {
            let revision = event.revision();
            if revision <= delivered {
                self.advance_past(&event, delivered);
                return Delivery::Skipped;
            }
            if *base_revision != delivered {
                return Delivery::Diverged;
            }
            self.delivered = Some(revision);
            self.cursor = event.cursor.clone();
            return Delivery::Item(SessionObservationStreamItem::Event(event));
        }
        self.advance_past(&event, delivered);
        Delivery::Item(SessionObservationStreamItem::Event(event))
    }

    /// Move the feed's cursor to a delivered or skipped event's position,
    /// keeping the newest revision delivered.
    fn advance_past(&mut self, event: &SessionObservationEvent, delivered: SessionRevision) {
        let Ok(at) = event.cursor.parse_for_session(&self.source.session_id) else {
            return;
        };
        self.cursor = SessionCursor::new(
            at.replay_incarnation_id,
            &self.source.session_id,
            delivered.max(at.revision),
            at.live_position,
        );
    }

    /// Replace the consumer's state with the durable head: a gap item whose
    /// cursor the feed continues from.
    async fn rebuild(
        &mut self,
        reason: LiveReplayGapReason,
    ) -> Result<SessionObservationStreamItem> {
        let requested_cursor = self.cursor.clone();
        let snapshot = self.source.snapshot().await?;
        let latest_revision = snapshot
            .cursor
            .parse_for_session(&self.source.session_id)
            .map_err(|error| live_replay_error(error.into()))?
            .revision;
        let latest_cursor = self.fresh_cursor(&requested_cursor, snapshot.cursor, latest_revision);
        self.delivered = Some(latest_revision);
        self.cursor = latest_cursor.clone();
        self.set_live(None);
        Ok(SessionObservationStreamItem::Gap {
            observation: SessionObservation {
                read_view: snapshot.read_view,
                cursor: latest_cursor.clone(),
            },
            gap: LiveReplayGap {
                session_id: self.source.session_id.clone(),
                requested_cursor,
                latest_cursor,
                latest_revision,
                reason,
            },
        })
    }

    /// The cursor a gap continues from: the snapshot's, or the live
    /// replay's current one at the snapshot's revision when the snapshot's
    /// is the position that gapped, whichever sits earlier.
    fn fresh_cursor(
        &self,
        requested: &SessionCursor,
        snapshot: SessionCursor,
        revision: SessionRevision,
    ) -> SessionCursor {
        let session_id = &self.source.session_id;
        let current = self.source.live_replay.current_cursor(session_id, revision);
        match (
            requested.parse_for_session(session_id),
            snapshot.parse_for_session(session_id),
            current.parse_for_session(session_id),
        ) {
            (Ok(requested), Ok(at_snapshot), Ok(at_current)) => [
                (at_snapshot.live_position, &snapshot),
                (at_current.live_position, &current),
            ]
            .into_iter()
            .filter(|(position, _)| *position != requested.live_position)
            .min_by_key(|(position, _)| *position)
            .map_or_else(|| snapshot.clone(), |(_, cursor)| cursor.clone()),
            _ => snapshot.clone(),
        }
    }
}

pub(crate) fn live_replay_error(err: LiveReplayStoreError) -> EmbedError {
    EmbedError::Runtime(lash_core::RuntimeError::new(
        RuntimeErrorCode::LiveReplay,
        err.to_string(),
    ))
}
