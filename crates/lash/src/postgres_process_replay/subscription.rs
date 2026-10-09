//! Replay and subscription: reads judged against the head in one snapshot,
//! and a live tail that re-reads the table whenever its doorbell rings.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::Stream;
use lash_core::runtime::ParsedProcessObservationCursor;
use lash_core::{
    ProcessId, ProcessObservationEvent, ProcessReplayGapReason, ProcessReplayStoreError,
    ProcessSequence,
};
use sqlx::Row as _;

use super::schema::{Incarnation, column, db_error, ensure_incarnation, micros, position};
use super::{BellGuard, Shared};

/// What a read after a position found.
pub(super) enum Read {
    Gap(ProcessReplayGapReason),
    /// The retained events after the position, in order.
    Events(Vec<Arc<ProcessObservationEvent>>),
}

/// The retained events of `cursor`'s process after its position, at most
/// one window's worth, or the gap that prevents continuing from it.
///
/// One statement reads the incarnation, the head, the window's start by
/// database time and the rows, so the judgment and the rows are one
/// snapshot.
pub(super) async fn read(
    shared: &Shared,
    cursor: &ParsedProcessObservationCursor<'_>,
) -> Result<Read, ProcessReplayStoreError> {
    let rows = sqlx::query(&shared.sql.read)
        .bind(cursor.process_id.as_str())
        .bind(column(cursor.live_position))
        .bind(micros(shared.config.max_age))
        .bind(column(shared.config.max_events_per_process as u64))
        .fetch_all(&shared.pool)
        .await
        .map_err(db_error("read"))?;
    let Some(first) = rows.first().filter(|first| first.get::<bool, _>("valid")) else {
        // No incarnation row, or a sentinel that is gone: the history
        // behind every cursor is lost.
        ensure_incarnation(&shared.pool, &shared.sql).await?;
        return Ok(Read::Gap(ProcessReplayGapReason::Unavailable));
    };
    let incarnation = Incarnation {
        id: first.get("incarnation_id"),
        watermark: position(first.get("watermark")),
    };
    if cursor.replay_incarnation_id != incarnation.id {
        return Ok(Read::Gap(ProcessReplayGapReason::Unavailable));
    }
    let requested = cursor.live_position;
    let Some(tail) = first.get::<Option<i64>, _>("tail_position").map(position) else {
        // A process without a head continues only from the watermark a
        // head would start at.
        return Ok(if requested == incarnation.watermark {
            Read::Events(Vec::new())
        } else {
            Read::Gap(ProcessReplayGapReason::Unavailable)
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
    if requested < floor || requested > tail {
        return Ok(Read::Gap(ProcessReplayGapReason::Unavailable));
    }
    if requested + 1 < first_live {
        return Ok(Read::Gap(ProcessReplayGapReason::Trimmed));
    }
    let mut events = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(event_position) = row.get::<Option<i64>, _>("position").map(position) else {
            continue;
        };
        let sequence = ProcessSequence::new(position(row.get("sequence")));
        events.push(super::codec::decode(
            incarnation.cursor(&cursor.process_id, sequence, event_position),
            &row.get::<Vec<u8>, _>("payload"),
        )?);
    }
    Ok(Read::Events(events))
}

/// A subscription's live tail: it waits for its process's doorbell and
/// reads the rows past the last position it delivered. A missed doorbell
/// only delays it; the table is the log.
struct LiveTail {
    shared: Arc<Shared>,
    process_id: ProcessId,
    incarnation: String,
    last: u64,
    bell: BellGuard,
    queue: VecDeque<Arc<ProcessObservationEvent>>,
    done: bool,
}

type LiveItem = Result<Arc<ProcessObservationEvent>, ProcessReplayStoreError>;

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
                return self.end(ProcessReplayStoreError::Closed);
            }
            let cursor = Incarnation {
                id: self.incarnation.clone(),
                watermark: 0,
            }
            .cursor(&self.process_id, ProcessSequence::new(0), self.last);
            let parsed = match cursor.parse() {
                Ok(parsed) => parsed,
                Err(error) => return self.end(error.into()),
            };
            match read(&self.shared, &parsed).await {
                Ok(Read::Events(events)) => {
                    if let Some(last) = events.last() {
                        self.last = last.live_position();
                        // A full window may have more behind it.
                        if events.len() >= self.shared.config.max_events_per_process {
                            self.bell.receiver.mark_changed();
                        }
                    }
                    self.queue.extend(events);
                }
                // The window moved past this subscriber: it lagged.
                Ok(Read::Gap(ProcessReplayGapReason::Trimmed)) => {
                    return self.end(ProcessReplayStoreError::SubscriberLagged(1));
                }
                // Invalidated, evicted, forgotten or rotated: the
                // generation it followed ended.
                Ok(Read::Gap(ProcessReplayGapReason::Unavailable)) => {
                    return self.end(ProcessReplayStoreError::Closed);
                }
                Err(error) => {
                    tracing::warn!(
                        process_id = %self.process_id,
                        %error,
                        "a process replay tail could not read its process; the observer resubscribes"
                    );
                    return self.end(ProcessReplayStoreError::Closed);
                }
            }
        }
    }

    fn end(&mut self, error: ProcessReplayStoreError) -> Option<LiveItem> {
        self.done = true;
        Some(Err(error))
    }
}

/// The live tail after `last` for a subscriber whose doorbell `bell` was
/// registered before its replay was read.
pub(super) fn live_tail(
    shared: Arc<Shared>,
    process_id: ProcessId,
    incarnation: String,
    last: u64,
    bell: BellGuard,
) -> impl Stream<Item = LiveItem> + Send + 'static {
    let tail = LiveTail {
        shared,
        process_id,
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
