//! Parked work: the one host surface that lists parked turns and parked
//! processes together, summarizes them, and follows their transitions
//! (FIG-3659).
//!
//! A turn parks in its session store and a process parks on its registry
//! record; the two live in different stores (on SQLite, different database
//! files), so each is read through its own seam and merged here. Both are
//! ordered by `since_ms` — the first refusal of the park — so the merged list
//! is too: the page cursor keeps each source's own keyset position, which is
//! what makes the merge exact without comparing a session id with a process
//! id. The feed merge works the same way over each store's transition ledger.
//!
//! Parking is not failing: parked work holds what it holds until an operator
//! acts (FIG-3586). This surface is how an operator finds it.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use lash_core::store::{
    ParkEventKind, ParkFeedCursor, ParkId, ParkReason, ParkReasonCode, ParkSummary,
    ProcessParkQuery, TurnParkQuery,
};
use lash_core::{Clock, ProcessId, ProcessRegistry, SessionStoreFactory};
use lash_sansio::{SessionId, TurnId};
use serde::{Deserialize, Serialize};

use crate::Result;

/// The deployment's parked work, turns and processes alike.
///
/// Obtained from [`LashCore::parked_work`](crate::LashCore::parked_work).
#[derive(Clone)]
pub struct ParkedWork {
    pub(crate) store_factory: Arc<dyn SessionStoreFactory>,
    pub(crate) process_registry: Arc<dyn ProcessRegistry>,
    pub(crate) clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for ParkedWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ParkedWork").finish_non_exhaustive()
    }
}

/// The work a park holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParkedWorkRef {
    /// A session's parked turn.
    Turn {
        /// The session whose turn parked.
        session_id: SessionId,
        /// The turn that parked.
        turn_id: TurnId,
    },
    /// A parked process.
    Process {
        /// The process that parked.
        process_id: ProcessId,
    },
}

/// One live park.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedWorkRecord {
    /// The parked work.
    pub target: ParkedWorkRef,
    /// The park's identity: the operator verbs' CAS token.
    pub park_id: ParkId,
    /// Why it parked, as its latest refusal said.
    pub reason: ParkReason,
    /// Host-clock epoch milliseconds of the park's first refusal.
    pub since_ms: u64,
    /// Host-clock epoch milliseconds of its latest refusal.
    pub last_refused_ms: u64,
    /// Refusals since the park opened (1 on the first).
    pub attempts: u32,
}

/// Which kinds of parked work a [`ParkedWorkQuery`] reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParkedKinds {
    /// Parked turns only.
    Turns,
    /// Parked processes only.
    Processes,
    /// Both, merged.
    #[default]
    Both,
}

impl ParkedKinds {
    fn turns(self) -> bool {
        matches!(self, Self::Turns | Self::Both)
    }

    fn processes(self) -> bool {
        matches!(self, Self::Processes | Self::Both)
    }
}

/// The filter and page a [`ParkedWork::list`] read applies.
#[derive(Clone, Debug)]
pub struct ParkedWorkQuery {
    /// Which kinds of work to list.
    pub kinds: ParkedKinds,
    /// Restrict to these reason codes; `None` (or an empty set) means all.
    pub reasons: Option<BTreeSet<ParkReasonCode>>,
    /// Only parks at least this old (by their first refusal).
    pub min_age: Option<Duration>,
    /// Resume after a previous page.
    pub after: Option<ParkedWorkCursor>,
    /// Page size.
    pub limit: NonZeroUsize,
}

impl ParkedWorkQuery {
    /// Every parked turn and process, `limit` at a time.
    #[must_use]
    pub fn all(limit: NonZeroUsize) -> Self {
        Self {
            kinds: ParkedKinds::Both,
            reasons: None,
            min_age: None,
            after: None,
            limit,
        }
    }
}

/// Where a [`ParkedWork::list`] page resumes: each source's own keyset
/// position. Opaque; hosts persist it through serde.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedWorkCursor {
    turn: Option<(u64, SessionId)>,
    process: Option<(u64, ProcessId)>,
}

/// One page of parked work in `(since_ms, kind)` order, turns before
/// processes on a tie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedWorkPage {
    /// The parks on this page.
    pub records: Vec<ParkedWorkRecord>,
    /// Where the next page starts, `None` when this page is the last.
    pub next: Option<ParkedWorkCursor>,
}

/// Live parks per kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParkedWorkSummary {
    /// Parked turns.
    pub turns: ParkSummary,
    /// Parked processes.
    pub processes: ParkSummary,
}

