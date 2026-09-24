//! The in-memory turn park feed (FIG-3659): the factory-global transition
//! ledger every park write and every park clear appends to while holding the
//! park's lock, mirroring what `turn_park_events` + `turn_park_clock` give the
//! durable backends. A session's events outlive its store the way the durable
//! ledger rows outlive their session's catalog rows.

use super::*;

/// The feed state every store of one factory shares: the sequence clock and
/// the compaction horizon ride the same structure so a cursor position and the
/// horizon it is checked against can never disagree.
#[derive(Default)]
pub struct TurnParkFeed {
    events: Vec<crate::store::TurnParkFeedEvent>,
    next_seq: u64,
    compaction_horizon: u64,
}

/// `Arc<Mutex<TurnParkFeed>>`, handed to each store the way `session_catalog`
/// is.
pub(crate) type SharedTurnParkFeed = Arc<Mutex<TurnParkFeed>>;

impl TurnParkFeed {
    /// Allocate the next feed sequence. The caller inserts the matching event
    /// while still holding the lock it mutated the park under, so allocation
    /// and append are one critical section — the SQL backends' in-transaction
    /// clock bump.
    fn allocate_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn push(
        &mut self,
        seq: u64,
        session_id: SessionId,
        turn_id: TurnId,
        park_id: crate::store::ParkId,
        kind: crate::store::TurnParkEventKind,
        at_ms: u64,
    ) -> crate::store::TurnParkFeedEvent {
        let event = crate::store::TurnParkFeedEvent {
            seq,
            at_ms,
            session_id,
            turn_id,
            park_id,
            kind,
        };
        self.events.push(event.clone());
        event
    }

    /// Append one closing transition. Callers hold the `turn_park` lock across
    /// the park mutation and this append so the two never separate.
    pub fn log(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        park_id: crate::store::ParkId,
        kind: crate::store::TurnParkEventKind,
        at_ms: u64,
    ) -> crate::store::TurnParkFeedEvent {
        let seq = self.allocate_seq();
        self.push(seq, session_id, turn_id, park_id, kind, at_ms)
    }

    /// Append the `Parked` event that opens a park and return the `ParkId` it
    /// mints: the park's identity is the event's own sequence.
    pub fn log_opening(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        reason: crate::store::ParkReason,
        at_ms: u64,
    ) -> crate::store::ParkId {
        let seq = self.allocate_seq();
        let park_id = crate::store::ParkId::from_feed_sequence(seq);
        self.push(
            seq,
            session_id,
            turn_id,
            park_id,
            crate::store::TurnParkEventKind::Parked { reason },
            at_ms,
        );
        park_id
    }

    /// Events strictly after `after`, in commit order, and the cursor that
    /// resumes past them.
    ///
    /// # Errors
    /// `StoreError::ParkFeedCursorCompacted` when `after` sits below the
    /// compaction horizon: the events it would resume from are gone.
    pub fn events_after(
        &self,
        after: crate::store::TurnParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::TurnParkFeedPage, crate::StoreError> {
        let position = after.store_sequence();
        if position < self.compaction_horizon {
            return Err(crate::StoreError::ParkFeedCursorCompacted {
                horizon: crate::store::TurnParkFeedCursor::from_store_sequence(
                    self.compaction_horizon,
                ),
            });
        }
        let events = self
            .events
            .iter()
            .filter(|event| event.seq > position)
            .take(limit.get())
            .cloned()
            .collect::<Vec<_>>();
        let next = events
            .last()
            .map(|event| crate::store::TurnParkFeedCursor::from_store_sequence(event.seq))
            .unwrap_or(after);
        Ok(crate::store::TurnParkFeedPage { events, next })
    }

    /// Drop events at or below `through` and raise the durable horizon to it.
    /// `through` is clamped to the allocated sequence first: raising the
    /// horizon past `next_seq` would strand every event the feed has not yet
    /// appended. Compaction is host-gated; nothing calls this implicitly.
    pub fn compact_through(&mut self, through: crate::store::TurnParkFeedCursor) {
        let position = through.store_sequence().min(self.next_seq);
        self.events.retain(|event| event.seq > position);
        self.compaction_horizon = self.compaction_horizon.max(position);
    }
}
