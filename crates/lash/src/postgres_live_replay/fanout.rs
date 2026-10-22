//! The one node-local owner of what this replica's subscribers share of a
//! session's replay: its decoded events and its in-flight live reads
//! (FIG-7053).
//!
//! PostgreSQL stays the authority. Every read still runs the one-snapshot
//! statement, and every subscriber still judges its own cursor, gap, expiry
//! and lag against what that statement returned. Only representation work
//! is shared:
//!
//! - **Decoded events.** A followed session holds the events its reads
//!   decoded, keyed by incarnation and position and bound to the row's
//!   revision, turn id and payload bytes. A row that matches all of them
//!   reuses the held event; any difference decodes the row again, so a
//!   changed row is never masked. A miss decodes under the session's lock,
//!   so readers racing on one row decode it once.
//! - **Live reads.** Live tails of one session at the same incarnation and
//!   position share one read, in flight or finished, until the next doorbell
//!   for the session: a tail woken by a later doorbell needs a snapshot that
//!   includes what that doorbell announced, so every doorbell clears the
//!   shared reads. A replay or a subscription's first read never shares one.
//! - **Bounds and invalidation.** A session's share lives while one of this
//!   replica's subscribers follows it. It holds at most the session's
//!   retained event count and twice its retained bytes (each payload and
//!   its decoded copy), and every share together at most [`NODE_BUDGET`];
//!   it keeps at most [`MAX_FLIGHTS`] shared reads.
//!   An invalidation, trim, forgetting, rotation or listener reconnect drops
//!   the held events and advances a generation, so a read that started
//!   before it never repopulates the share.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures_util::FutureExt as _;
use futures_util::future::{BoxFuture, Shared as SharedFuture};
use lash_core::{LiveReplayStoreError, SessionObservationEvent};
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::SessionId;

use super::subscription::Read;

/// What every share of one replica holds together, in bytes.
const NODE_BUDGET: u64 = 64 * 1024 * 1024;

/// The shared live reads one session keeps until its next doorbell; a tail
/// past the lowest of them reads for itself.
const MAX_FLIGHTS: usize = 16;

/// What a held event is charged beyond its payload and decoded copy.
const HELD_CHARGE: u64 = 192;

/// What one live read hands every tail that shares it.
pub(super) type LiveRead = Arc<Result<Read, LiveReplayStoreError>>;

type Sessions = Arc<StdMutex<HashMap<SessionId, Followed>>>;

/// The shares of the sessions this replica's subscribers follow.
pub(super) struct Fanout {
    sessions: Sessions,
    held: Arc<AtomicU64>,
    budget: u64,
}

struct Followed {
    share: Arc<Share>,
    followers: usize,
}

/// One followed session's held events and live reads.
pub(super) struct Share {
    state: StdMutex<State>,
    held: Arc<AtomicU64>,
    budget: u64,
    max_events: usize,
    max_bytes: u64,
}

#[derive(Default)]
struct State {
    /// Advances whenever the held events are dropped.
    generation: u64,
    /// The incarnation every held event belongs to.
    incarnation: String,
    events: BTreeMap<u64, Held>,
    charge: u64,
    /// The live reads started since the last doorbell, by incarnation and
    /// position; every doorbell clears them.
    flights: HashMap<(String, u64), SharedFuture<BoxFuture<'static, LiveRead>>>,
}

struct Held {
    revision: u64,
    turn_id: Option<String>,
    payload: Vec<u8>,
    event: Arc<SessionObservationEvent>,
    charge: u64,
}

/// A stored row as one read returned it.
pub(super) struct Row<'a> {
    pub(super) incarnation: &'a str,
    pub(super) position: u64,
    pub(super) revision: u64,
    pub(super) turn_id: Option<String>,
    pub(super) payload: Vec<u8>,
}

/// Keeps a session's share while one subscriber follows it.
pub(super) struct Follow {
    sessions: Sessions,
    session_id: SessionId,
    share: Arc<Share>,
}

/// What one read decodes through: the session's share as it stood before
/// the read's statement ran, or nothing for a session no subscriber here
/// follows.
pub(super) struct Reader(Option<(Arc<Share>, u64)>);

impl Default for Fanout {
    fn default() -> Self {
        Self::with_budget(NODE_BUDGET)
    }
}

impl Fanout {
    fn with_budget(budget: u64) -> Self {
        Self {
            sessions: Sessions::default(),
            held: Arc::default(),
            budget,
        }
    }

    /// Follow `session_id` until the guard drops. A subscription takes it
    /// before its first read; `max_events` and `max_bytes` are the
    /// session's retention.
    pub(super) fn follow(
        &self,
        session_id: &SessionId,
        max_events: usize,
        max_bytes: usize,
    ) -> Follow {
        let mut sessions = self.sessions.lock_recover();
        let followed = sessions
            .entry(session_id.clone())
            .or_insert_with(|| Followed {
                share: Arc::new(Share {
                    state: StdMutex::default(),
                    held: Arc::clone(&self.held),
                    budget: self.budget,
                    max_events,
                    max_bytes: 2 * max_bytes as u64,
                }),
                followers: 0,
            });
        followed.followers += 1;
        Follow {
            sessions: Arc::clone(&self.sessions),
            session_id: session_id.clone(),
            share: Arc::clone(&followed.share),
        }
    }

    /// Taken before a read's statement runs.
    pub(super) fn reader(&self, session_id: &SessionId) -> Reader {
        let share = self
            .sessions
            .lock_recover()
            .get(session_id)
            .map(|followed| Arc::clone(&followed.share));
        Reader(share.map(|share| {
            let generation = share.state.lock_recover().generation;
            (share, generation)
        }))
    }

