//! The session feed: observation whose snapshot is the session's durable
//! head and whose tail is the configured live replay store (FIG-5090).
//!
//! A feed's snapshot is the durable head, paired with a cursor bound to its
//! revision ([`ObservableSession::snapshot`](crate::ObservableSession::snapshot)).
//! The cursor stands at the head's commit in the live replay, so every event
//! the replay orders after that commit is news to the snapshot's holder and
//! replays (FIG-5322). Its tail is the session's live replay after that
//! cursor. The durable head is the authority a cursor is judged against: a
//! cursor behind it is continued only when the replay carries a `Committed`
//! event bridging to it, and every gap's replacement snapshot is the durable
//! head, never a resident projection that may trail a commit another
//! process made.
//!
//! A `Committed` event carries the commit's entries delta, not the session's
//! read view: a replay or a stream delivers it only to a consumer that holds
//! the revision the delta extends, leaves it out for one that already holds
//! the commit (FIG-5387), and rebuilds from the durable head when the
//! consumer holds any other revision. An `AgentFrameSwitched` names the
//! commit that made the switch, and the feed leaves it out for a consumer
//! that holds that commit: a snapshot taken between a commit and its
//! publication already stands on the frame and holds the commit.
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
use lash_vocabulary::identity::SessionId;

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
    #[cfg(test)]
    rebuild_barrier: Option<Arc<tokio::sync::Notify>>,
    session_id: SessionId,
    store: lash_core::store::SessionStore,
    live_replay: Arc<dyn LiveReplayStore>,
    transcript_decoders: crate::transcript::TranscriptDecoders,
    resident: RuntimeHandle,
    /// The commits this node is still publishing.
    published_heads: Arc<lash_core::runtime::durable::services::PublishedHeads>,
}

