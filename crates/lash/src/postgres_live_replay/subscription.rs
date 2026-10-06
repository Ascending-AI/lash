//! Replay and subscription: reads judged against the head in one snapshot,
//! and a live tail that re-reads the table whenever its doorbell rings.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::Stream;
use lash_core::runtime::ParsedSessionCursor;
use lash_core::{
    LiveReplayGapReason, LiveReplayStoreError, SessionObservationEvent, SessionRevision,
};
use lash_sansio::SessionId;
use sqlx::Row as _;

use super::schema::{Incarnation, column, db_error, ensure_incarnation, micros, position};
use super::{BellGuard, Shared};

/// What a read after a position found.
pub(super) enum Read {
    Gap(LiveReplayGapReason),
    /// The retained events after the position, in order.
    Events(Vec<Arc<SessionObservationEvent>>),
}

/// The retained events of `cursor`'s session after its position, at most
/// one window's worth, or the gap that prevents continuing from it.
///
/// One statement reads the incarnation, the head, the window's start by
/// database time and the rows, so the judgment and the rows are one
/// snapshot.
pub(super) async fn read(
    shared: &Shared,
    cursor: &ParsedSessionCursor<'_>,
) -> Result<Read, LiveReplayStoreError> {
    let session = cursor.session_id.as_str();
    let rows = sqlx::query(&shared.sql.read)
        .bind(session)
        .bind(column(cursor.live_position))
        .bind(micros(shared.config.max_age))
        .bind(column(shared.config.max_events_per_session as u64))
        .fetch_all(&shared.pool)
        .await
        .map_err(db_error("read"))?;
    let Some(first) = rows.first() else {
        // No incarnation row: the tables were recreated under this store.
        let incarnation = ensure_incarnation(&shared.pool, &shared.sql).await?;
        shared.adopt(incarnation);
        return Ok(Read::Gap(LiveReplayGapReason::Unavailable));
    };
    if !first.get::<bool, _>("valid") {
        let incarnation = ensure_incarnation(&shared.pool, &shared.sql).await?;
        shared.adopt(incarnation);
        return Ok(Read::Gap(LiveReplayGapReason::Unavailable));
    }
    let incarnation = Incarnation {
        id: first.get("incarnation_id"),
        watermark: position(first.get("watermark")),
    };
    shared.adopt(incarnation.clone());
    if cursor.replay_incarnation_id != incarnation.id {
        return Ok(Read::Gap(LiveReplayGapReason::Unavailable));
    }
    let requested = cursor.live_position;
    let Some(tail) = first.get::<Option<i64>, _>("tail_position").map(position) else {
        // A session without a head continues only from the watermark a
        // head would start at.
        return Ok(if requested == incarnation.watermark {
            Read::Events(Vec::new())
        } else {
            Read::Gap(LiveReplayGapReason::Unavailable)
        });
    };
    let floor = position(
        first
            .get::<Option<i64>, _>("floor_position")
            .unwrap_or_default(),
    );
    let first_live = position(
        first
            .get::<Option<i64>, _>("first_live")
            .unwrap_or_default(),
    );
    shared.observed(&cursor.session_id, floor, first_live);
    if requested < floor || requested > tail {
        return Ok(Read::Gap(LiveReplayGapReason::Unavailable));
    }
    if requested + 1 < first_live {
        return Ok(Read::Gap(LiveReplayGapReason::Trimmed));
    }
    let mut events = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(event_position) = row.get::<Option<i64>, _>("position").map(position) else {
            continue;
        };
        let revision = SessionRevision::new(position(row.get("revision")));
        events.push(super::codec::decode(
            incarnation.cursor(&cursor.session_id, revision, event_position),
            row.get("turn_id"),
            &row.get::<Vec<u8>, _>("payload"),
        )?);
    }
    Ok(Read::Events(events))
}

/// A subscription's live tail: it waits for its session's doorbell and
/// reads the rows past the last position it delivered. A missed doorbell
/// only delays it; the table is the log.
struct LiveTail {
    shared: Arc<Shared>,
    session_id: SessionId,
    incarnation: String,
    last: u64,
    bell: BellGuard,
    queue: VecDeque<Arc<SessionObservationEvent>>,
    done: bool,
}

type LiveItem = Result<Arc<SessionObservationEvent>, LiveReplayStoreError>;

impl LiveTail {
    async fn next(&mut self) -> Option<LiveItem> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(Ok(event));
            }
            if self.done {
                return None;
            }
            if self.bell.receiver.changed().await.is_err() {
                // The store is gone.
                return self.end(LiveReplayStoreError::Closed);
            }
            let cursor = Incarnation {
                id: self.incarnation.clone(),
                watermark: 0,
            }
            .cursor(&self.session_id, SessionRevision::new(0), self.last);
            let parsed = match cursor.parse() {
                Ok(parsed) => parsed,
                Err(error) => return self.end(error.into()),
            };
            match read(&self.shared, &parsed).await {
                Ok(Read::Events(events)) => {
                    if let Some(last) = events.last() {
                        self.last = last
                            .cursor
                            .parse()
                            .map(|parsed| parsed.live_position)
                            .unwrap_or(self.last);
                        // A full window may have more behind it.
                        if events.len() >= self.shared.config.max_events_per_session {
                            self.bell.receiver.mark_changed();
                        }
                    }
                    self.queue.extend(events);
                }
                // The window moved past this subscriber: it lagged.
                Ok(Read::Gap(LiveReplayGapReason::Trimmed)) => {
                    return self.end(LiveReplayStoreError::SubscriberLagged(1));
                }
                // Invalidated, forgotten or rotated: the generation it
                // followed ended.
                Ok(Read::Gap(LiveReplayGapReason::Unavailable)) => {
                    return self.end(LiveReplayStoreError::Closed);
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        %error,
                        "a live replay tail could not read its session; the observer resubscribes"
                    );
                    return self.end(LiveReplayStoreError::Closed);
                }
            }
        }
    }

    fn end(&mut self, error: LiveReplayStoreError) -> Option<LiveItem> {
        self.done = true;
        Some(Err(error))
    }
}

/// The live tail after `last` for a subscriber whose doorbell `bell` was
/// registered before its replay was read.
pub(super) fn live_tail(
    shared: Arc<Shared>,
    session_id: SessionId,
    incarnation: String,
    last: u64,
    bell: BellGuard,
) -> impl Stream<Item = LiveItem> + Send + 'static {
    let tail = LiveTail {
        shared,
        session_id,
        incarnation,
        last,
        bell,
        queue: VecDeque::new(),
        done: false,
    };
    futures_util::stream::unfold(tail, |mut tail| async move {
        let item = tail.next().await?;
        Some((item, tail))
    })
}