    /// A doorbell is about to ring `session_id`'s subscribers. One that
    /// ends the session's continuity (`ends`) also drops its held events.
    pub(super) fn rang(&self, session_id: &SessionId, ends: bool) {
        let share = self
            .sessions
            .lock_recover()
            .get(session_id)
            .map(|followed| Arc::clone(&followed.share));
        if let Some(share) = share {
            share.rang(ends);
        }
    }

    /// Every subscriber is about to be rung: the listener reconnected, or
    /// the history was replaced.
    pub(super) fn rang_all(&self) {
        let shares = self
            .sessions
            .lock_recover()
            .values()
            .map(|followed| Arc::clone(&followed.share))
            .collect::<Vec<_>>();
        for share in shares {
            share.rang(true);
        }
    }
}

impl Follow {
    pub(super) fn share(&self) -> &Arc<Share> {
        &self.share
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        let mut sessions = self.sessions.lock_recover();
        let Some(followed) = sessions.get_mut(&self.session_id) else {
            return;
        };
        followed.followers -= 1;
        if followed.followers == 0 {
            sessions.remove(&self.session_id);
            drop(sessions);
            self.share.rang(true);
        }
    }
}

impl Reader {
    /// The event `row` holds: the share's when it holds this very row,
    /// else what `decode` makes of it.
    pub(super) fn event<E>(
        &self,
        row: Row<'_>,
        decode: impl FnOnce(&Row<'_>) -> Result<Arc<SessionObservationEvent>, E>,
    ) -> Result<Arc<SessionObservationEvent>, E> {
        match &self.0 {
            Some((share, generation)) => share.event(*generation, row, decode),
            None => decode(&row),
        }
    }
}

impl Share {
    fn rang(&self, ends: bool) {
        let mut state = self.state.lock_recover();
        // No tail woken by this doorbell may share a read from before it.
        let flights = std::mem::take(&mut state.flights);
        if ends {
            state.generation += 1;
            self.release(&mut state);
        }
        drop(state);
        drop(flights);
    }

    fn release(&self, state: &mut State) {
        state.events.clear();
        self.held.fetch_sub(state.charge, Ordering::AcqRel);
        state.charge = 0;
    }

    fn event<E>(
        &self,
        generation: u64,
        row: Row<'_>,
        decode: impl FnOnce(&Row<'_>) -> Result<Arc<SessionObservationEvent>, E>,
    ) -> Result<Arc<SessionObservationEvent>, E> {
        let mut state = self.state.lock_recover();
        if state.generation != generation {
            drop(state);
            return decode(&row);
        }
        if state.incarnation != row.incarnation {
            self.release(&mut state);
            row.incarnation.clone_into(&mut state.incarnation);
        }
        if let Some(held) = state.events.get(&row.position)
            && held.revision == row.revision
            && held.turn_id == row.turn_id
            && held.payload == row.payload
        {
            return Ok(Arc::clone(&held.event));
        }
        let event = decode(&row)?;
        if let Some(changed) = state.events.remove(&row.position) {
            state.charge -= changed.charge;
            self.held.fetch_sub(changed.charge, Ordering::AcqRel);
        }
        let charge = 2 * row.payload.len() as u64 + HELD_CHARGE;
        if self.admit(&mut state, charge) {
            state.events.insert(
                row.position,
                Held {
                    revision: row.revision,
                    turn_id: row.turn_id,
                    payload: row.payload,
                    event: Arc::clone(&event),
                    charge,
                },
            );
        }
        Ok(event)
    }

    /// Make room for `charge` within the session's and the node's bounds,
    /// evicting this session's oldest events; false when it cannot fit.
    fn admit(&self, state: &mut State, charge: u64) -> bool {
        while state.events.len() >= self.max_events || state.charge + charge > self.max_bytes {
            if !self.evict_oldest(state) {
                return false;
            }
        }
        loop {
            let reserved = self
                .held
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    (held + charge <= self.budget).then_some(held + charge)
                });
            if reserved.is_ok() {
                state.charge += charge;
                return true;
            }
            if !self.evict_oldest(state) {
                return false;
            }
        }
    }

    fn evict_oldest(&self, state: &mut State) -> bool {
        let Some((_, oldest)) = state.events.pop_first() else {
            return false;
        };
        state.charge -= oldest.charge;
        self.held.fetch_sub(oldest.charge, Ordering::AcqRel);
        true
    }

    /// The live read of `incarnation` after `position`: the one this share
    /// started after the session's last doorbell, in flight or finished,
    /// else a new one `start` makes. Which one is decided by this call, not
    /// when the future is first polled.
    pub(super) fn live_read(
        &self,
        incarnation: &str,
        position: u64,
        start: impl FnOnce() -> BoxFuture<'static, LiveRead>,
    ) -> SharedFuture<BoxFuture<'static, LiveRead>> {
        let key = (incarnation.to_owned(), position);
        let mut state = self.state.lock_recover();
        if let Some(read) = state.flights.get(&key) {
            return read.clone();
        }
        let lowest = (state.flights.len() >= MAX_FLIGHTS)
            .then(|| state.flights.keys().min_by_key(|key| key.1).cloned())
            .flatten();
        let evicted = lowest.and_then(|lowest| state.flights.remove(&lowest));
        let read = start().shared();
        state.flights.insert(key, read.clone());
        drop(state);
        // A read still in flight drops its statement outside the lock.
        drop(evicted);
        read
    }
}

#[cfg(test)]
mod tests;
