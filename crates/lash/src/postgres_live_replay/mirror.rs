//! A replica's local knowledge of every session's head, so
//! [`current_cursor`](lash_core::LiveReplayStore::current_cursor), which
//! lash calls from synchronous code, answers without a round trip.
//!
//! The mirror is loaded when the listener connects and merged from the
//! store's own publications and every doorbell after that. What it does not
//! know makes it answer earlier, never later: a position whose revision it
//! has not learned counts as newer than any snapshot, so a cursor it hands
//! out never sits past an event a stale snapshot lacks (P12). Earlier only
//! replays more.

use std::collections::{BTreeMap, HashMap};

use lash_core::{SessionCursor, SessionRevision};
use lash_sansio::SessionId;

use super::codec::Doorbell;
use super::schema::Incarnation;

#[derive(Debug)]
pub(super) struct Mirror {
    pub(super) incarnation: Incarnation,
    sessions: HashMap<SessionId, MirrorSession>,
}

/// What a replica knows of one session's head.
#[derive(Debug, Default)]
struct MirrorSession {
    /// The generation boundary: positions at or below it belong to an
    /// earlier generation.
    floor: u64,
    first_retained: u64,
    tail: u64,
    /// Known revisions by run of positions: first position to last
    /// position and revision.
    runs: BTreeMap<u64, (u64, u64)>,
}

impl MirrorSession {
    fn new(floor: u64) -> Self {
        Self {
            floor,
            first_retained: floor + 1,
            tail: floor,
            runs: BTreeMap::new(),
        }
    }

    fn published(&mut self, first: u64, last: u64, revision: u64) {
        self.tail = self.tail.max(last);
        // A batch that continues the run before it at the same revision
        // extends that run, so a streaming session keeps one.
        if let Some((_, (previous_last, previous_revision))) =
            self.runs.range_mut(..first).next_back()
            && *previous_revision == revision
            && previous_last.saturating_add(1) >= first
        {
            *previous_last = (*previous_last).max(last);
            return;
        }
        self.runs.insert(first, (last, revision));
    }

    fn trimmed(&mut self, first_retained: u64) {
        self.first_retained = self.first_retained.max(first_retained);
        let retained = self.first_retained;
        self.runs.retain(|_, (last, _)| *last >= retained);
    }

    /// The last position before every retained event this session may hold
    /// at a revision past `revision`.
    fn position_before(&self, revision: u64) -> u64 {
        let retained = self.first_retained.max(self.floor + 1);
        let mut next = retained;
        for (&first, &(last, run_revision)) in &self.runs {
            if last < next {
                continue;
            }
            if first > next {
                // An unknown position may be newer.
                return next - 1;
            }
            if run_revision > revision {
                return first.max(retained) - 1;
            }
            next = last + 1;
        }
        // Past the known runs lies only what this replica has not learned:
        // the tail it knows, at most.
        (next - 1).min(self.tail.max(retained - 1))
    }
}

impl Mirror {
    pub(super) fn new(incarnation: Incarnation) -> Self {
        Self {
            incarnation,
            sessions: HashMap::new(),
        }
    }

