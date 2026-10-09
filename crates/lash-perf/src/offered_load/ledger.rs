//! The arrival generator and its operation ledger.
//!
//! Arrivals are independent: operation `n` is due at `n / rate` after the
//! window opens whether or not earlier operations have finished, and every
//! latency clock starts at that scheduled instant. A service that stalls
//! therefore shows its stall in every operation that was due while it was
//! stalled. A closed loop, which sends the next operation only when the last
//! one returns and times from the send, records the stall once and drops the
//! queue it caused.
//!
//! Admission is finite on both axes: the schedule holds a fixed number of
//! operations, and at most `max_in_flight` of them are with the service at
//! once. An arrival that finds no free slot waits for one with its clock
//! already running. The window closes `drain_timeout` after the last
//! scheduled arrival; operations still open then are counted `unfinished`,
//! never dropped.

use crate::perf_support::metrics::nearest_rank;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use serde::Serialize;
use tokio::time::Instant;

/// One finite schedule of independent arrivals.
#[derive(Clone, Copy, Debug)]
pub struct ArrivalSchedule {
    /// Configured arrival rate; operation `n` is due at `n / rate`.
    pub rate_per_second: f64,
    /// Configured number of scheduled operations.
    pub operations: usize,
    /// Configured upper bound on operations with the service at once.
    pub max_in_flight: usize,
    /// Configured wait after the last scheduled arrival before the window
    /// closes on whatever is still open.
    pub drain_timeout: Duration,
}

impl ArrivalSchedule {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.rate_per_second.is_finite() && self.rate_per_second > 0.0,
            "the arrival rate must be finite and positive"
        );
        anyhow::ensure!(self.operations > 0, "a schedule needs operations");
        anyhow::ensure!(self.max_in_flight > 0, "admission needs a slot");
        Ok(())
    }

    fn due(&self, ordinal: usize) -> Duration {
        Duration::from_secs_f64(ordinal as f64 / self.rate_per_second)
    }
}

/// What the generator hands the service at the send instant.
#[derive(Clone, Debug)]
pub struct Arrival {
    pub ordinal: usize,
    pub key: String,
}

