//! Each obligation kind's due pass on a lane of its own (ADR 0109 §1.8).
//!
//! A recovery tick starts every kind's [`relay_due`] pass as a task on that
//! kind's lane and waits for them at most [`RecoveryPassBudget::tick_wait`]
//! before its leader-only arms run. A pass is bounded by its kind's attempt
//! budget (its page's rows are attempted together, each under the budget),
//! so a slow delivery delays nothing but its own kind: the tick's parks and
//! drain arms run at most `tick_wait` after it started, every other kind's
//! pass runs beside it, and the next tick still fires on its interval. A
//! kind whose last pass is still delivering is reported busy and skipped
//! until that pass ends; the tick that finds it ended reports it. A delivery
//! can arm another kind's row, so within the wait a kind whose pass found
//! nothing looks again once another kind's pass claimed rows: what one kind
//! delivers early in a tick, a later kind still picks up in that tick.
//!
//! The lanes belong to the deployment that runs the tick: dropping them
//! aborts every pass still running, as a deployment that dies drops its
//! deliveries.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt as _;

use super::relay::{ObligationRelay, relay_due, relay_kind};
use crate::engine::{RecoveryPassBudget, RelayPass};
use crate::store::ObligationKind;
use crate::{Clock, StoreError};

/// One kind's pass in flight.
struct Lane {
    kind: ObligationKind,
    pass: tokio::task::JoinHandle<Result<RelayPass, StoreError>>,
    /// The pass's end, once a tick saw it.
    ended: Option<Result<RelayPass, String>>,
}

impl Lane {
    fn end(&mut self, joined: Result<Result<RelayPass, StoreError>, tokio::task::JoinError>) {
        self.ended = Some(match joined {
            Ok(pass) => pass.map_err(|error| error.to_string()),
            Err(error) => Err(format!("the due pass did not finish: {error}")),
        });
    }
}

/// What the kinds' lanes did during one tick.
#[derive(Debug, Default)]
pub struct LanesTick {
    /// Each pass that ended by the time the tick stopped waiting, with its
    /// kind: its report, or why it failed.
    pub ended: Vec<(ObligationKind, Result<RelayPass, String>)>,
    /// Kinds whose earlier pass was still delivering, so the tick started
    /// none for them.
    pub busy: Vec<ObligationKind>,
}

/// Every obligation kind's due-pass lane of one deployment.
pub struct RelayLanes {
    clock: Arc<dyn Clock>,
    tick_wait: Duration,
    running: Mutex<Vec<Lane>>,
}

impl RelayLanes {
    /// Lanes whose passes run on `clock`, a tick waiting on them for at
    /// most `budget`'s [`tick_wait`](RecoveryPassBudget::tick_wait).
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, budget: RecoveryPassBudget) -> Self {
        Self {
            clock,
            tick_wait: budget.tick_wait,
            running: Mutex::new(Vec::new()),
        }
    }

    /// One tick's due passes: collect each pass that ended since the last
    /// tick, start a pass of at most `page` rows for every kind of `relays`
    /// whose lane is free, and wait for the running passes until they all
    /// end or the tick's wait runs out.
    ///
    /// A delivery can arm another kind's row (a close intent's
    /// acknowledgement arms its session's delete), so while the wait lasts a
    /// kind whose pass claimed nothing gets another pass once some other
    /// kind's pass claimed rows. A kind claims rows at most once a tick.
    pub async fn tick(&self, relays: &[Arc<dyn ObligationRelay>], page: NonZeroUsize) -> LanesTick {
        let deadline = self.clock.now() + self.tick_wait;
        let mut lanes = std::mem::take(
            &mut *self
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut tick = LanesTick::default();
        let mut eligible: Vec<&Arc<dyn ObligationRelay>> = relays.iter().collect();
        let mut first_round = true;
        loop {
            let mut started = Vec::new();
            for relay in eligible {
                let kind = relay_kind(relay.as_ref());
                if let Some(lane) = lanes.iter_mut().find(|lane| lane.kind == kind) {
                    // A pass that ended since the last tick frees its lane.
                    if let Some(joined) = (&mut lane.pass).now_or_never() {
                        lane.end(joined);
                    } else {
                        if first_round {
                            tick.busy.push(kind);
                        }
                        continue;
                    }
                }
                let relay = Arc::clone(relay);
                let clock = Arc::clone(&self.clock);
                lanes.push(Lane {
                    kind,
                    pass: crate::task::spawn(async move {
                        relay_due(relay.as_ref(), clock.as_ref(), page).await
                    }),
                    ended: None,
                });
                started.push(kind);
            }
            first_round = false;
            {
                let running = futures_util::future::join_all(
                    lanes
                        .iter_mut()
                        .filter(|lane| lane.ended.is_none())
                        .map(|lane| async move {
                            let joined = (&mut lane.pass).await;
                            lane.end(joined);
                        }),
                );
                tokio::select! {
                    _ = running => {}
                    () = self.clock.sleep_until(deadline) => {}
                }
            }
            let mut still_running = Vec::new();
            let mut claimed = false;
            for mut lane in lanes {
                match lane.ended.take() {
                    Some(ended) => {
                        claimed |= matches!(&ended, Ok(pass) if pass.claimed > 0);
                        merge(&mut tick.ended, lane.kind, ended);
                    }
                    None => still_running.push(lane),
                }
            }
            lanes = still_running;
            // Another round only when this one claimed rows and ended in
            // time: the kinds whose pass found nothing look again.
            let settled = lanes.is_empty() && self.clock.now() < deadline;
            eligible = relays
                .iter()
                .filter(|relay| {
                    let kind = relay_kind(relay.as_ref());
                    started.contains(&kind)
                        && tick.ended.iter().any(|(ended, pass)| {
                            *ended == kind && matches!(pass, Ok(pass) if pass.claimed == 0)
                        })
                })
                .collect();
            if !(claimed && settled) || eligible.is_empty() {
                break;
            }
        }
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(lanes);
        // Relay order: the ended passes are reported in the order the tick
        // names the kinds.
        tick.ended.sort_by_key(|(kind, _)| {
            relays
                .iter()
                .position(|relay| relay_kind(relay.as_ref()) == *kind)
                .unwrap_or(usize::MAX)
        });
        tick
    }
}

/// Add `ended` to `kind`'s report in `ended_by_kind`: one entry per kind,
/// its passes' counts summed, a failure kept over a count.
fn merge(
    ended_by_kind: &mut Vec<(ObligationKind, Result<RelayPass, String>)>,
    kind: ObligationKind,
    ended: Result<RelayPass, String>,
) {
    let Some((_, total)) = ended_by_kind.iter_mut().find(|(seen, _)| *seen == kind) else {
        ended_by_kind.push((kind, ended));
        return;
    };
    match (total, ended) {
        (Ok(total), Ok(pass)) => {
            total.claimed += pass.claimed;
            total.delivered += pass.delivered;
            total.requested += pass.requested;
            total.retried += pass.retried;
            total.stalled += pass.stalled;
            total.claim_lost += pass.claim_lost;
        }
        (total @ Ok(_), Err(error)) => *total = Err(error),
        (Err(_), _) => {}
    }
}

impl Drop for RelayLanes {
    fn drop(&mut self) {
        for lane in self
            .running
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            lane.pass.abort();
        }
    }
}
