//! Parked work: the host surface that lists parked turns, summarizes them,
//! and follows their transitions (FIG-3659), ordered by `since_ms`, the first
//! refusal of the park.
//!
//! A parked process is its actor's state (ADR 0132 §11): it is recorded in
//! the substrate's park feed, not here, and an operator redrives it by
//! [`ParkedWork::redrive`] or `ProcessAdmin::redrive`.
//!
//! Parking is not failing: parked work holds what it holds until an operator
//! acts (FIG-3586). This surface is how an operator finds it.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use lash_core::store::{
    ParkEventKind, ParkFeedCursor, ParkId, ParkReason, ParkReasonCode, ParkReport, TurnParkQuery,
};
use lash_core::{Clock, DeploymentStore, ProcessId};
use lash_sansio::{SessionId, TurnId};
use serde::{Deserialize, Serialize};

use crate::Result;

/// The deployment's parked work.
///
/// Obtained from [`LashCore::parked_work`](crate::LashCore::parked_work).
#[derive(Clone)]
pub struct ParkedWork {
    pub(crate) store_factory: Arc<dyn DeploymentStore>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
    pub(crate) work: Arc<crate::core::CoreWorkSlot>,
    pub(crate) scopes: Arc<dyn lash_core::engine::ScopeCloseSink>,
    /// The `ScopeClose` kind's relay (ADR 0109 §3): a cancelled or forked
    /// run's scope close is its obligation's immediate delivery.
    pub(crate) scope_close_obligations: Arc<dyn lash_core::runtime::shift::relay::ObligationRelay>,
    /// The store set's `ControlIntent` obligation ledger: a verb's engine
    /// half is delivered through it (ADR 0109).
    pub(crate) intents: Arc<dyn lash_core::store::ObligationLedger>,
    /// The relay policy a verb's immediate `deliver_intent` runs under: the
    /// host's configured attempt budget (FIG-4246).
    pub(crate) relay_policy: lash_core::shift::relay::RelayPolicy,
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

/// The filter and page a [`ParkedWork::list`] read applies.
#[derive(Clone, Debug)]
pub struct ParkedWorkQuery {
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
    /// Every parked turn, `limit` at a time.
    #[must_use]
    pub fn all(limit: NonZeroUsize) -> Self {
        Self {
            reasons: None,
            min_age: None,
            after: None,
            limit,
        }
    }
}

/// Where a [`ParkedWork::list`] page resumes: the turn parks' keyset
/// position. Opaque; hosts persist it through serde.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedWorkCursor {
    turn: Option<(u64, SessionId)>,
}

/// One page of parked work in `since_ms` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedWorkPage {
    /// The parks on this page.
    pub records: Vec<ParkedWorkRecord>,
    /// Where the next page starts, `None` when this page is the last.
    pub next: Option<ParkedWorkCursor>,
}

/// Live parks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParkedWorkReport {
    /// Parked turns.
    pub turns: ParkReport,
}

impl ParkedWorkReport {
    /// The oldest live park's first refusal.
    #[must_use]
    pub fn oldest_since_ms(&self) -> Option<u64> {
        self.turns.oldest_since_ms
    }

    /// Live retired-generation parks, per the executable generation their
    /// admission recorded (FIG-3571).
    #[must_use]
    pub fn retired_by_executable_generation(
        &self,
    ) -> std::collections::BTreeMap<lash_core::ExecutableGeneration, usize> {
        self.turns.retired_by_executable_generation.clone()
    }
}

/// One transition of a park.
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

/// Where a [`ParkedWork::events`] read resumes: the turn park feed's
/// position. Opaque; hosts persist it through serde.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedWorkEventsCursor {
    turn: ParkFeedCursor,
}

impl ParkedWorkEventsCursor {
    /// The feed from its start.
    #[must_use]
    pub fn initial() -> Self {
        Self::default()
    }
}