impl ParkedWorkSummary {
    /// The oldest live park's first refusal over both kinds.
    #[must_use]
    pub fn oldest_since_ms(&self) -> Option<u64> {
        match (self.turns.oldest_since_ms, self.processes.oldest_since_ms) {
            (Some(turns), Some(processes)) => Some(turns.min(processes)),
            (turns, processes) => turns.or(processes),
        }
    }
}

/// One transition of a park, from either feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedWorkEvent {
    /// Host-clock epoch milliseconds the transition happened.
    pub at_ms: u64,
    /// The work whose park transitioned.
    pub target: ParkedWorkRef,
    /// The park the transition applies to.
    pub park_id: ParkId,
    /// The transition.
    pub kind: ParkEventKind,
}

/// Where a [`ParkedWork::events`] read resumes: each feed's own position.
/// Opaque; hosts persist it through serde.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedWorkEventsCursor {
    turn: ParkFeedCursor,
    process: ParkFeedCursor,
}

impl ParkedWorkEventsCursor {
    /// Both feeds from their start.
    #[must_use]
    pub fn initial() -> Self {
        Self::default()
    }
}

/// One page of park transitions from both feeds, each feed in its commit
/// order, merged by `at_ms`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedWorkEventPage {
    /// The transitions on this page.
    pub events: Vec<ParkedWorkEvent>,
    /// Where the next read resumes; every event on this page is behind it.
    pub next: ParkedWorkEventsCursor,
}

