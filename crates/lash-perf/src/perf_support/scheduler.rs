//! Process and Tokio scheduler measurements for whole scenario windows.
//!
//! A window starts immediately before a scenario future is polled and ends
//! immediately after it resolves. Process CPU is the process-wide `utime +
//! stime` delta from `/proc/self/stat`, so it includes every thread in this
//! process during that wall-clock bracket. Tokio busy time and park counts are
//! sums of per-worker counter deltas from the runtime executing the scenario.
//! The global-queue value is the maximum of the endpoint samples; stable Tokio
//! metrics do not expose a historical high-water mark.
//!
//! None of these process- or worker-wide counters can attribute CPU to an
//! individual turn. In particular, this module intentionally emits no
//! `turn.cpu_ms` (or other `turn.cpu*`) key.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::time::round3;

const AT_CLKTCK: u64 = 17;

#[derive(Clone, Debug)]
pub struct RuntimeSchedulerSample {
    captured_at: Instant,
    process_cpu_ticks: Option<u64>,
    process_clock_ticks_per_second: Option<u64>,
    num_workers: usize,
    global_queue_depth: usize,
    #[cfg(target_has_atomic = "64")]
    worker_total_busy: Vec<Duration>,
    #[cfg(target_has_atomic = "64")]
    worker_park_count: Vec<u64>,
}

impl RuntimeSchedulerSample {
    pub fn capture() -> Self {
        let metrics = tokio::runtime::Handle::current().metrics();
        let num_workers = metrics.num_workers();
        Self {
            captured_at: Instant::now(),
            process_cpu_ticks: process_cpu_ticks(),
            process_clock_ticks_per_second: process_clock_ticks_per_second(),
            num_workers,
            global_queue_depth: metrics.global_queue_depth(),
            #[cfg(target_has_atomic = "64")]
            worker_total_busy: (0..num_workers)
                .map(|worker| metrics.worker_total_busy_duration(worker))
                .collect(),
            #[cfg(target_has_atomic = "64")]
            worker_park_count: (0..num_workers)
                .map(|worker| metrics.worker_park_count(worker))
                .collect(),
        }
    }

    pub fn window_metric_samples(&self, after: &Self) -> BTreeMap<String, Vec<f64>> {
        let wall_ms = after
            .captured_at
            .saturating_duration_since(self.captured_at)
            .as_secs_f64()
            * 1_000.0;
        let mut metrics = BTreeMap::from([
            (
                "runtime.workers".to_string(),
                vec![after.num_workers as f64],
            ),
            (
                "runtime.global_queue_depth_max".to_string(),
                vec![self.global_queue_depth.max(after.global_queue_depth) as f64],
            ),
        ]);

        if let (Some(before_ticks), Some(after_ticks), Some(ticks_per_second)) = (
            self.process_cpu_ticks,
            after.process_cpu_ticks,
            self.process_clock_ticks_per_second
                .filter(|rate| Some(*rate) == after.process_clock_ticks_per_second),
        ) {
            let cpu_ms =
                after_ticks.saturating_sub(before_ticks) as f64 / ticks_per_second as f64 * 1_000.0;
            metrics.insert("process.cpu_ms".to_string(), vec![round3(cpu_ms)]);
            metrics.insert(
                "process.cpu_utilization".to_string(),
                vec![round3(ratio(cpu_ms, wall_ms))],
            );
        }

        #[cfg(target_has_atomic = "64")]
        {
            let busy_ms = self
                .worker_total_busy
                .iter()
                .zip(&after.worker_total_busy)
                .map(|(before, after)| after.saturating_sub(*before).as_secs_f64() * 1_000.0)
                .sum::<f64>();
            let park_count = self
                .worker_park_count
                .iter()
                .zip(&after.worker_park_count)
                .map(|(before, after)| after.saturating_sub(*before))
                .sum::<u64>();
            metrics.insert("runtime.worker_busy_ms".to_string(), vec![round3(busy_ms)]);
            metrics.insert(
                "runtime.busy_fraction".to_string(),
                vec![round3(
                    ratio(busy_ms, wall_ms * after.num_workers as f64).min(1.0),
                )],
            );
            metrics.insert(
                "runtime.worker_park_count".to_string(),
                vec![park_count as f64],
            );
        }

        metrics
    }
}

fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        (numerator / denominator).max(0.0)
    } else {
        0.0
    }
}

fn process_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    parse_process_cpu_ticks(&stat)
}

fn process_clock_ticks_per_second() -> Option<u64> {
    static CLOCK_TICKS_PER_SECOND: OnceLock<Option<u64>> = OnceLock::new();
    *CLOCK_TICKS_PER_SECOND.get_or_init(|| {
        let auxv = std::fs::read("/proc/self/auxv").ok()?;
        parse_auxv_clock_ticks(&auxv)
    })
}

fn parse_auxv_clock_ticks(auxv: &[u8]) -> Option<u64> {
    let word_bytes = std::mem::size_of::<usize>();
    for pair in auxv.chunks_exact(word_bytes * 2) {
        let key = native_word(&pair[..word_bytes]);
        let value = native_word(&pair[word_bytes..]);
        if key == AT_CLKTCK {
            return (value > 0).then_some(value);
        }
        if key == 0 {
            break;
        }
    }
    None
}

fn native_word(bytes: &[u8]) -> u64 {
    if cfg!(target_endian = "little") {
        bytes
            .iter()
            .enumerate()
            .fold(0_u64, |value, (index, byte)| {
                value | (u64::from(*byte) << (index * 8))
            })
    } else {
        bytes
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
    }
}

fn parse_process_cpu_ticks(stat: &str) -> Option<u64> {
    // `comm` is parenthesized and may contain spaces or `)` characters. The
    // final `)` is therefore the only safe boundary before field 3 (`state`).
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    utime.checked_add(stime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_stat_parser_handles_spaces_and_closing_parentheses_in_comm() {
        let stat = "42 (lash perf) worker) R 1 2 3 4 5 6 7 8 9 10 120 30 0 0";

        assert_eq!(parse_process_cpu_ticks(stat), Some(150));
    }

    #[test]
    fn process_stat_parser_rejects_truncated_input() {
        assert_eq!(parse_process_cpu_ticks("42 (lash-perf) R 1 2"), None);
    }

    #[test]
    fn auxv_parser_reads_the_process_clock_tick_rate() {
        let mut auxv = Vec::new();
        auxv.extend_from_slice(&(AT_CLKTCK as usize).to_ne_bytes());
        auxv.extend_from_slice(&250_usize.to_ne_bytes());
        auxv.extend_from_slice(&0_usize.to_ne_bytes());
        auxv.extend_from_slice(&0_usize.to_ne_bytes());

        assert_eq!(parse_auxv_clock_ticks(&auxv), Some(250));
    }
}
