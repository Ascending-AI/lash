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
//! A `Committed` event carries the commit's entries delta, not the session's
//! read view: the feed delivers it only to a consumer that holds the
//! revision the delta extends, and rebuilds from the durable head when the
//! consumer holds any other. An `AgentFrameSwitched` names the commit that
//! made the switch, and the feed leaves it out for a consumer that holds
//! that commit: a snapshot taken between a commit and its publication
//! already stands on the frame.
//!
//! Delivery is at least once, and every event carries a redelivery identity
//! ([`SessionObservationEventId`]). A stream drops an identity it already
//! delivered within a bounded window, which a host seeds with the identities
//! it applied before a reconnect; a gap or a commit clears the window.
//!
//! Which commits reach the tail is the live replay store's property. The
//! in-memory default holds this process's publications; a host whose
//! sessions run on several processes configures one shared store, and every
//! process's feed then carries every commit.

use std::collections::{BTreeSet, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::BoxFuture;
use futures_util::{FutureExt as _, Stream, StreamExt as _};
use lash_core::facade_support::{LiveReplayGap, SessionObservationSubscription, SessionResume};
use lash_core::{
    LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeOutcome, LiveReplaySubscription, SessionObservationEvent,
    SessionObservationEventPayload, SessionRevision,
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
    work_limits: lash_trace::ObservationWorkLimits,
    session_id: SessionId,
    store: lash_core::store::SessionStore,
    live_replay: Arc<dyn LiveReplayStore>,
    transcript_decoders: crate::transcript::TranscriptDecoders,
    resident: RuntimeHandle,
}

impl FeedSource {
    pub(crate) fn new(
        resident: RuntimeHandle,
        store: lash_core::store::SessionStore,
        work_limits: lash_trace::ObservationWorkLimits,
    ) -> Self {
        let observation = resident.observe();
        Self {
            work_limits,
            session_id: observation.session_id().clone(),
            transcript_decoders: observation.read_view.transcript_decoders().clone(),
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
                        view.with_transcript_decoders(self.transcript_decoders.clone()),
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

    /// The revision `cursor` names, refused when it is malformed or names
    /// another session.
    fn requested_revision(&self, cursor: &SessionCursor) -> Result<SessionRevision> {
        Ok(cursor
            .parse_for_session(&self.session_id)
            .map_err(|error| live_replay_error(error.into()))?
            .revision)
    }

    /// The live replay after `cursor`, judged against the durable head read
    /// now: a cursor past the head, or behind it without a replayed
    /// `Committed` bridging to it, is a gap rebuilt from the head.
    pub(crate) async fn resume(&self, cursor: &SessionCursor) -> Result<SessionResume> {
        let requested = self.requested_revision(cursor)?;
        let durable = self.durable_revision().await?;
        let reason = if requested > durable {
            LiveReplayGapReason::Unavailable
        } else {
            match self
                .live_replay
                .replay_after_cursor(cursor)
                .await
                .map_err(live_replay_error)?
            {
                LiveReplayOutcome::Replayed(events)
                    if requested == durable
                        || events.iter().any(|event| bridges(event, durable)) =>
                {
                    return Ok(SessionResume::Replayed { events });
                }
                LiveReplayOutcome::Replayed(_) => LiveReplayGapReason::Unavailable,
                LiveReplayOutcome::Gap(reason) => reason,
            }
        };
        let (observation, gap) = self.gap(cursor, reason).await?;
        Ok(SessionResume::Gap { observation, gap })
    }

    /// A live replay subscription after `cursor`, judged against the
    /// durable head read now, as [`resume`](Self::resume) judges a replay.
    pub(crate) async fn subscribe(
        &self,
        cursor: &SessionCursor,
    ) -> Result<SessionObservationSubscription> {
        let requested = self.requested_revision(cursor)?;
        let durable = self.durable_revision().await?;
        let reason = if requested > durable {
            LiveReplayGapReason::Unavailable
        } else {
            match self
                .live_replay
                .subscribe_after_cursor(cursor)
                .await
                .map_err(live_replay_error)?
            {
                LiveReplaySubscribeOutcome::Subscribed(subscription)
                    if requested == durable || subscription.bridges_to(durable) =>
                {
                    return Ok(SessionObservationSubscription::Subscribed(subscription));
                }
                LiveReplaySubscribeOutcome::Subscribed(_) => LiveReplayGapReason::Unavailable,
                LiveReplaySubscribeOutcome::Gap(reason) => reason,
            }
        };
        let (observation, gap) = self.gap(cursor, reason).await?;
        Ok(SessionObservationSubscription::Gap { observation, gap })
    }

    /// A gap from `requested`: the durable head, and the cursor a reader
    /// continues from.
    async fn gap(
        &self,
        requested: &SessionCursor,
        reason: LiveReplayGapReason,
    ) -> Result<(SessionObservation, LiveReplayGap)> {
        let snapshot = self.snapshot().await?;
        let latest_revision = self.requested_revision(&snapshot.cursor)?;
        let latest_cursor = self
            .fresh_cursor(requested, snapshot.cursor, latest_revision)
            .await;
        Ok((
            SessionObservation {
                read_view: snapshot.read_view,
                cursor: latest_cursor.clone(),
            },
            LiveReplayGap {
                session_id: self.session_id.clone(),
                requested_cursor: requested.clone(),
                latest_cursor,
                latest_revision,
                reason,
            },
        ))
    }

    /// The cursor a gap continues from: the earlier of the snapshot's and
    /// the live replay's current one at the snapshot's revision, leaving out
    /// the position that gapped and any the live replay cannot continue.
    /// A snapshot's cursor can trail a discontinuity it never saw (a
    /// resident runtime's, behind another process's activity and an
    /// invalidation after it); continuing from it would gap again. The
    /// current cursor is the fallback: it starts the live replay's fresh
    /// continuity.
    async fn fresh_cursor(
        &self,
        requested: &SessionCursor,
        snapshot: SessionCursor,
        revision: SessionRevision,
    ) -> SessionCursor {
        let session_id = &self.session_id;
        let current = self.live_replay.current_cursor(session_id, revision);
        let position =
            |cursor: &SessionCursor| Some(cursor.parse_for_session(session_id).ok()?.live_position);
        let (Some(gapped), Some(at_snapshot), Some(at_current)) =
            (position(requested), position(&snapshot), position(&current))
        else {
            return snapshot;
        };
        let mut candidates = [(at_snapshot, &snapshot), (at_current, &current)];
        candidates.sort_by_key(|(position, _)| *position);
        for (position, candidate) in candidates {
            if position == gapped {
                continue;
            }
            if matches!(
                self.live_replay.replay_after_cursor(candidate).await,
                Ok(LiveReplayOutcome::Replayed(_))
            ) {
                return candidate.clone();
            }
        }
        current.clone()
    }
}

/// Whether `event` is a `Committed` at or after `revision`: the evidence
/// that a replay from a cursor behind `revision` bridges to it.
fn bridges(event: &SessionObservationEvent, revision: SessionRevision) -> bool {
    event.revision() >= revision
        && matches!(
            event.payload,
            SessionObservationEventPayload::Committed { .. }
        )
}

async fn adopt_committed_head(resident: &mut lash_core::facade_support::LashRuntime) -> Result<()> {
    resident.adopt_committed_head().await?;
    resident.reload_invalidated_resident_session_state().await?;
    Ok(())
}

/// At-least-once delivery identity of one observation event.
///
/// The replay-store incarnation makes it safe to persist across process
/// restarts: a newly constructed store may reuse a cursor, but it cannot
/// reproduce an old identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionObservationEventId {
    /// Session that produced the event.
    pub session_id: SessionId,
    /// Replay-store incarnation that produced the event.
    pub replay_incarnation_id: String,
    /// Replay cursor of the event.
    pub cursor: String,
}

impl SessionObservationEventId {
    /// The identity of `event`.
    pub fn of(event: &SessionObservationEvent) -> Self {
        Self {
            session_id: event.session_id(),
            replay_incarnation_id: event.replay_incarnation_id().to_string(),
            cursor: event.cursor.to_string(),
        }
    }
}

/// The bounded window of identities a stream delivered or its host applied.
#[derive(Default)]
struct AppliedEventIds {
    limits: lash_trace::ObservationWorkLimits,
    ids: BTreeSet<SessionObservationEventId>,
    order: VecDeque<SessionObservationEventId>,
}

impl AppliedEventIds {
    fn insert(&mut self, id: SessionObservationEventId) -> bool {
        if !self.ids.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > self.limits.session_dedup_ids {
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

    /// Whether the stream delivers `item`: a gap always, clearing the
    /// window, since its snapshot is authoritative; an event only when its
    /// identity is new, and a commit clears the window it settles.
    fn admit(&mut self, item: &SessionObservationStreamItem) -> bool {
        match item {
            SessionObservationStreamItem::Gap { .. } => {
                self.clear();
                true
            }
            SessionObservationStreamItem::Event(event) => {
                let id = SessionObservationEventId::of(event);
                if !self.insert(id.clone()) {
                    return false;
                }
                if matches!(
                    event.payload,
                    SessionObservationEventPayload::Committed { .. }
                ) {
                    self.clear();
                    self.insert(id);
                }
                true
            }
        }
    }
}

/// Stream returned by [`ObservableSession::subscribe_and_recover`](crate::ObservableSession::subscribe_and_recover):
/// the session feed from a cursor.
///
/// It yields the live replay's events after the cursor, each durable commit
/// once, and [`SessionObservationStreamItem::Gap`] with the durable head
/// when the cursor cannot be continued. It keeps going after a gap from the
/// gap's cursor. It never yields an event identity twice within its bounded
/// window.
///
/// Dropping the stream only disconnects observation; it never cancels work.
pub struct SessionObservationStream {
    cursor: SessionCursor,
    state: Option<Box<FeedState>>,
    step: Option<FeedStep>,
    applied: AppliedEventIds,
}

/// One feed step in flight: it owns the feed's state and hands it back
/// with the item it produced.
type FeedStep = BoxFuture<'static, (Box<FeedState>, Option<Result<SessionObservationStreamItem>>)>;

impl SessionObservationStream {
    pub(crate) fn new(source: FeedSource, cursor: SessionCursor) -> Self {
        let limits = source.work_limits;
        Self {
            cursor: cursor.clone(),
            state: Some(Box::new(FeedState {
                source,
                cursor,
                done: false,
                delivered: None,
                live: None,
            })),
            step: None,
            applied: AppliedEventIds {
                limits,
                ..Default::default()
            },
        }
    }

    /// Configure deduplication for this stream. Oldest identities are dropped
    /// when the window shrinks; zero disables deduplication. Other work limits
    /// are resolved by the core or the replay store.
    pub fn with_work_limits(mut self, limits: crate::tracing::ObservationWorkLimits) -> Self {
        self.applied.limits = limits;
        while self.applied.order.len() > limits.session_dedup_ids {
            if let Some(id) = self.applied.order.pop_front() {
                self.applied.ids.remove(&id);
            }
        }
        self
    }

    /// Seed identities the host already applied, so a reconnect's
    /// redelivery is idempotent even when the host's persisted cursor
    /// trails individual applied events. The stream keeps a bounded recent
    /// window and clears it at a gap, whose snapshot is authoritative.
    pub fn with_applied_event_ids(
        mut self,
        ids: impl IntoIterator<Item = SessionObservationEventId>,
    ) -> Self {
        for id in ids {
            self.applied.insert(id);
        }
        self
    }

    /// Returns the stream's current replay cursor.
    pub fn cursor(&self) -> &SessionCursor {
        &self.cursor
    }
}

impl Stream for SessionObservationStream {
    type Item = Result<SessionObservationStreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
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
            if let Some(Ok(delivered)) = &item
                && !self.applied.admit(delivered)
            {
                continue;
            }
            return Poll::Ready(item);
        }
    }
}

/// What the feed does with one live event.
enum Delivery {
    Item(SessionObservationStreamItem),
    /// A commit the consumer already holds, or that commit's frame switch.
    Skipped,
    /// A commit whose entries extend a revision the consumer does not hold:
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
}

impl FeedState {
    fn set_live(&mut self, live: Option<LiveReplaySubscription>) {
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
    /// a gap replaces the consumer's state with the head, and the feed
    /// continues from the gap's cursor.
    async fn subscribe(&mut self) -> Result<Option<SessionObservationStreamItem>> {
        let requested = self.source.requested_revision(&self.cursor)?;
        match self.source.subscribe(&self.cursor).await? {
            SessionObservationSubscription::Subscribed(subscription) => {
                self.delivered = Some(self.delivered.map_or(requested, |held| held.max(requested)));
                self.set_live(Some(subscription));
                Ok(None)
            }
            SessionObservationSubscription::Gap { observation, gap } => {
                Ok(Some(self.adopt_gap(observation, gap)))
            }
        }
    }

    /// Replace the consumer's state with the durable head: a gap item whose
    /// cursor the feed continues from.
    async fn rebuild(
        &mut self,
        reason: LiveReplayGapReason,
    ) -> Result<SessionObservationStreamItem> {
        let (observation, gap) = self.source.gap(&self.cursor, reason).await?;
        Ok(self.adopt_gap(observation, gap))
    }

    /// Continue from `gap`'s cursor, holding its revision.
    fn adopt_gap(
        &mut self,
        observation: SessionObservation,
        gap: LiveReplayGap,
    ) -> SessionObservationStreamItem {
        self.delivered = Some(gap.latest_revision);
        self.cursor = gap.latest_cursor.clone();
        self.set_live(None);
        SessionObservationStreamItem::Gap { observation, gap }
    }

    /// Deliver one live event. A `Committed` at or below the revision the
    /// consumer holds is a redelivery, and so is the frame switch of a
    /// commit at or below it; a `Committed` whose delta extends a revision
    /// the consumer does not hold diverged from it.
    fn deliver(&mut self, event: Arc<SessionObservationEvent>) -> Delivery {
        let delivered = self.delivered.unwrap_or(SessionRevision::new(0));
        if let SessionObservationEventPayload::AgentFrameSwitched {
            commit: Some(commit),
            ..
        } = &event.payload
            && *commit <= delivered
        {
            self.advance_past(&event, delivered);
            return Delivery::Skipped;
        }
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
}

pub(crate) fn live_replay_error(err: LiveReplayStoreError) -> EmbedError {
    EmbedError::Runtime(lash_core::RuntimeError::new(
        RuntimeErrorCode::LiveReplay,
        err.to_string(),
    ))
}