impl ParkedWork {
    /// A page of live parks, oldest first.
    ///
    /// # Errors
    /// When either store refuses the read.
    #[tracing::instrument(name = "lash.parked_work.list", skip_all)]
    pub async fn list(&self, query: &ParkedWorkQuery) -> Result<ParkedWorkPage> {
        let fetch = NonZeroUsize::new(query.limit.get().saturating_add(1)).unwrap_or(query.limit);
        let at_or_before = query.min_age.map(|age| {
            self.clock
                .timestamp_ms()
                .saturating_sub(u64::try_from(age.as_millis()).unwrap_or(u64::MAX))
        });
        let cursor = query.after.clone().unwrap_or_default();
        let turns = if query.kinds.turns() {
            self.store_factory
                .list_turn_parks(&TurnParkQuery {
                    reasons: query.reasons.clone(),
                    session: None,
                    parked_at_or_before_ms: at_or_before,
                    after: cursor.turn.clone(),
                    limit: fetch,
                })
                .await?
        } else {
            Vec::new()
        };
        let processes = if query.kinds.processes() {
            self.process_registry
                .list_parked_processes(&ProcessParkQuery {
                    reasons: query.reasons.clone(),
                    parked_at_or_before_ms: at_or_before,
                    after: cursor.process.clone(),
                    limit: fetch,
                })
                .await?
        } else {
            Vec::new()
        };
        let exhausted_turns = turns.len() < fetch.get();
        let exhausted_processes = processes.len() < fetch.get();
        let mut turns = turns.into_iter().peekable();
        let mut processes = processes
            .into_iter()
            .filter_map(|record| {
                let park = record.park.as_deref()?.clone();
                Some((record.park_key(), park))
            })
            .peekable();
        let mut next = cursor;
        let mut records = Vec::with_capacity(query.limit.get());
        while records.len() < query.limit.get() {
            let take_turn = match (turns.peek(), processes.peek()) {
                (Some(turn), Some((_, process))) => turn.since_ms <= process.since_ms,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_turn {
                let Some(turn) = turns.next() else { break };
                next.turn = Some((turn.since_ms, turn.session_id.clone()));
                records.push(ParkedWorkRecord {
                    target: ParkedWorkRef::Turn {
                        session_id: turn.session_id,
                        turn_id: turn.turn_id,
                    },
                    park_id: turn.park_id,
                    reason: turn.reason,
                    since_ms: turn.since_ms,
                    last_refused_ms: turn.last_refused_ms,
                    attempts: turn.attempts,
                });
            } else {
                let Some((process_id, park)) = processes.next() else {
                    break;
                };
                next.process = Some((park.since_ms, process_id.clone()));
                records.push(ParkedWorkRecord {
                    target: ParkedWorkRef::Process { process_id },
                    park_id: park.park_id,
                    reason: park.reason,
                    since_ms: park.since_ms,
                    last_refused_ms: park.last_refused_ms,
                    attempts: park.attempts,
                });
            }
        }
        let more = turns.peek().is_some()
            || processes.peek().is_some()
            || !exhausted_turns
            || !exhausted_processes;
        Ok(ParkedWorkPage {
            records,
            next: more.then_some(next),
        })
    }

    /// Live parks per kind and reason, and the oldest of each. Records the
    /// `lash.parked_work.count` and `lash.parked_work.oldest_age` gauges for
    /// every reason, zero included, so a host alerting on parked work calls
    /// this on its scrape interval.
    ///
    /// # Errors
    /// When either store refuses the read.
    #[tracing::instrument(name = "lash.parked_work.summary", skip_all)]
    pub async fn summary(&self) -> Result<ParkedWorkSummary> {
        let turns = self.store_factory.count_unsettled_turns().await?;
        let processes = self.process_registry.summarize_parked_processes().await?;
        let summary = ParkedWorkSummary {
            turns: ParkSummary {
                by_reason: turns.parked_by_reason,
                oldest_since_ms: turns.oldest_parked_since_ms,
            },
            processes,
        };
        record_park_gauges(&summary, self.clock.timestamp_ms());
        Ok(summary)
    }

    /// Park transitions from both feeds strictly after `from`, at most
    /// `limit` from each.
    ///
    /// # Errors
    /// When either feed refuses the read, including a position below a
    /// feed's compaction horizon (`StoreError::ParkFeedCursorCompacted`,
    /// `PluginError::ProcessParkFeedCursorCompacted`): relist, then resume
    /// from the horizon.
    #[tracing::instrument(name = "lash.parked_work.events", skip_all)]
    pub async fn events(
        &self,
        from: &ParkedWorkEventsCursor,
        limit: NonZeroUsize,
    ) -> Result<ParkedWorkEventPage> {
        let turns = self.store_factory.turn_park_feed(from.turn, limit).await?;
        let processes = self
            .process_registry
            .process_park_feed(from.process, limit)
            .await?;
        let next = ParkedWorkEventsCursor {
            turn: turns.next,
            process: processes.next,
        };
        let mut events = turns
            .events
            .into_iter()
            .map(|event| {
                (
                    (event.at_ms, 0u8, event.seq),
                    ParkedWorkEvent {
                        at_ms: event.at_ms,
                        target: ParkedWorkRef::Turn {
                            session_id: event.target.session_id,
                            turn_id: event.target.turn_id,
                        },
                        park_id: event.park_id,
                        kind: event.kind,
                    },
                )
            })
            .chain(processes.events.into_iter().map(|event| {
                (
                    (event.at_ms, 1u8, event.seq),
                    ParkedWorkEvent {
                        at_ms: event.at_ms,
                        target: ParkedWorkRef::Process {
                            process_id: event.target,
                        },
                        park_id: event.park_id,
                        kind: event.kind,
                    },
                )
            }))
            .collect::<Vec<_>>();
        // A stable sort on `(at_ms, feed, seq)` keeps each feed in its own
        // commit order.
        events.sort_by_key(|(key, _)| *key);
        Ok(ParkedWorkEventPage {
            events: events.into_iter().map(|(_, event)| event).collect(),
            next,
        })
    }

    /// Compact both feeds through `through`: events at or behind it are
    /// removed and each feed's horizon advances to it. Host-gated, never
    /// automatic.
    ///
    /// # Errors
    /// When either store refuses the compaction.
    pub async fn compact_events(&self, through: &ParkedWorkEventsCursor) -> Result<()> {
        self.store_factory
            .compact_turn_park_feed(through.turn)
            .await?;
        self.process_registry
            .compact_process_park_feed(through.process)
            .await?;
        Ok(())
    }
}

/// Record the parked-work gauges for both kinds: every reason's count, zero
/// included, and the oldest park's age.
pub(crate) fn record_park_gauges(summary: &ParkedWorkSummary, now_ms: u64) {
    for (kind, parks) in [("turn", &summary.turns), ("process", &summary.processes)] {
        for reason in ParkReasonCode::ALL {
            let count = parks.by_reason.get(reason).copied().unwrap_or_default();
            lash_core::operational_metrics::record_parked_work_count(
                kind,
                reason.as_str(),
                u64::try_from(count).unwrap_or(u64::MAX),
            );
        }
        lash_core::operational_metrics::record_parked_work_oldest_age(
            kind,
            parks
                .oldest_since_ms
                .map(|since| now_ms.saturating_sub(since))
                .unwrap_or_default(),
        );
    }
}