/// One page of park transitions in the feed's commit order.
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
        let turns = self
            .store_factory
            .list_turn_parks(&TurnParkQuery {
                reasons: query.reasons.clone(),
                session: None,
                parked_at_or_before_ms: at_or_before,
                after: cursor.turn.clone(),
                limit: fetch,
            })
            .await?;
        let more = turns.len() > query.limit.get();
        let mut next = cursor;
        let records = turns
            .into_iter()
            .take(query.limit.get())
            .map(|turn| {
                next.turn = Some((turn.since_ms, turn.session_id.clone()));
                ParkedWorkRecord {
                    target: ParkedWorkRef::Turn {
                        session_id: turn.session_id,
                        turn_id: turn.turn_id,
                    },
                    park_id: turn.park_id,
                    reason: turn.reason,
                    since_ms: turn.since_ms,
                    last_refused_ms: turn.last_refused_ms,
                    attempts: turn.attempts,
                }
            })
            .collect();
        Ok(ParkedWorkPage {
            records,
            next: more.then_some(next),
        })
    }

    /// Live parks per reason, and the oldest. Records the
    /// `lash.parked_work.count` and `lash.parked_work.oldest_age` gauges for
    /// every reason, zero included, so a host alerting on parked work calls
    /// this on its scrape interval.
    ///
    /// # Errors
    /// When either store refuses the read.
    #[tracing::instrument(name = "lash.parked_work.summary", skip_all)]
    pub async fn summary(&self) -> Result<ParkedWorkReport> {
        let turns = self.store_factory.count_unsettled_turns().await?;
        let report = ParkedWorkReport {
            turns: ParkReport {
                by_reason: turns.parked_by_reason,
                oldest_since_ms: turns.oldest_parked_since_ms,
                retired_by_executable_generation: turns.retired_by_executable_generation,
            },
        };
        record_park_gauges(&self.metrics, &report, self.clock.timestamp_ms());
        Ok(report)
    }

    /// Park transitions strictly after `from`, at most `limit`.
    ///
    /// # Errors
    /// When the feed refuses the read, including a position below its
    /// compaction horizon (`StoreError::ParkFeedCursorCompacted`): relist,
    /// then resume from the horizon.
    #[tracing::instrument(name = "lash.parked_work.events", skip_all)]
    pub async fn events(
        &self,
        from: &ParkedWorkEventsCursor,
        limit: NonZeroUsize,
    ) -> Result<ParkedWorkEventPage> {
        let turns = self.store_factory.turn_park_feed(from.turn, limit).await?;
        Ok(ParkedWorkEventPage {
            events: turns
                .events
                .into_iter()
                .map(|event| ParkedWorkEvent {
                    at_ms: event.at_ms,
                    target: ParkedWorkRef::Turn {
                        session_id: event.target.session_id,
                        turn_id: event.target.turn_id,
                    },
                    park_id: event.park_id,
                    kind: event.kind,
                })
                .collect(),
            next: ParkedWorkEventsCursor { turn: turns.next },
        })
    }

    /// Compact the feed through `through`: events at or behind it are
    /// removed and its horizon advances to it. Host-gated, never automatic.
    ///
    /// # Errors
    /// When the store refuses the compaction.
    pub async fn compact_events(&self, through: &ParkedWorkEventsCursor) -> Result<()> {
        self.store_factory
            .compact_turn_park_feed(through.turn)
            .await?;
        Ok(())
    }
}

/// Record the parked-work gauges: every reason's count, zero included, and
/// the oldest park's age.
pub(crate) fn record_park_gauges(
    metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
    report: &ParkedWorkReport,
    now_ms: u64,
) {
    for (kind, parks) in [("turn", &report.turns)] {
        for reason in ParkReasonCode::ALL {
            let count = parks.by_reason.get(reason).copied().unwrap_or_default();
            lash_core::operational_metrics::record_parked_work_count(
                metrics,
                kind,
                reason.as_str(),
                u64::try_from(count).unwrap_or(u64::MAX),
            );
        }
        lash_core::operational_metrics::record_parked_work_oldest_age(
            metrics,
            kind,
            parks
                .oldest_since_ms
                .map(|since| now_ms.saturating_sub(since))
                .unwrap_or_default(),
        );
    }
}