/// Instants the service observed, as offsets from its own send.
#[derive(Clone, Copy, Debug, Default)]
pub struct ServiceMarks {
    pub admitted_after_send: Option<Duration>,
    pub settled_after_send: Option<Duration>,
    /// The caller-visible completion, when the service keeps working after
    /// it (reading late marks). Absent, completion is the service's return.
    pub completed_after_send: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationOutcome {
    Completed,
    Error,
    Unfinished,
}

/// One operation's row. Timestamps are microseconds on the generator
/// process's monotonic clock since the window opened.
#[derive(Clone, Debug, Serialize)]
pub struct OperationRecord {
    pub key: String,
    pub ordinal: usize,
    pub scheduled_us: u64,
    pub sent_us: Option<u64>,
    pub admitted_us: Option<u64>,
    pub settled_us: Option<u64>,
    pub completed_us: Option<u64>,
    pub outcome: OperationOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl OperationRecord {
    fn span(from: Option<u64>, to: Option<u64>) -> Option<u64> {
        Some(to?.saturating_sub(from?))
    }

    fn completed(&self) -> Option<u64> {
        (self.outcome == OperationOutcome::Completed)
            .then_some(self.completed_us)
            .flatten()
    }
}

/// Every operation of one schedule, in ordinal order.
#[derive(Clone, Debug)]
pub struct ArrivalLedger {
    pub schedule: ArrivalSchedule,
    pub operations: Vec<OperationRecord>,
    /// When the window closed: the last operation's end, or the drain
    /// deadline when operations were still open.
    pub closed_us: u64,
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Run `schedule` against `service` and return the ledger.
///
/// The service is called once per operation, at the send instant, on its own
/// task. A service error is recorded on the operation; it never ends the
/// window.
pub async fn run_scheduled<S, F>(
    schedule: ArrivalSchedule,
    key_prefix: &str,
    service: S,
) -> ArrivalLedger
where
    S: Fn(Arrival) -> F + Send + Sync + 'static,
    F: Future<Output = anyhow::Result<ServiceMarks>> + Send + 'static,
{
    let records = Arc::new(Mutex::new(
        (0..schedule.operations)
            .map(|ordinal| OperationRecord {
                key: format!("{key_prefix}-{ordinal:06}"),
                ordinal,
                scheduled_us: micros(schedule.due(ordinal)),
                sent_us: None,
                admitted_us: None,
                settled_us: None,
                completed_us: None,
                outcome: OperationOutcome::Unfinished,
                error: None,
            })
            .collect::<Vec<_>>(),
    ));
    let service = Arc::new(service);
    let slots = Arc::new(tokio::sync::Semaphore::new(schedule.max_in_flight));
    let mut tasks = tokio::task::JoinSet::new();
    let opened = Instant::now();

    for ordinal in 0..schedule.operations {
        tokio::time::sleep_until(opened + schedule.due(ordinal)).await;
        let key = records.lock_recover()[ordinal].key.clone();
        let records = Arc::clone(&records);
        let service = Arc::clone(&service);
        let slots = Arc::clone(&slots);
        tasks.spawn(async move {
            let Ok(_slot) = slots.acquire_owned().await else {
                return;
            };
            let sent = Instant::now();
            let sent_us = micros(sent - opened);
            records.lock_recover()[ordinal].sent_us = Some(sent_us);
            let result = service(Arrival { ordinal, key }).await;
            let returned_us = micros(Instant::now() - opened);
            let mut records = records.lock_recover();
            let record = &mut records[ordinal];
            match result {
                Ok(marks) => {
                    let at = |offset: Option<Duration>| offset.map(|o| sent_us + micros(o));
                    record.admitted_us = at(marks.admitted_after_send);
                    record.settled_us = at(marks.settled_after_send);
                    record.completed_us =
                        Some(at(marks.completed_after_send).unwrap_or(returned_us));
                    record.outcome = OperationOutcome::Completed;
                }
                Err(error) => {
                    record.completed_us = Some(returned_us);
                    record.outcome = OperationOutcome::Error;
                    record.error = Some(format!("{error:#}"));
                }
            }
        });
    }

    let deadline =
        opened + schedule.due(schedule.operations.saturating_sub(1)) + schedule.drain_timeout;
    let drained = tokio::time::timeout_at(deadline, async {
        while let Some(joined) = tasks.join_next().await {
            if let Err(error) = joined
                && error.is_panic()
            {
                std::panic::resume_unwind(error.into_panic());
            }
        }
    })
    .await
    .is_ok();
    if !drained {
        tasks.shutdown().await;
    }
    let operations = records.lock_recover().clone();
    let closed_us = if drained {
        operations
            .iter()
            .filter_map(|record| record.completed_us)
            .max()
            .unwrap_or_default()
    } else {
        micros(deadline - opened)
    };
    ArrivalLedger {
        schedule,
        operations,
        closed_us,
    }
}

// ---------------------------------------------------------------------------
// Summaries.
// ---------------------------------------------------------------------------

/// One reported number with what it measures.
#[derive(Clone, Debug, Serialize)]
pub struct Quantity {
    pub quantity: &'static str,
    pub unit: &'static str,
    /// The process or window the number covers.
    pub window: &'static str,
    /// How the number was produced; `configured` is an input, not a
    /// measurement.
    pub statistic: &'static str,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Bucket {
    /// Inclusive upper bound; each bucket's lower bound is the previous
    /// power of two.
    pub le_ms: f64,
    pub count: usize,
}

/// One interval's distribution over the operations that reached both marks.
///
/// Percentiles are nearest-rank over the exact samples, so a percentile is
/// always an observed operation and never interpolates below the tail. The
/// buckets are unclipped: the last one holds `max`.
#[derive(Clone, Debug, Serialize)]
pub struct IntervalReport {
    pub quantity: &'static str,
    pub unit: &'static str,
    pub window: &'static str,
    pub statistic: &'static str,
    pub n: usize,
    /// Scheduled operations with no sample here (errors, unfinished, or a
    /// mark that was never observed). They are missing from the tail.
    pub censored: usize,
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p99_9: f64,
    pub max: f64,
    pub mean: f64,
    pub buckets: Vec<Bucket>,
}

fn ms(us: u64) -> f64 {
    us as f64 / 1_000.0
}

impl IntervalReport {
    fn of(
        quantity: &'static str,
        window: &'static str,
        scheduled: usize,
        mut samples_us: Vec<u64>,
    ) -> Option<Self> {
        if samples_us.is_empty() {
            return None;
        }
        samples_us.sort_unstable();
        let mut buckets: BTreeMap<u64, usize> = BTreeMap::new();
        for sample in &samples_us {
            *buckets
                .entry(sample.max(&1).next_power_of_two())
                .or_default() += 1;
        }
        Some(Self {
            quantity,
            unit: "ms",
            window,
            statistic: "nearest-rank percentiles over exact samples; power-of-two buckets",
            n: samples_us.len(),
            censored: scheduled - samples_us.len(),
            min: ms(samples_us[0]),
            p50: ms(nearest_rank(&samples_us, 0.50)),
            p90: ms(nearest_rank(&samples_us, 0.90)),
            p99: ms(nearest_rank(&samples_us, 0.99)),
            p99_9: ms(nearest_rank(&samples_us, 0.999)),
            max: ms(samples_us[samples_us.len() - 1]),
            mean: ms(samples_us.iter().sum::<u64>()) / samples_us.len() as f64,
            buckets: buckets
                .into_iter()
                .map(|(le_us, count)| Bucket {
                    le_ms: ms(le_us),
                    count,
                })
                .collect(),
        })
    }
}

/// Operation counts for one window.
#[derive(Clone, Debug, Serialize)]
pub struct Counts {
    pub unit: &'static str,
    pub window: &'static str,
    pub statistic: &'static str,
    /// Configured: the schedule's size.
    pub scheduled: usize,
    pub sent: usize,
    pub admitted_observed: usize,
    pub settled_observed: usize,
    pub completed: usize,
    pub errors: usize,
    pub unfinished: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Intervals {
    pub scheduled_to_completed: Option<IntervalReport>,
    pub scheduled_to_sent: Option<IntervalReport>,
    pub sent_to_admitted: Option<IntervalReport>,
    pub admitted_to_settled: Option<IntervalReport>,
    pub settled_to_completed: Option<IntervalReport>,
    pub sent_to_completed: Option<IntervalReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OpenOperation {
    pub key: String,
    pub scheduled_us: u64,
    pub sent_us: Option<u64>,
    /// Lower bound: the window closed with the operation still open.
    pub open_for_at_least_ms: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FailedOperation {
    pub key: String,
    pub error: String,
}

/// The receipt for one schedule.
#[derive(Clone, Debug, Serialize)]
pub struct LedgerReport {
    pub offered_rate: Quantity,
    pub achieved_rate: Quantity,
    pub achieved_to_offered: Quantity,
    pub schedule_window: Quantity,
    pub completion_window: Quantity,
    pub max_in_flight: Quantity,
    pub drain_timeout: Quantity,
    pub counts: Counts,
    pub intervals: Intervals,
    /// The slowest completed operations by scheduled-to-completed, slowest
    /// first, with every timestamp.
    pub slowest: Vec<OperationRecord>,
    pub unfinished: Vec<OpenOperation>,
    pub errors: Vec<FailedOperation>,
}

const GENERATOR_WINDOW: &str = "generator process, one schedule";

impl ArrivalLedger {
    pub fn summary(&self, slowest: usize) -> LedgerReport {
        let operations = &self.operations;
        let scheduled = operations.len();
        let completed: Vec<&OperationRecord> = operations
            .iter()
            .filter(|record| record.completed().is_some())
            .collect();
        let interval = |quantity, window, pick: &dyn Fn(&OperationRecord) -> Option<u64>| {
            IntervalReport::of(
                quantity,
                window,
                scheduled,
                operations.iter().filter_map(pick).collect(),
            )
        };
        // The departure rate between the first and the last completion, the
        // same shape as the arrival rate between the first and the last
        // scheduled arrival. A service that keeps up at a steady latency
        // reports exactly the offered rate however short the schedule is; a
        // backlog spreads the completions out and lowers it.
        let completions = completed.iter().filter_map(|record| record.completed_us);
        let departures_us =
            completions.clone().max().unwrap_or_default() - completions.min().unwrap_or_default();
        let achieved = if departures_us == 0 {
            0.0
        } else {
            (completed.len() - 1) as f64 * 1_000_000.0 / departures_us as f64
        };
        let mut slow = completed.clone();
        slow.sort_by_key(|record| {
            std::cmp::Reverse(record.completed_us.unwrap_or_default() - record.scheduled_us)
        });
        LedgerReport {
            offered_rate: Quantity {
                quantity: "scheduled arrival rate",
                unit: "operations/s",
                window: "the schedule",
                statistic: "configured",
                value: self.schedule.rate_per_second,
            },
            achieved_rate: Quantity {
                quantity: "completion rate",
                unit: "operations/s",
                window: "first completion to last completion",
                statistic: "(completed count - 1) / window; 0 with fewer than two completions",
                value: achieved,
            },
            achieved_to_offered: Quantity {
                quantity: "completion rate over scheduled arrival rate",
                unit: "ratio",
                window: "first completion to last completion",
                statistic: "measured / configured",
                value: achieved / self.schedule.rate_per_second,
            },
            schedule_window: Quantity {
                quantity: "first scheduled arrival to last scheduled arrival",
                unit: "ms",
                window: "the schedule",
                statistic: "configured: (operations - 1) / rate",
                value: ms(micros(self.schedule.due(scheduled.saturating_sub(1)))),
            },
            completion_window: Quantity {
                quantity: "window open to window close",
                unit: "ms",
                window: GENERATOR_WINDOW,
                statistic: "measured; the drain deadline when operations were unfinished",
                value: ms(self.closed_us),
            },
            max_in_flight: Quantity {
                quantity: "operations admitted to the service at once",
                unit: "operations",
                window: "the schedule",
                statistic: "configured upper bound",
                value: self.schedule.max_in_flight as f64,
            },
            drain_timeout: Quantity {
                quantity: "wait after the last scheduled arrival",
                unit: "ms",
                window: "the schedule",
                statistic: "configured upper bound",
                value: self.schedule.drain_timeout.as_secs_f64() * 1_000.0,
            },
            counts: Counts {
                unit: "operations",
                window: GENERATOR_WINDOW,
                statistic: "count",
                scheduled,
                sent: operations.iter().filter(|r| r.sent_us.is_some()).count(),
                admitted_observed: operations
                    .iter()
                    .filter(|r| r.admitted_us.is_some())
                    .count(),
                settled_observed: operations.iter().filter(|r| r.settled_us.is_some()).count(),
                completed: completed.len(),
                errors: operations
                    .iter()
                    .filter(|r| r.outcome == OperationOutcome::Error)
                    .count(),
                unfinished: operations
                    .iter()
                    .filter(|r| r.outcome == OperationOutcome::Unfinished)
                    .count(),
            },
            intervals: Intervals {
                scheduled_to_completed: interval(
                    "scheduled arrival to caller-visible completion; includes every queue",
                    "completed operations",
                    &|r| OperationRecord::span(Some(r.scheduled_us), r.completed()),
                ),
                scheduled_to_sent: interval(
                    "scheduled arrival to send; generator lateness and the wait for an in-flight slot",
                    "sent operations",
                    &|r| OperationRecord::span(Some(r.scheduled_us), r.sent_us),
                ),
                sent_to_admitted: interval(
                    "send to observed admission",
                    "operations with an observed admission",
                    &|r| OperationRecord::span(r.sent_us, r.admitted_us),
                ),
                admitted_to_settled: interval(
                    "observed admission to observed settlement",
                    "operations with both marks observed",
                    &|r| OperationRecord::span(r.admitted_us, r.settled_us),
                ),
                settled_to_completed: interval(
                    "observed settlement to caller-visible completion",
                    "completed operations with an observed settlement",
                    &|r| OperationRecord::span(r.settled_us, r.completed()),
                ),
                sent_to_completed: interval(
                    "send to caller-visible completion; excludes the wait before the send",
                    "completed operations",
                    &|r| OperationRecord::span(r.sent_us, r.completed()),
                ),
            },
            slowest: slow.into_iter().take(slowest).cloned().collect(),
            unfinished: operations
                .iter()
                .filter(|r| r.outcome == OperationOutcome::Unfinished)
                .map(|r| OpenOperation {
                    key: r.key.clone(),
                    scheduled_us: r.scheduled_us,
                    sent_us: r.sent_us,
                    open_for_at_least_ms: ms(self.closed_us.saturating_sub(r.scheduled_us)),
                })
                .collect(),
            errors: operations
                .iter()
                .filter(|r| r.outcome == OperationOutcome::Error)
                .map(|r| FailedOperation {
                    key: r.key.clone(),
                    error: r.error.clone().unwrap_or_default(),
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// The saturation knee.
// ---------------------------------------------------------------------------

/// Configured criteria for calling a swept rate saturated.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct KneeCriteria {
    /// A step whose completion rate falls below this share of its scheduled
    /// rate has fallen behind.
    pub min_achieved_to_offered: f64,
    /// A step whose scheduled-to-completed p99 exceeds this multiple of the
    /// lowest swept rate's p99 has fallen behind.
    pub max_p99_to_lowest_rate: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct KneeStep {
    pub offered_rate_per_second: f64,
    pub achieved_to_offered: f64,
    pub scheduled_to_completed_p99_ms: Option<f64>,
    pub p99_to_lowest_rate: Option<f64>,
    pub unfinished: usize,
    /// Which criteria this step crossed; empty when it kept up.
    pub fell_behind_on: Vec<&'static str>,
}

/// Where a rate sweep first fell behind its arrivals.
#[derive(Clone, Debug, Serialize)]
pub struct Knee {
    pub criteria: KneeCriteria,
    pub criteria_statistic: &'static str,
    /// The lowest swept rate that fell behind. The knee lies above the last
    /// sustained rate and at or below this one; the sweep does not resolve
    /// it further.
    pub first_saturated_rate_per_second: Option<f64>,
    /// The highest swept rate below it that kept up.
    pub last_sustained_rate_per_second: Option<f64>,
    pub verdict: &'static str,
    pub steps: Vec<KneeStep>,
}

/// `summaries` are one sweep's steps in ascending rate order.
pub fn knee(summaries: &[LedgerReport], criteria: KneeCriteria) -> Knee {
    let p99 = |summary: &LedgerReport| {
        summary
            .intervals
            .scheduled_to_completed
            .as_ref()
            .map(|interval| interval.p99)
    };
    let lowest_p99 = summaries.first().and_then(p99);
    let steps: Vec<KneeStep> = summaries
        .iter()
        .map(|summary| {
            let step_p99 = p99(summary);
            let p99_ratio = step_p99
                .zip(lowest_p99)
                .map(|(step, lowest)| step / lowest.max(f64::EPSILON));
            let mut fell_behind_on = Vec::new();
            if summary.counts.unfinished > 0 {
                fell_behind_on.push("unfinished operations at window close");
            }
            if summary.achieved_to_offered.value < criteria.min_achieved_to_offered {
                fell_behind_on.push("completion rate below the scheduled rate");
            }
            if p99_ratio.is_some_and(|ratio| ratio > criteria.max_p99_to_lowest_rate) {
                fell_behind_on.push("scheduled-to-completed p99 above the lowest rate's");
            }
            KneeStep {
                offered_rate_per_second: summary.offered_rate.value,
                achieved_to_offered: summary.achieved_to_offered.value,
                scheduled_to_completed_p99_ms: step_p99,
                p99_to_lowest_rate: p99_ratio,
                unfinished: summary.counts.unfinished,
                fell_behind_on,
            }
        })
        .collect();
    let first_saturated = steps
        .iter()
        .position(|step| !step.fell_behind_on.is_empty());
    let verdict = match first_saturated {
        Some(0) => "saturated at the lowest swept rate; the knee is at or below it",
        Some(_) => "the knee lies between the last sustained and the first saturated rate",
        None => "not reached; the knee is above the highest swept rate",
    };
    Knee {
        criteria,
        criteria_statistic: "configured",
        first_saturated_rate_per_second: first_saturated
            .map(|index| steps[index].offered_rate_per_second),
        last_sustained_rate_per_second: match first_saturated {
            Some(index) => index
                .checked_sub(1)
                .map(|below| steps[below].offered_rate_per_second),
            None => steps.last().map(|step| step.offered_rate_per_second),
        },
        verdict,
        steps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE: Duration = Duration::from_millis(1);
    const STALL: Duration = Duration::from_millis(500);
    const STALLED_ORDINAL: usize = 20;
    /// Well above the service time and below the stall.
    const VICTIM_MS: f64 = 100.0;

    /// A fixed fake service: one server, `SERVICE` per operation, and one
    /// operation that holds the server for `STALL`.
    fn stalling_server()
    -> impl Fn(Arrival) -> futures_util::future::BoxFuture<'static, anyhow::Result<ServiceMarks>>
    + Send
    + Sync
    + 'static {
        let server = Arc::new(tokio::sync::Mutex::new(()));
        move |arrival| {
            let server = Arc::clone(&server);
            Box::pin(async move {
                let _serving = server.lock().await;
                tokio::time::sleep(if arrival.ordinal == STALLED_ORDINAL {
                    STALL
                } else {
                    SERVICE
                })
                .await;
                Ok(ServiceMarks::default())
            })
        }
    }

    fn schedule(rate_per_second: f64, operations: usize) -> ArrivalSchedule {
        ArrivalSchedule {
            rate_per_second,
            operations,
            max_in_flight: operations,
            drain_timeout: Duration::from_secs(60),
        }
    }

    /// OFFERED-LOAD-TAIL: operations due while the service is stalled wait
    /// out the stall, and scheduled arrivals put every one of them in the
    /// tail. The same service driven closed-loop records the stall once.
    #[tokio::test(start_paused = true)]
    async fn scheduled_arrivals_keep_a_stalls_victims_that_a_closed_loop_hides() {
        let operations = 100;

        let ledger = run_scheduled(schedule(100.0, operations), "open", stalling_server()).await;
        let summary = ledger.summary(3);

        let closed_loop = stalling_server();
        let mut closed_ms = Vec::with_capacity(operations);
        for ordinal in 0..operations {
            let sent = Instant::now();
            closed_loop(Arrival {
                ordinal,
                key: String::new(),
            })
            .await
            .expect("the fake service answers");
            closed_ms.push(sent.elapsed().as_secs_f64() * 1_000.0);
        }
        closed_ms.sort_by(f64::total_cmp);

        let open_victims = ledger
            .operations
            .iter()
            .filter(|r| ms(r.completed_us.expect("completed") - r.scheduled_us) > VICTIM_MS)
            .count();
        let closed_victims = closed_ms.iter().filter(|ms| **ms > VICTIM_MS).count();
        // The stall spans 50 scheduled arrivals; the backlog then drains at
        // nine operations per arrival interval.
        assert!(open_victims >= 40, "open-loop victims: {open_victims}");
        assert_eq!(
            closed_victims, 1,
            "a closed loop times only the stall itself"
        );

        let tail = summary
            .intervals
            .scheduled_to_completed
            .expect("completed operations");
        assert_eq!((tail.n, tail.censored), (operations, 0));
        assert!(tail.p90 > VICTIM_MS && tail.p99 > VICTIM_MS, "{tail:?}");
        assert_eq!(tail.p99_9, tail.max);
        assert_eq!(tail.max, STALL.as_secs_f64() * 1_000.0);
        assert_eq!(
            tail.buckets
                .iter()
                .map(|bucket| bucket.count)
                .sum::<usize>(),
            operations
        );
        assert!(tail.buckets.last().expect("a bucket").le_ms >= tail.max);
        // Nearest-rank p99 of the closed loop's 100 samples is its 99th.
        assert_eq!(closed_ms[98], SERVICE.as_secs_f64() * 1_000.0);
        assert_eq!(
            summary.slowest[0].key,
            format!("open-{STALLED_ORDINAL:06}"),
            "the slowest operation is named"
        );
        assert_eq!(summary.slowest.len(), 3);
    }

    /// OFFERED-LOAD-ACCOUNTING: every scheduled operation ends in exactly
    /// one of completed, error or unfinished; an arrival that finds no free
    /// slot waits with its clock running.
    #[tokio::test(start_paused = true)]
    async fn finite_admission_accounts_for_errors_and_unfinished_operations() {
        let ledger = run_scheduled(
            ArrivalSchedule {
                rate_per_second: 100.0,
                operations: 10,
                max_in_flight: 2,
                drain_timeout: Duration::from_secs(1),
            },
            "op",
            |arrival| async move {
                match arrival.ordinal {
                    3 => anyhow::bail!("refused"),
                    5 => std::future::pending().await,
                    _ => {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok(ServiceMarks {
                            admitted_after_send: Some(Duration::from_millis(10)),
                            settled_after_send: Some(Duration::from_millis(20)),
                            completed_after_send: None,
                        })
                    }
                }
            },
        )
        .await;
        let summary = ledger.summary(1);

        let counts = &summary.counts;
        assert_eq!(
            (counts.scheduled, counts.sent, counts.completed),
            (10, 10, 8)
        );
        assert_eq!((counts.errors, counts.unfinished), (1, 1));
        assert_eq!((counts.admitted_observed, counts.settled_observed), (8, 8));
        assert_eq!(summary.errors[0].key, "op-000003");
        assert_eq!(summary.errors[0].error, "refused");
        let open = &summary.unfinished[0];
        assert_eq!(open.key, "op-000005");
        assert!(open.sent_us.is_some(), "it was sent and never answered");
        // The window closed at the drain deadline: 90 ms + 1 s.
        assert_eq!(summary.completion_window.value, 1_090.0);
        assert_eq!(open.open_for_at_least_ms, 1_040.0);
        // Two slots at 30 ms each cannot keep up with a 10 ms schedule, so
        // later arrivals wait for a slot before they are sent.
        let wait = summary
            .intervals
            .scheduled_to_sent
            .expect("sent operations");
        assert!(wait.max >= 30.0, "{wait:?}");
        let tail = summary
            .intervals
            .scheduled_to_completed
            .expect("completed operations");
        assert_eq!((tail.n, tail.censored), (8, 2));
        assert!(summary.achieved_to_offered.value < 1.0);
        let admitted = summary.intervals.sent_to_admitted.expect("admissions");
        assert_eq!((admitted.min, admitted.max), (10.0, 10.0));
    }

    /// OFFERED-LOAD-KNEE: a server that takes 10 ms sustains 100 operations
    /// a second; the sweep names the first rate above that as saturated and
    /// the last one below it as sustained.
    #[tokio::test(start_paused = true)]
    async fn the_knee_is_the_first_swept_rate_the_service_falls_behind() {
        let mut summaries = Vec::new();
        for rate in [25.0, 50.0, 200.0, 400.0] {
            let server = Arc::new(tokio::sync::Mutex::new(()));
            let ledger = run_scheduled(schedule(rate, 100), "sweep", move |_| {
                let server = Arc::clone(&server);
                async move {
                    let _serving = server.lock().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(ServiceMarks::default())
                }
            })
            .await;
            summaries.push(ledger.summary(0));
        }

        let knee = knee(
            &summaries,
            KneeCriteria {
                min_achieved_to_offered: 0.95,
                max_p99_to_lowest_rate: 2.0,
            },
        );

        assert_eq!(knee.last_sustained_rate_per_second, Some(50.0));
        assert_eq!(knee.first_saturated_rate_per_second, Some(200.0));
        assert!(knee.steps[0].fell_behind_on.is_empty());
        assert!(knee.steps[1].fell_behind_on.is_empty());
        assert_eq!(
            knee.steps[2].fell_behind_on,
            vec![
                "completion rate below the scheduled rate",
                "scheduled-to-completed p99 above the lowest rate's"
            ]
        );
        assert_eq!(knee.steps[0].achieved_to_offered, 1.0);
    }
}