    pub(super) fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
    ) -> SessionCursor {
        let position = self
            .sessions
            .get(session_id)
            .map_or(self.incarnation.watermark, |session| {
                session.position_before(revision.as_u64())
            });
        self.incarnation.cursor(session_id, revision, position)
    }

    /// The cursor before the first retained position this replica knows of
    /// the session. A trim it has not learned yet makes its replay answer
    /// `Trimmed`.
    pub(super) fn earliest_cursor(&self, session_id: &SessionId) -> SessionCursor {
        let position = self
            .sessions
            .get(session_id)
            .map_or(self.incarnation.watermark, |session| {
                session.first_retained.max(session.floor + 1) - 1
            });
        self.incarnation
            .cursor(session_id, SessionRevision::new(0), position)
    }

    /// Adopt `incarnation`, forgetting every session when it is new.
    pub(super) fn adopt(&mut self, incarnation: Incarnation) {
        if incarnation.id != self.incarnation.id {
            self.sessions.clear();
        }
        self.incarnation = incarnation;
    }

    /// Replace everything known with a load from the tables.
    pub(super) fn reload(
        &mut self,
        incarnation: Incarnation,
        heads: Vec<(SessionId, u64, u64, u64)>,
        runs: Vec<(SessionId, u64, u64, u64)>,
    ) {
        self.incarnation = incarnation;
        self.sessions = heads
            .into_iter()
            .map(|(session_id, tail, floor, first_retained)| {
                let mut session = MirrorSession::new(floor);
                session.tail = tail;
                session.first_retained = first_retained;
                (session_id, session)
            })
            .collect();
        for (session_id, first, last, revision) in runs {
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.published(first, last, revision);
            }
        }
    }

    /// Merge one doorbell, this replica's own or another's. Merges are
    /// idempotent and order-tolerant: a generation is named by its floor,
    /// and a doorbell from an older one is stale.
    pub(super) fn ring(&mut self, doorbell: &Doorbell) {
        match doorbell {
            Doorbell::Published {
                session,
                floor,
                first,
                last,
                revision,
            } => {
                if let Some(entry) = self.generation(session, *floor) {
                    entry.published(*first, *last, *revision);
                }
            }
            Doorbell::Invalidated { session, floor } => {
                self.generation(session, *floor);
            }
            Doorbell::Trimmed {
                session,
                first_retained,
            } => {
                if let Ok(session_id) = SessionId::parse(session.as_str())
                    && let Some(entry) = self.sessions.get_mut(&session_id)
                {
                    entry.trimmed(*first_retained);
                }
            }
            Doorbell::Forgotten {
                sessions,
                watermark,
            } => {
                for session in sessions {
                    if let Ok(session_id) = SessionId::parse(session.as_str())
                        && self
                            .sessions
                            .get(&session_id)
                            .is_some_and(|entry| entry.tail < *watermark)
                    {
                        self.sessions.remove(&session_id);
                    }
                }
                self.incarnation.watermark = self.incarnation.watermark.max(*watermark);
            }
            Doorbell::Rotated { incarnation } => {
                if *incarnation != self.incarnation.id {
                    self.sessions.clear();
                    self.incarnation = Incarnation {
                        id: incarnation.clone(),
                        watermark: 0,
                    };
                }
            }
        }
    }

    /// Learn what a read saw of `session_id`'s head: its generation and the
    /// first position its window still holds.
    pub(super) fn observed(&mut self, session_id: &SessionId, floor: u64, first_live: u64) {
        let entry = self
            .sessions
            .entry(session_id.clone())
            .or_insert_with(|| MirrorSession::new(floor));
        if floor > entry.floor {
            *entry = MirrorSession::new(floor);
        }
        if floor == entry.floor {
            entry.trimmed(first_live);
        }
    }

    /// The entry of `session`'s generation above `floor`, started afresh when
    /// that generation is newer than the one known; `None` when older.
    fn generation(&mut self, session: &str, floor: u64) -> Option<&mut MirrorSession> {
        let session_id = SessionId::parse(session).ok()?;
        let entry = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| MirrorSession::new(floor));
        if floor > entry.floor {
            *entry = MirrorSession::new(floor);
        }
        (floor == entry.floor).then_some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror() -> Mirror {
        Mirror::new(Incarnation {
            id: "inc".into(),
            watermark: 7,
        })
    }

    fn published(session: &str, floor: u64, first: u64, last: u64, revision: u64) -> Doorbell {
        Doorbell::Published {
            session: session.into(),
            floor,
            first,
            last,
            revision,
        }
    }

    fn position(mirror: &Mirror, session: &'static str, revision: u64) -> u64 {
        let cursor =
            mirror.current_cursor(&SessionId::from(session), SessionRevision::new(revision));
        cursor
            .parse_for_session(&SessionId::from(session))
            .expect("a mirror cursor parses")
            .live_position
    }

    /// P12 against partial knowledge: a cursor sits before the first
    /// retained event at a newer revision, and before any position whose
    /// revision the replica has not learned.
    #[test]
    fn a_cursor_never_passes_a_newer_or_unknown_position() {
        let mut mirror = mirror();
        assert_eq!(
            position(&mirror, "s", 1),
            7,
            "an unknown session starts at the watermark"
        );
        mirror.ring(&published("s", 0, 1, 3, 1));
        mirror.ring(&published("s", 0, 4, 5, 2));
        assert_eq!(position(&mirror, "s", 1), 3);
        assert_eq!(position(&mirror, "s", 2), 5);
        mirror.ring(&published("s", 0, 9, 9, 2));
        assert_eq!(position(&mirror, "s", 2), 5, "positions 6..8 are unknown");
        mirror.ring(&published("s", 0, 6, 8, 1));
        assert_eq!(position(&mirror, "s", 2), 9);
        mirror.ring(&Doorbell::Trimmed {
            session: "s".into(),
            first_retained: 5,
        });
        assert_eq!(
            position(&mirror, "s", 1),
            4,
            "a trimmed newer run clamps to the window"
        );
        mirror.ring(&Doorbell::Invalidated {
            session: "s".into(),
            floor: 10,
        });
        mirror.ring(&published("s", 0, 10, 10, 1));
        assert_eq!(
            position(&mirror, "s", 2),
            10,
            "an older generation's doorbell is stale"
        );
    }
}