impl FeedSource {
    pub(crate) fn new(
        resident: RuntimeHandle,
        store: lash_core::store::SessionStore,
        work_limits: lash_trace::ObservationWorkLimits,
        published_heads: Arc<lash_core::runtime::durable::services::PublishedHeads>,
    ) -> Self {
        let observation = resident.observe();
        Self {
            work_limits,
            #[cfg(test)]
            rebuild_barrier: None,
            published_heads,
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

    /// The durable head with a cursor at its commit in the live replay
    /// ([`commit_cursor`](Self::commit_cursor)).
    pub(crate) async fn snapshot(&self) -> Result<SessionObservation> {
        let (revision, read_view) = self.head_view().await?;
        Ok(SessionObservation {
            cursor: self.commit_cursor(revision).await,
            read_view,
        })
    }

    /// The durable head's revision and read view. The resident runtime
    /// answers when it holds the head; a resident that trails is brought to
    /// the head first, unless a run holds it, and the store answers then.
    async fn head_view(&self) -> Result<(SessionRevision, SessionReadView)> {
        let revision = self.durable_revision().await?;
        if let Some(head) = self.resident_at(revision) {
            return Ok(head);
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
            if let Some(head) = self.resident_at(revision) {
                return Ok(head);
            }
        }
        match self.durable_head().await? {
            Some(head) => Ok(head),
            None => {
                let observation = self.resident.observe();
                Ok((
                    observation.session_revision(),
                    observation.read_view.clone(),
                ))
            }
        }
    }

    /// Where a holder of the durable head at `revision` stands in the live
    /// replay: at the head's commit, so every event the replay orders after
    /// it replays, and none before it does.
    ///
    /// The replay's retained window is read from its start, up to the first
    /// event stamped past `revision`:
    /// - the window holds the head's `Committed`: the cursor is that event's;
    /// - the commit is still to come, because this node is publishing it or
    ///   the window holds an earlier commit and not this one: the cursor is
    ///   the window's end, and the commit's publication, when it lands, is a
    ///   redelivery the holder is not handed;
    /// - otherwise retention trimmed the commit, so everything retained came
    ///   after it: the cursor is the window's start.
    ///
    /// A store that cannot name its window's start
    /// ([`LiveReplayStore::earliest_cursor`]) answers its head there, and
    /// the cursor is that head.
    async fn commit_cursor(&self, revision: SessionRevision) -> SessionCursor {
        let session_id = &self.session_id;
        let publishing = self.published_heads.window(session_id, revision).is_open();
        let start = self.live_replay.earliest_cursor(session_id, revision);
        let Ok(LiveReplayOutcome::Replayed(events)) =
            self.live_replay.replay_after_cursor(&start).await
        else {
            return self.live_replay.current_cursor(session_id, revision);
        };
        let mut end = None;
        let mut earlier_commit = false;
        for event in &events {
            if event.revision() > revision {
                break;
            }
            if matches!(
                event.payload,
                SessionObservationEventPayload::Committed { .. }
            ) {
                if event.revision() == revision {
                    return self.at_revision(&event.cursor, revision).unwrap_or(start);
                }
                earlier_commit = true;
            }
            end = Some(&event.cursor);
        }
        match end {
            Some(end) if publishing || earlier_commit => {
                self.at_revision(end, revision).unwrap_or(start)
            }
            _ => start,
        }
    }

    /// `cursor`'s live position, bound to `revision`.
    fn at_revision(
        &self,
        cursor: &SessionCursor,
        revision: SessionRevision,
    ) -> Option<SessionCursor> {
        let at = cursor.parse_for_session(&self.session_id).ok()?;
        Some(SessionCursor::new(
            at.replay_incarnation_id,
            &self.session_id,
            revision,
            at.live_position,
        ))
    }

    /// Attach to the durable head and replay all retained provisional
    /// evidence, including language executions that completed before attach.
    pub(crate) async fn attach(&self) -> Result<SessionObservation> {
        let mut observation = self.snapshot().await?;
        let revision = self.requested_revision(&observation.cursor)?;
        observation.cursor = self.live_replay.earliest_cursor(&self.session_id, revision);
        Ok(observation)
    }

    fn resident_at(&self, revision: SessionRevision) -> Option<(SessionRevision, SessionReadView)> {
        let observation = self.resident.observe();
        (observation.session_revision() >= revision).then(|| {
            (
                observation.session_revision(),
                observation.read_view.clone(),
            )
        })
    }

    /// The revision `cursor` names, refused when it is malformed or names
    /// another session.
    fn requested_revision(&self, cursor: &SessionCursor) -> Result<SessionRevision> {
        Ok(cursor
            .parse_for_session(&self.session_id)
            .map_err(|error| live_replay_error(error.into()))?
            .revision)
    }

    /// The live replay after `cursor`, judged against the durable head: a
    /// cursor past the head, or behind it without a replayed `Committed`
    /// bridging to it, is a gap rebuilt from the head. The replay is then
    /// judged as a stream judges it ([`judge`]): a commit the cursor's
    /// revision already holds is left out, and one that extends another
    /// revision is a gap.
    pub(crate) async fn resume(&self, cursor: &SessionCursor) -> Result<SessionResume> {
        let live_replay = &self.live_replay;
        let replayed = self
            .continued(
                cursor,
                move || async move {
                    Ok(match live_replay.replay_after_cursor(cursor).await? {
                        LiveReplayOutcome::Replayed(events) => Ok(events),
                        LiveReplayOutcome::Gap(reason) => Err(reason),
                    })
                },
                |events, durable| events.iter().any(|event| bridges(event, durable)),
            )
            .await?;
        let reason = match replayed {
            Ok(events) => match self.held_suffix(cursor, events)? {
                Some(events) => return Ok(SessionResume::Replayed { events }),
                None => LiveReplayGapReason::Unavailable,
            },
            Err(reason) => reason,
        };
        let (observation, gap) = self.gap(cursor, reason).await?;
        Ok(SessionResume::Gap { observation, gap })
    }

    /// What of `events`, replayed after `cursor`, reaches the consumer that
    /// holds `cursor`'s revision; `None` when a commit diverged from it.
    fn held_suffix(
        &self,
        cursor: &SessionCursor,
        events: Vec<Arc<SessionObservationEvent>>,
    ) -> Result<Option<Vec<Arc<SessionObservationEvent>>>> {
        let mut held = self.requested_revision(cursor)?;
        let mut delivered = Vec::with_capacity(events.len());
        for event in events {
            match judge(&event, held) {
                Judged::Redelivered => {}
                Judged::Diverged => return Ok(None),
                Judged::Commit(revision) => {
                    held = revision;
                    delivered.push(event);
                }
                Judged::Other => delivered.push(event),
            }
        }
        Ok(Some(delivered))
    }

    /// A live replay subscription after `cursor`, judged against the
    /// durable head as [`resume`](Self::resume) judges a replay.
    pub(crate) async fn subscribe(
        &self,
        cursor: &SessionCursor,
    ) -> Result<SessionObservationSubscription> {
        let live_replay = &self.live_replay;
        let subscribed = self
            .continued(
                cursor,
                move || async move {
                    Ok(match live_replay.subscribe_after_cursor(cursor).await? {
                        LiveReplaySubscribeOutcome::Subscribed(subscription) => Ok(subscription),
                        LiveReplaySubscribeOutcome::Gap(reason) => Err(reason),
                    })
                },
                LiveReplaySubscription::bridges_to,
            )
            .await?;
        match subscribed {
            Ok(subscription) => Ok(SessionObservationSubscription::Subscribed(subscription)),
            Err(reason) => {
                let (observation, gap) = self.gap(cursor, reason).await?;
                Ok(SessionObservationSubscription::Gap { observation, gap })
            }
        }
    }

    /// What `read` finds in the live replay after `cursor`, once it
    /// continues the cursor to the durable head, or why it cannot.
    ///
    /// The head is read first, then its publication window is taken, then
    /// the replay is read. A replay that does not bridge to the head is no
    /// gap while this node is still publishing a commit behind that head:
    /// the feed waits the publication out and judges again, from a new
    /// read of the head, so a head a later commit moved is judged by that
    /// commit's own publication (`PublicationWindow`, FIG-5605, FIG-5626).
    /// With no publication in flight the replay is final: another node
    /// committed, or the publication was lost, and the reader rebuilds
    /// from the head.
    async fn continued<T, Read>(
        &self,
        cursor: &SessionCursor,
        read: impl Fn() -> Read,
        bridges_to: impl Fn(&T, SessionRevision) -> bool,
    ) -> Result<std::result::Result<T, LiveReplayGapReason>>
    where
        Read: Future<
            Output = std::result::Result<
                std::result::Result<T, LiveReplayGapReason>,
                LiveReplayStoreError,
            >,
        >,
    {
        let requested = self.requested_revision(cursor)?;
        loop {
            let durable = self.durable_revision().await?;
            if requested > durable {
                return Ok(Err(LiveReplayGapReason::Unavailable));
            }
            let window = self.published_heads.window(&self.session_id, durable);
            let replay = match read().await.map_err(live_replay_error)? {
                Ok(replay) => replay,
                Err(reason) => return Ok(Err(reason)),
            };
            if requested == durable || bridges_to(&replay, durable) {
                return Ok(Ok(replay));
            }
            if !window.closed().await {
                return Ok(Err(LiveReplayGapReason::Unavailable));
            }
        }
    }

    /// A gap from `requested`: the durable head, and the cursor at its
    /// commit a reader continues from.
    async fn gap(
        &self,
        requested: &SessionCursor,
        reason: LiveReplayGapReason,
    ) -> Result<(SessionObservation, LiveReplayGap)> {
        #[cfg(test)]
        if let Some(barrier) = &self.rebuild_barrier {
            barrier.notified().await;
        }
        let snapshot = self.snapshot().await?;
        let latest_revision = self.requested_revision(&snapshot.cursor)?;
        let gap = LiveReplayGap {
            session_id: self.session_id.clone(),
            requested_cursor: requested.clone(),
            latest_cursor: snapshot.cursor.clone(),
            latest_revision,
            reason,
        };
        Ok((snapshot, gap))
    }
}

/// What a consumer that holds one revision does with one live event.
enum Judged {
    /// A commit extending the held revision: the consumer now holds this one.
    Commit(SessionRevision),
    /// Any event that is not a commit or a commit's frame switch.
    Other,
    /// A commit the consumer holds, or that commit's frame switch.
    Redelivered,
    /// A commit whose entries extend a revision the consumer does not hold.
    Diverged,
}

/// Judge `event` for a consumer that holds `held`: a `Committed` at or below
/// it is a redelivery, and so is the frame switch of a commit at or below
/// it; a `Committed` whose delta extends a revision other than `held`
/// diverged from the consumer.
fn judge(event: &SessionObservationEvent, held: SessionRevision) -> Judged {
    match &event.payload {
        SessionObservationEventPayload::AgentFrameSwitched {
            commit: Some(commit),
            ..
        } if *commit <= held => Judged::Redelivered,
        SessionObservationEventPayload::Committed { base_revision, .. } => {
            let revision = event.revision();
            if revision <= held {
                Judged::Redelivered
            } else if *base_revision != held {
                Judged::Diverged
            } else {
                Judged::Commit(revision)
            }
        }
        _ => Judged::Other,
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

    /// Deliver one live event to the consumer, as [`judge`] judges it for
    /// the revision the consumer holds.
    fn deliver(&mut self, event: Arc<SessionObservationEvent>) -> Delivery {
        let delivered = self.delivered.unwrap_or(SessionRevision::new(0));
        match judge(&event, delivered) {
            Judged::Redelivered => {
                self.advance_past(&event, delivered);
                Delivery::Skipped
            }
            Judged::Diverged => Delivery::Diverged,
            Judged::Commit(revision) => {
                self.delivered = Some(revision);
                self.cursor = event.cursor.clone();
                Delivery::Item(SessionObservationStreamItem::Event(event))
            }
            Judged::Other => {
                self.advance_past(&event, delivered);
                Delivery::Item(SessionObservationStreamItem::Event(event))
            }
        }
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

#[cfg(test)]
mod cancellation_law {
    use crate::tests::*;
    use lash_vocabulary::schema_contract::*;

    /// A consumed divergence must complete its durable rebuild while tool
    /// traffic arrives; dropping the read loses the only commit notification.
    #[tokio::test]
    async fn divergence_rebuild_completes_under_concurrent_tool_live_traffic() -> Result<()> {
        let core = explicit_ephemeral_facets(crate::LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(
            crate::testing::TestProvider::builder()
                .kind("embed-test")
                .complete(|_| async { Ok(lash_core::llm::types::LlmResponse::default()) })
                .build()
                .into_handle(),
            mock_llm_profile_spec(),
        )
        .build(crate::tests::host::runtime_lease_owner())?;
        let id = SessionId::from("tool-live-divergence");
        let session = created_session(&core, id.clone()).await.open().await?;
        let snapshot = session.observe().snapshot().await?;
        let mut feed = session
            .observe()
            .subscribe_and_recover(snapshot.cursor.clone());
        let barrier = Arc::new(tokio::sync::Notify::new());
        let state = feed.state.as_mut().expect("unpolled feed");
        state.source.rebuild_barrier = Some(barrier.clone());
        state.delivered = Some(SessionRevision::new(0));
        state.live = Some(LiveReplaySubscription::new(
            vec![Arc::new(
                SessionObservationEvent::new(
                    None,
                    SessionCursor::new("divergence", &id, SessionRevision::new(2), 1),
                    SessionObservationEventPayload::Committed {
                        base_revision: SessionRevision::new(1),
                        entries: Vec::new(),
                    },
                )
                .expect("valid divergence event"),
            )],
            futures_util::stream::pending(),
        ));
        let owner = lash_core::ExecutionOwner::SessionFrame {
            session_id: id,
            agent_frame_id: lash_core::FrameNodeId::new("live-frame").expect("frame"),
        };
        let mut live = core
            .tool_live()
            .subscribe(lash_core::ToolLiveFilter::Owner(owner.clone()))
            .await
            .expect("tool lane");
        let publisher = core.tool_live().publisher_for_test(lash_core::CallAddress {
            owner,
            call_id: lash_core::ToolCallId::fixture("fixture-call"),
            attempt: 1,
        });
        assert!(futures_util::poll!(feed.next()).is_pending());
        publisher.publish(lash_core::ToolLiveFact::Progress {
            progress: 1.0,
            total: None,
            message: None,
        });
        publisher.finish();
        let _ = live
            .next()
            .await
            .expect("concurrent tool traffic")
            .expect("tool emission");
        assert!(
            futures_util::poll!(feed.next()).is_pending(),
            "tool traffic cancelled the durable divergence rebuild"
        );
        barrier.notify_one();
        assert!(matches!(
            feed.next().await,
            Some(Ok(SessionObservationStreamItem::Gap { .. }))
        ));
        core.shutdown().await?;
        Ok(())
    }
}
