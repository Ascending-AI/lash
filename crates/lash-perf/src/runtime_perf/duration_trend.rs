//! Durable per-scenario wall-clock history and the sustained-drift signal
//! computed from it.
//!
//! FIG-1385 demoted every runtime duration ceiling to advisory: on shared
//! runners the same binary measured 7.67 ms and then 1.625 ms against the same
//! 0.25 ms phase ceiling, so a hard single-run duration gate either flakes or
//! is widened until it detects nothing. That ruling stands. It left one real
//! hole: a wall-clock regression that allocates nothing — added blocking I/O,
//! lock contention, an accidental sleep — has no gate at all, because the
//! allocation ceilings cannot see it.
//!
//! This module closes the hole the only way single-run measurement allows:
//! trend, not threshold. Every main-branch perf run appends one record per
//! scenario to an append-only history, and the drift signal fires only when a
//! scenario has sat above its own trailing median for several consecutive main
//! runs. Nothing here can fail a run — the signal is advisory by construction,
//! which is what keeps it from re-litigating FIG-1385.
//!
//! Records also carry duration-valued whole-scenario scheduler observations
//! (`process.cpu_ms` and, where available, `runtime.worker_busy_ms`). They use
//! the same advisory series logic. Ratios, worker counts, queue depths, and
//! park counts do not fit this duration contract and are not trended here.
//!
//! # What this signal does not see
//!
//! **It is a transition detector, not a level check.** A step change is loud
//! for a bounded window and then goes quiet, because the new level walks into
//! the trailing baseline it is compared against. With the constants below, a
//! step reads `Elevated` on post-step runs 1-4, `DRIFTING` on runs 5-10, and
//! `Stable` from run 11 — the point where more than half the twenty-run window
//! is post-step, so the median has moved. Magnitude buys almost nothing: a step
//! from `B` to `E` stays `DRIFTING` for exactly one extra run (quiet from run
//! 12) iff `E > 3B`, and never for more than that. That boundary is not a
//! tuning choice, it falls out of the straddling median: at run 11 the window
//! holds ten pre-step and ten post-step runs, so the median is `(B + E) / 2`
//! and the run is elevated iff `E > 1.5 * (B + E) / 2`, i.e. `E > 3B`. Six or
//! seven main runs of loud output is the whole visibility budget; a regression
//! nobody looks at in that window becomes the new normal silently. Persisting a
//! level-shift marker so the signal survives its own baseline is a possible
//! follow-up, deliberately not built here.
//!
//! The quick-profile main-push smoke supplies its cache-backed history path.
//! Full and release profiles write a sibling ledger next to their uploaded
//! report. Records carry their size preset and series are keyed by it, so full
//! observations cannot contaminate the quick one.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::perf_support::git;
use crate::perf_support::report as report_support;
use crate::perf_support::time::round3;

use super::measurement::RuntimePerfScenarioSummary;

/// Trailing main-branch runs the baseline median is taken over (N).
///
/// Twenty runs is roughly a day of trunk traffic. Long enough that a couple of
/// loaded-runner outliers cannot move the median; short enough that the
/// baseline tracks the current runner generation instead of comparing today
/// against a months-old machine.
///
/// It also sets the closing edge of the signal: once more than half this window
/// is post-regression the median moves and the verdict returns to `Stable`, at
/// post-step run `N / 2 + 1` — one run later for a step larger than 3x, where
/// the even-length median straddles the step at exactly that boundary.
pub(crate) const TREND_WINDOW_RUNS: usize = 20;

/// How far above its trailing median a run must sit to count as elevated (X).
///
/// Runner noise here is multiplicative and one-sided — a shared runner gets
/// slower, never faster than its own hardware — and FIG-1385's evidence puts
/// the single-run spread at several-fold. So 50% is not, on its own, the
/// discriminator between noise and regression; the streak below is. 50% is
/// picked as the level a genuine wall-clock regression worth a human's
/// attention clears, while staying above the everyday jitter band so the
/// streak is not fed by ordinary scheduling.
///
/// Magnitude decides almost nothing about how long the signal lasts: a step is
/// loud for six main runs, or seven if it is larger than 3x, because either way
/// it is simply "above the baseline" until the baseline itself moves.
pub(crate) const DRIFT_THRESHOLD_PCT: f64 = 50.0;

/// Consecutive elevated main runs before the signal fires (K).
///
/// One loaded run clears 50% often enough to be worthless as a signal. Five in
/// a row does not: even at a generous one-in-five chance of a single run being
/// elevated by noise alone, five consecutive is ~3e-4 per scenario per run,
/// which across the whole scenario list is a false signal every few hundred
/// main commits — rare enough that a human still reads it. A real regression
/// is present on every subsequent run, so it trips on the fifth main commit
/// after it lands.
///
/// This is the opening edge; [`TREND_WINDOW_RUNS`] sets the closing one. The
/// two together give a visibility window of
/// `TREND_WINDOW_RUNS / 2 + 1 - DRIFT_CONSECUTIVE_RUNS` = 6 main runs, seven
/// for a step larger than 3x.
pub(crate) const DRIFT_CONSECUTIVE_RUNS: usize = 5;

/// Prior runs required before any verdict is issued at all.
///
/// A median over fewer than five points is a coin flip dressed as a baseline,
/// so a short history reports "insufficient data" rather than a verdict.
pub(crate) const MIN_BASELINE_RUNS: usize = 5;

/// Observations kept per `(profile, scenario)` series when the history is
/// rewritten.
///
/// Comfortably more than the `TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS` the
/// verdict actually reads, so truncation can never change a verdict, while
/// still bounding a file that would otherwise grow forever inside a CI cache
/// entry that is never evicted.
pub(crate) const RETAINED_RUNS_PER_SERIES: usize = 50;

/// Schema generation of a history record.
///
/// A schema bump regenerates the local history file with the new record shape;
/// there is intentionally no migration arm for older local history. Forwards:
/// a record whose `version` is *newer* than this build's is neither read through
/// a schema it does not match nor deleted — it is excluded from verdicts and
/// carried through any rewrite byte-for-byte, because an older binary meets
/// newer records on any revert or rerun of a pre-bump commit, and it is not
/// entitled to destroy them.
pub(crate) const HISTORY_RECORD_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BuildMode {
    Debug,
    Release,
}

impl BuildMode {
    pub(crate) const fn current() -> Self {
        if cfg!(debug_assertions) {
            Self::Debug
        } else {
            Self::Release
        }
    }
}

impl fmt::Display for BuildMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Debug => "debug",
            Self::Release => "release",
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DurationTrendGeometry {
    pub(crate) runs: usize,
    pub(crate) warmups: usize,
    pub(crate) turns: usize,
    pub(crate) build_mode: BuildMode,
}

impl DurationTrendGeometry {
    pub(crate) const fn current(runs: usize, warmups: usize, turns: usize) -> Self {
        Self {
            runs,
            warmups,
            turns,
            build_mode: BuildMode::current(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurationMetricHistoryValue {
    pub(crate) median_ms: f64,
    pub(crate) p95_ms: f64,
}

/// One durable observation: one scenario's median and p95 wall clock from one
/// perf run.
///
/// `total_ms` is the scenario summary's median `total_ms` — the same statistic
/// the advisory duration guard reads — so the trend and the advisory line are
/// never describing two different numbers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurationHistoryRecord {
    /// See [`HISTORY_RECORD_VERSION`].
    pub(crate) version: u32,
    pub(crate) scenario: String,
    /// The benchmark size preset (`quick`, `full`, ...). Durations are only
    /// comparable within one preset, so it is part of the series key rather
    /// than decoration.
    pub(crate) profile: String,
    pub(crate) runs: usize,
    pub(crate) warmups: usize,
    pub(crate) turns: usize,
    pub(crate) build_mode: BuildMode,
    pub(crate) commit: String,
    pub(crate) run_id: String,
    pub(crate) recorded_at: String,
    pub(crate) total_ms: f64,
    /// The same run's p95 wall clock. `None` means a legacy record predating
    /// percentile reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total_p95_ms: Option<f64>,
    /// Additional whole-scenario duration observations. Ratios and counters
    /// deliberately stay out of this wall-clock-duration trend contract.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) duration_metrics_ms: BTreeMap<String, DurationMetricHistoryValue>,
}

/// A history read leniently: what parsed, what could not, and what this build
/// is not entitled to read *or* delete.
#[derive(Debug, Clone)]
pub(crate) struct LoadedHistory {
    pub(crate) records: Vec<DurationHistoryRecord>,
    /// Raw lines from a newer schema generation, in file order.
    ///
    /// This build cannot read them into a verdict, but it must not destroy
    /// them either: an older binary meets newer records whenever a revert
    /// lands or a pre-bump commit is re-run, and if compaction dropped them
    /// the very next save on `main` would make that loss permanent. They are
    /// carried through byte-for-byte and left for a build that understands
    /// them.
    pub(crate) preserved: Vec<String>,
    /// Reasons, one per genuinely unparseable line, already rendered for an
    /// operator. These *are* dropped on rewrite — nothing can ever read them.
    pub(crate) skipped: Vec<String>,
}

/// What the history says about the newest observation of one series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriftVerdict {
    /// Too few prior runs to form a baseline. Never a verdict about the code.
    InsufficientData { runs: usize },
    /// The newest run is not elevated against its trailing median.
    Stable,
    /// Elevated, but for fewer than [`DRIFT_CONSECUTIVE_RUNS`] runs — the shape
    /// a single loaded runner produces, so it is reported and nothing more.
    Elevated { streak: usize },
    /// Elevated for [`DRIFT_CONSECUTIVE_RUNS`] consecutive runs: sustained
    /// drift a human can act on.
    Drifting { streak: usize },
}

impl DriftVerdict {
    pub(crate) fn is_drifting(self) -> bool {
        matches!(self, Self::Drifting { .. })
    }
}

impl fmt::Display for DriftVerdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsufficientData { runs } => {
                write!(formatter, "insufficient data ({runs} run(s))")
            }
            Self::Stable => write!(formatter, "stable"),
            Self::Elevated { streak } => write!(formatter, "elevated ({streak} run(s))"),
            Self::Drifting { streak } => write!(formatter, "DRIFTING ({streak} run(s))"),
        }
    }
}

/// One rendered row of the trend table: what this run measured, what the
/// history says it used to measure, and the verdict connecting the two.
#[derive(Debug, Clone)]
pub(crate) struct DurationTrendRow {
    pub(crate) scenario: String,
    pub(crate) profile: String,
    pub(crate) metric: String,
    pub(crate) geometry: DurationTrendGeometry,
    pub(crate) current_ms: f64,
    pub(crate) current_p95_ms: Option<f64>,
    pub(crate) baseline_median_ms: Option<f64>,
    pub(crate) delta_pct: Option<f64>,
    pub(crate) verdict: DriftVerdict,
}

/// The per-scenario records this run contributes to the history.
pub(crate) fn records_for_run(
    summaries: &[RuntimePerfScenarioSummary],
    profile: &str,
    geometry: DurationTrendGeometry,
) -> Vec<DurationHistoryRecord> {
    let commit = history_commit();
    let run_id = history_run_id();
    let recorded_at = Utc::now().to_rfc3339();
    summaries
        .iter()
        .map(|summary| DurationHistoryRecord {
            version: HISTORY_RECORD_VERSION,
            scenario: summary.scenario.clone(),
            profile: profile.to_string(),
            runs: geometry.runs,
            warmups: geometry.warmups,
            turns: geometry.turns,
            build_mode: geometry.build_mode,
            commit: commit.clone(),
            run_id: run_id.clone(),
            recorded_at: recorded_at.clone(),
            total_ms: summary.total_ms.median,
            total_p95_ms: Some(summary.total_ms.p95),
            duration_metrics_ms: summary
                .metric_summary
                .iter()
                .filter(|(key, _)| key.ends_with("_ms"))
                .map(|(key, value)| {
                    (
                        key.clone(),
                        DurationMetricHistoryValue {
                            median_ms: value.median,
                            p95_ms: value.p95,
                        },
                    )
                })
                .collect(),
        })
        .collect()
}

/// Append records to the history file, creating it (and its parent) if needed.
///
/// Append-only within a run: a run adds its own observations and never rewrites
/// another run's, so a partial write cannot corrupt earlier history.
///
/// Two main runs in flight at once do *not* merge. Each restores the same
/// cache entry, appends to its own copy, and saves under its own key; the
/// prefix restore-key then picks exactly one of them next time and the other
/// lineage is orphaned. The cost is a lost observation, which shortens a
/// series by one and cannot change a verdict's direction — the trailing median
/// and the streak are both computed over whatever observations survived.
pub(crate) fn append_records(path: &Path, records: &[DurationHistoryRecord]) -> anyhow::Result<()> {
    report_support::ensure_parent_dir(path, "duration trend history")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening duration trend history {}", path.display()))?;
    let mut buffer = String::new();
    for record in records {
        buffer.push_str(&serde_json::to_string(record)?);
        buffer.push('\n');
    }
    file.write_all(buffer.as_bytes())
        .with_context(|| format!("appending to duration trend history {}", path.display()))?;
    Ok(())
}

/// Read a JSONL history strictly: any unparseable line is an error.
///
/// This is the human-facing read, used by the standalone `duration-trend`
/// command. Someone who points the tool at a file wants to be told the file is
/// broken, not handed a quietly shortened series.
pub(crate) fn load_history(path: &Path) -> anyhow::Result<Vec<DurationHistoryRecord>> {
    let loaded = load_history_lenient(path)?;
    if let Some(first) = loaded.skipped.first() {
        anyhow::bail!(
            "duration trend history {} has {} unparseable record(s); first: {first}",
            path.display(),
            loaded.skipped.len()
        );
    }
    if !loaded.preserved.is_empty() {
        anyhow::bail!(
            "duration trend history {} has {} record(s) from a newer schema generation than this build understands; \
             rebuild lash-perf to read them",
            path.display(),
            loaded.preserved.len()
        );
    }
    Ok(loaded.records)
}

/// Read a JSONL history leniently: unparseable lines are dropped and counted.
///
/// This is the CI read. A history is a cache artifact spanning schema changes,
/// truncated writes and the occasional hand edit; one bad line must cost one
/// observation, not the entire signal. `record_and_render` rewrites the file
/// from what parsed, so a bad line is dropped once instead of being re-saved
/// into every future cache entry.
///
/// A missing file is an empty history, not an error: the first main run after
/// the cache is evicted has nothing to read and must still report.
pub(crate) fn load_history_lenient(path: &Path) -> anyhow::Result<LoadedHistory> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading duration trend history {}", path.display()));
        }
    };
    let mut records = Vec::new();
    let mut preserved = Vec::new();
    let mut skipped = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_number = index + 1;
        match serde_json::from_str::<DurationHistoryRecord>(line) {
            // Readable JSON, unreadable schema. Not a verdict input, and not
            // this build's to delete either — see `LoadedHistory::preserved`.
            Ok(record) if record.version > HISTORY_RECORD_VERSION => {
                preserved.push(line.to_string());
            }
            Ok(record) => records.push(record),
            Err(error) => skipped.push(format!("line {line_number}: {error}")),
        }
    }
    // Appends from separate runs can land out of order; the series is defined
    // by observation time, not by who won the write.
    records.sort_by(|left, right| left.recorded_at.cmp(&right.recorded_at));
    Ok(LoadedHistory {
        records,
        preserved,
        skipped,
    })
}

/// The trailing [`RETAINED_RUNS_PER_SERIES`] observations of every
/// `(profile, scenario, geometry)` series, in the original chronological order.
pub(crate) fn retained_records(records: &[DurationHistoryRecord]) -> Vec<DurationHistoryRecord> {
    let mut seen_from_newest: BTreeMap<(&str, &str, usize, usize, usize, BuildMode), usize> =
        BTreeMap::new();
    let mut keep = vec![false; records.len()];
    for (index, record) in records.iter().enumerate().rev() {
        let count = seen_from_newest
            .entry((
                record.profile.as_str(),
                record.scenario.as_str(),
                record.runs,
                record.warmups,
                record.turns,
                record.build_mode,
            ))
            .or_default();
        if *count < RETAINED_RUNS_PER_SERIES {
            *count += 1;
            keep[index] = true;
        }
    }
    records
        .iter()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .map(|(record, _)| record.clone())
        .collect()
}

/// Replace the history with exactly these records plus these raw lines,
/// atomically.
///
/// `preserved` lines are written back byte-for-byte: they are records this
/// build cannot interpret, and rewriting them in any form would be a guess.
///
/// Written to a sibling temp file and renamed so an interrupted rewrite leaves
/// the previous history intact rather than a half file. The temp file is
/// removed on a failed write so a crashed rewrite cannot leave an orphan
/// sitting inside the cached directory forever.
pub(crate) fn compact_history(
    path: &Path,
    records: &[DurationHistoryRecord],
    preserved: &[String],
) -> anyhow::Result<()> {
    report_support::ensure_parent_dir(path, "duration trend history")?;
    let mut buffer = String::new();
    for record in records {
        buffer.push_str(&serde_json::to_string(record)?);
        buffer.push('\n');
    }
    for line in preserved {
        buffer.push_str(line);
        buffer.push('\n');
    }
    let temp_path = rewrite_temp_path(path);
    if let Err(error) = std::fs::write(&temp_path, buffer.as_bytes()) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error)
            .with_context(|| format!("writing duration trend history {}", temp_path.display()));
    }
    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error)
            .with_context(|| format!("replacing duration trend history {}", path.display()));
    }
    Ok(())
}

/// The sibling scratch file [`compact_history`] renames into place. Sibling
/// rather than `/tmp` because the rename must stay on one filesystem to be
/// atomic; the cost is an orphan to sweep, which the run path does on entry.
fn rewrite_temp_path(path: &Path) -> std::path::PathBuf {
    path.with_extension("jsonl.rewrite")
}

/// One trend row per `(profile, scenario, duration metric, geometry)` series
/// present in the history, ordered for stable output.
pub(crate) fn trend_rows(
    history: &[DurationHistoryRecord],
    profile_filter: Option<&str>,
) -> Vec<DurationTrendRow> {
    let mut series =
        BTreeMap::<(String, String, String, DurationTrendGeometry), Vec<(f64, Option<f64>)>>::new();
    for record in history {
        if profile_filter.is_some_and(|profile| profile != record.profile) {
            continue;
        }
        series
            .entry((
                record.profile.clone(),
                record.scenario.clone(),
                "total_ms".to_string(),
                DurationTrendGeometry {
                    runs: record.runs,
                    warmups: record.warmups,
                    turns: record.turns,
                    build_mode: record.build_mode,
                },
            ))
            .or_default()
            .push((record.total_ms, record.total_p95_ms));
        for (metric, value) in &record.duration_metrics_ms {
            series
                .entry((
                    record.profile.clone(),
                    record.scenario.clone(),
                    metric.clone(),
                    DurationTrendGeometry {
                        runs: record.runs,
                        warmups: record.warmups,
                        turns: record.turns,
                        build_mode: record.build_mode,
                    },
                ))
                .or_default()
                .push((value.median_ms, Some(value.p95_ms)));
        }
    }
    series
        .into_iter()
        .filter_map(|((profile, scenario, metric, geometry), records)| {
            let values = records
                .iter()
                .map(|(median, _)| *median)
                .collect::<Vec<_>>();
            let current_ms = *values.last()?;
            let baseline_median_ms = baseline_median(&values, values.len() - 1);
            Some(DurationTrendRow {
                scenario,
                profile,
                metric,
                geometry,
                current_ms: round3(current_ms),
                current_p95_ms: records.last().and_then(|(_, p95)| *p95).map(round3),
                baseline_median_ms: baseline_median_ms.map(round3),
                delta_pct: baseline_median_ms
                    .filter(|median| *median > 0.0)
                    .map(|median| round3((current_ms - median) / median * 100.0)),
                verdict: verdict(&values),
            })
        })
        .collect()
}

/// The verdict for the newest observation of one chronological series.
pub(crate) fn verdict(series: &[f64]) -> DriftVerdict {
    let Some(current_index) = series.len().checked_sub(1) else {
        return DriftVerdict::InsufficientData { runs: 0 };
    };
    if baseline_median(series, current_index).is_none() {
        return DriftVerdict::InsufficientData { runs: series.len() };
    }
    let streak = drift_streak(series);
    if streak >= DRIFT_CONSECUTIVE_RUNS {
        DriftVerdict::Drifting { streak }
    } else if streak > 0 {
        DriftVerdict::Elevated { streak }
    } else {
        DriftVerdict::Stable
    }
}

/// How many consecutive runs, counting back from the newest, sat above their
/// own trailing median.
///
/// Each run is judged against the window that preceded *it*, never against one
/// shared baseline taken from the newest run. On a series that has been
/// swinging, those two readings genuinely disagree: a shared baseline is
/// dragged up by the very runs it is meant to judge, so an established
/// regression scores a short streak and reads `Elevated` instead of
/// `DRIFTING`. Judging each run against its own past is what makes a sustained
/// shift accumulate a streak while a single spike scores exactly one — and it
/// is why the streak stops at the first run whose own history is too short to
/// judge.
fn drift_streak(series: &[f64]) -> usize {
    let mut streak = 0;
    for index in (0..series.len()).rev() {
        match baseline_median(series, index) {
            Some(baseline) if exceeds_threshold(series[index], baseline) => streak += 1,
            _ => break,
        }
    }
    streak
}

/// The median of up to [`TREND_WINDOW_RUNS`] observations preceding `index`,
/// or `None` when fewer than [`MIN_BASELINE_RUNS`] precede it.
fn baseline_median(series: &[f64], index: usize) -> Option<f64> {
    let window = &series[index.saturating_sub(TREND_WINDOW_RUNS)..index];
    if window.len() < MIN_BASELINE_RUNS {
        return None;
    }
    let mut sorted = window.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    })
}

fn exceeds_threshold(value: f64, baseline: f64) -> bool {
    value > baseline * (1.0 + DRIFT_THRESHOLD_PCT / 100.0)
}

fn geometry_label(geometry: DurationTrendGeometry) -> String {
    format!(
        "runs={} warmups={} turns={} build={}",
        geometry.runs, geometry.warmups, geometry.turns, geometry.build_mode
    )
}

/// The trend table, as printed by the run path and by the standalone CLI.
pub(crate) fn render_trend_table(rows: &[DurationTrendRow]) -> String {
    let mut out = format!(
        "runtime perf duration trend (advisory; baseline = median of the last {TREND_WINDOW_RUNS} runs, \
         drift = >{DRIFT_THRESHOLD_PCT:.0}% for {DRIFT_CONSECUTIVE_RUNS} consecutive runs)\n"
    );
    if rows.is_empty() {
        out.push_str("  (no history records)\n");
        return out;
    }
    let scenario_width = rows
        .iter()
        .map(|row| row.scenario.len())
        .max()
        .unwrap_or(8)
        .max("scenario".len());
    let profile_width = rows
        .iter()
        .map(|row| row.profile.len())
        .max()
        .unwrap_or(7)
        .max("profile".len());
    let metric_width = rows
        .iter()
        .map(|row| row.metric.len())
        .max()
        .unwrap_or(6)
        .max("metric".len());
    let geometry_width = rows
        .iter()
        .map(|row| geometry_label(row.geometry).len())
        .max()
        .unwrap_or(8)
        .max("geometry".len());
    out.push_str(&format!(
        "  {:scenario_width$}  {:profile_width$}  {:metric_width$}  {:geometry_width$}  {:>12}  {:>12}  {:>12}  {:>9}  {}\n",
        "scenario", "profile", "metric", "geometry", "current_ms", "p95_ms", "median_ms", "delta", "verdict"
    ));
    for row in rows {
        let p95 = row
            .current_p95_ms
            .map(|value| format!("{value:.3}"))
            .unwrap_or_else(|| "-".to_string());
        let median = row
            .baseline_median_ms
            .map(|value| format!("{value:.3}"))
            .unwrap_or_else(|| "-".to_string());
        let delta = row
            .delta_pct
            .map(|value| format!("{value:+.1}%"))
            .unwrap_or_else(|| "-".to_string());
        let geometry = geometry_label(row.geometry);
        out.push_str(&format!(
            "  {:scenario_width$}  {:profile_width$}  {:metric_width$}  {:geometry_width$}  {:>12.3}  {:>12}  {:>12}  {:>9}  {}\n",
            row.scenario, row.profile, row.metric, geometry, row.current_ms, p95, median, delta, row.verdict
        ));
    }
    out
}

/// Print the drifting rows loudly, and as GitHub annotations when running
/// under Actions. Deliberately returns nothing: no caller can turn drift into
/// an exit code.
pub(crate) fn report_drift(rows: &[DurationTrendRow]) {
    let drifting = rows
        .iter()
        .filter(|row| row.verdict.is_drifting())
        .collect::<Vec<_>>();
    if drifting.is_empty() {
        return;
    }
    let github_actions = std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true");
    for row in &drifting {
        let delta = row
            .delta_pct
            .map(|value| format!("{value:+.1}%"))
            .unwrap_or_else(|| "unknown".to_string());
        let message = format!(
            "runtime perf duration drift: {} {} ({}) is {} against a trailing median of {} ms over the last {} runs ({})",
            row.scenario,
            row.metric,
            row.profile,
            delta,
            row.baseline_median_ms
                .map(|value| format!("{value:.3}"))
                .unwrap_or_else(|| "-".to_string()),
            TREND_WINDOW_RUNS,
            row.verdict,
        );
        // Annotations go to stderr like every other perf warning: this
        // binary's stdout is a JSON contract `scripts/profile_runtime.py`
        // parses, and that script re-emits workflow-command lines on its own
        // stdout where the Actions runner reads them.
        if github_actions {
            eprintln!("::warning title=Runtime perf duration drift::{message}");
        }
        eprintln!("warning: {message}");
    }
    eprintln!(
        "warning: duration drift is advisory and never fails the run (FIG-1385): \
         it reports sustained wall-clock movement that allocation ceilings cannot see. \
         It is loud for about six main runs after a step change and then goes quiet."
    );
}

/// Append this run's observations, then print the trend table and any drift
/// warning.
///
/// Returns nothing on purpose. The history lives in a CI cache the perf run
/// does not own, and an unwritable cache entry is an infrastructure fact, not
/// a statement about the code under test — turning it into a red main would
/// make this advisory signal gate the build by the back door. It is still
/// never a silent all-clear: the failure is named on stderr in place of the
/// table, and the standalone `duration-trend` command exits non-zero on the
/// same input so a human can see it deliberately.
pub(crate) fn record_and_report(
    path: &Path,
    profile: &str,
    geometry: DurationTrendGeometry,
    summaries: &[RuntimePerfScenarioSummary],
) {
    if let Err(error) = record_and_render(path, profile, geometry, summaries) {
        eprintln!(
            "warning: runtime perf duration trend unavailable ({error:#}); \
             the drift signal is silent for this run, which is not an all-clear"
        );
    }
}

fn record_and_render(
    path: &Path,
    profile: &str,
    geometry: DurationTrendGeometry,
    summaries: &[RuntimePerfScenarioSummary],
) -> anyhow::Result<()> {
    // Sweep any scratch file a previously killed rewrite left behind. It sits
    // inside the directory the CI cache saves, so nothing else would ever
    // remove it.
    let _ = std::fs::remove_file(rewrite_temp_path(path));

    append_records(path, &records_for_run(summaries, profile, geometry))?;
    let loaded = load_history_lenient(path)?;
    if !loaded.skipped.is_empty() {
        eprintln!(
            "warning: runtime perf duration history {}: skipped {} unparseable record(s), \
             which are dropped from the rewritten history:\n{}",
            path.display(),
            loaded.skipped.len(),
            loaded.skipped.join("\n")
        );
    }
    if !loaded.preserved.is_empty() {
        eprintln!(
            "warning: runtime perf duration history {}: {} record(s) come from a newer schema \
             generation than this build understands; they are excluded from the table below and \
             carried through the rewrite unchanged",
            path.display(),
            loaded.preserved.len()
        );
    }
    let rows = trend_rows(&loaded.records, Some(profile));
    eprint!("{}", render_trend_table(&rows));
    report_drift(&rows);

    // Self-heal, and bound growth. The file saved back to the cache carries
    // only what parsed, plus the trailing observations any verdict can read,
    // plus every line a newer build wrote. So a bad line costs one observation
    // once instead of riding into every future cache entry; the entry cannot
    // grow without limit just because every run touches it; and an older build
    // meeting newer records loses nothing. A wholesale reset — after a
    // scenario is redefined, say — is `gh cache delete` on the
    // `perf-duration-history-quick-*` keys.
    let retained = retained_records(&loaded.records);
    let rewrite_needed = !loaded.skipped.is_empty() || retained.len() != loaded.records.len();
    if rewrite_needed && let Err(error) = compact_history(path, &retained, &loaded.preserved) {
        // The table above was rendered from a good read and stands. Only the
        // write-back failed, so say that rather than claiming the trend was
        // unavailable.
        eprintln!(
            "warning: runtime perf duration history {} could not be rewritten ({error:#}); \
             the trend above is correct, but unparseable records and overlong series persist \
             into the next run",
            path.display()
        );
    }
    Ok(())
}

/// `lash-perf duration-trend --history <FILE>`: print the trend table for an
/// existing history without running the benchmark, so the signal is testable
/// and reviewable off CI. Strict about malformed records, unlike the CI path.
pub fn run_duration_trend_cli(history: &Path, profile: Option<&str>) -> anyhow::Result<()> {
    let records = load_history(history)?;
    let rows = trend_rows(&records, profile);
    print!("{}", render_trend_table(&rows));
    report_drift(&rows);
    Ok(())
}

fn history_commit() -> String {
    non_empty_env("GITHUB_SHA")
        .or_else(git::head_commit)
        .unwrap_or_else(|| "unknown".to_string())
}

fn history_run_id() -> String {
    non_empty_env("GITHUB_RUN_ID").unwrap_or_else(|| "local".to_string())
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat series at `value`, long enough to establish a baseline.
    fn flat(value: f64, runs: usize) -> Vec<f64> {
        vec![value; runs]
    }

    #[test]
    fn short_history_never_yields_a_verdict() {
        for runs in 0..=MIN_BASELINE_RUNS {
            let series = flat(10.0, runs);
            assert_eq!(
                verdict(&series),
                DriftVerdict::InsufficientData { runs },
                "{runs} run(s) must not produce a verdict"
            );
        }
        // One more run than the baseline minimum is the first judgeable point.
        assert_eq!(
            verdict(&flat(10.0, MIN_BASELINE_RUNS + 1)),
            DriftVerdict::Stable
        );
    }

    #[test]
    fn a_single_spike_does_not_trip_the_signal() {
        let mut series = flat(10.0, TREND_WINDOW_RUNS);
        series.push(100.0);

        assert_eq!(verdict(&series), DriftVerdict::Elevated { streak: 1 });
        assert!(!verdict(&series).is_drifting());
    }

    #[test]
    fn a_spike_that_recovers_leaves_no_streak_behind() {
        let mut series = flat(10.0, TREND_WINDOW_RUNS);
        series.push(100.0);
        series.push(10.0);

        assert_eq!(verdict(&series), DriftVerdict::Stable);
    }

    #[test]
    fn drift_shorter_than_the_streak_requirement_stays_advisory_only() {
        let mut series = flat(10.0, TREND_WINDOW_RUNS);
        series.extend(flat(20.0, DRIFT_CONSECUTIVE_RUNS - 1));

        assert_eq!(
            verdict(&series),
            DriftVerdict::Elevated {
                streak: DRIFT_CONSECUTIVE_RUNS - 1
            }
        );
    }

    #[test]
    fn sustained_drift_over_the_streak_requirement_trips_the_signal() {
        let mut series = flat(10.0, TREND_WINDOW_RUNS);
        series.extend(flat(20.0, DRIFT_CONSECUTIVE_RUNS));

        let result = verdict(&series);
        assert_eq!(
            result,
            DriftVerdict::Drifting {
                streak: DRIFT_CONSECUTIVE_RUNS
            }
        );
        assert!(result.is_drifting());
    }

    #[test]
    fn a_run_exactly_at_the_threshold_is_not_elevated() {
        let mut series = flat(10.0, TREND_WINDOW_RUNS);
        series.push(10.0 * (1.0 + DRIFT_THRESHOLD_PCT / 100.0));

        assert_eq!(verdict(&series), DriftVerdict::Stable);
    }

    #[test]
    fn everyday_jitter_under_the_threshold_never_accumulates_a_streak() {
        let mut series = flat(10.0, TREND_WINDOW_RUNS);
        // Alternating ±40%: far noisier than a real runner, still under 50%.
        for index in 0..(DRIFT_CONSECUTIVE_RUNS * 4) {
            series.push(if index.is_multiple_of(2) { 14.0 } else { 6.0 });
        }

        assert_eq!(verdict(&series), DriftVerdict::Stable);
    }

    /// The streak is per-run-against-its-own-window, and this is the series
    /// that proves it: replacing every window with one shared baseline taken
    /// from the newest run turns a `Drifting` verdict into `Elevated`, because
    /// the shared baseline is dragged up by the very drifted runs it judges.
    /// Every other test in this module passes under that mutation.
    #[test]
    fn a_swinging_series_distinguishes_per_run_windows_from_one_shared_baseline() {
        let series = [
            9.53, 16.73, 23.65, 10.37, 9.86, 10.65, 22.57, 15.93, 25.57, 24.04, 10.52, 17.43, 9.21,
            10.0, 98.53, 103.81, 10.1, 10.53, 9.64, 10.11, 104.8, 98.89, 23.32, 22.7, 100.35,
            106.62,
        ];

        // Per-run windows: the last six runs each cleared their own trailing
        // median, including the two ~23 ms runs whose own windows sat near
        // 13 ms. A single shared baseline of 20.0 (the newest run's window)
        // would score only the last two and report Elevated.
        assert_eq!(verdict(&series), DriftVerdict::Drifting { streak: 6 });

        let shared_baseline = baseline_median(&series, series.len() - 1).expect("baseline");
        let shared_streak = series
            .iter()
            .rev()
            .take_while(|value| exceeds_threshold(**value, shared_baseline))
            .count();
        assert_eq!(
            shared_streak, 2,
            "the shared-baseline reading must genuinely differ, or this test proves nothing"
        );
        assert!(shared_streak < DRIFT_CONSECUTIVE_RUNS);
    }

    /// The closing edge the module documentation promises, pinned as
    /// behaviour rather than left as prose: a regression is loud for a
    /// bounded number of main runs and then becomes the new normal.
    #[test]
    fn a_step_change_is_loud_for_a_bounded_window_then_ages_into_the_baseline() {
        let stepped = |multiplier: f64, post_step_runs: usize| {
            let mut series = flat(10.0, TREND_WINDOW_RUNS * 2);
            series.extend(flat(10.0 * multiplier, post_step_runs));
            verdict(&series)
        };

        // A doubling: Elevated 1-4, DRIFTING 5-10, Stable from 11.
        for post_step_runs in 1..DRIFT_CONSECUTIVE_RUNS {
            assert_eq!(
                stepped(2.0, post_step_runs),
                DriftVerdict::Elevated {
                    streak: post_step_runs
                },
                "post-step run {post_step_runs}"
            );
        }
        for post_step_runs in DRIFT_CONSECUTIVE_RUNS..=10 {
            assert_eq!(
                stepped(2.0, post_step_runs),
                DriftVerdict::Drifting {
                    streak: post_step_runs
                },
                "post-step run {post_step_runs}"
            );
        }
        for post_step_runs in 11..=14 {
            assert_eq!(
                stepped(2.0, post_step_runs),
                DriftVerdict::Stable,
                "post-step run {post_step_runs}"
            );
        }

        // Magnitude buys exactly one extra run and no more, and the boundary
        // is not a tuning choice: at run 11 the window holds ten pre-step and
        // ten post-step runs, so the median is (B + E) / 2 and the run is
        // elevated iff E > 1.5 * (B + E) / 2, i.e. iff E > 3B. Pinned either
        // side of exactly 3x.
        assert_eq!(
            stepped(3.0, 11),
            DriftVerdict::Stable,
            "exactly 3x does not clear the straddling median"
        );
        assert_eq!(
            stepped(3.01, 11),
            DriftVerdict::Drifting { streak: 11 },
            "just past 3x does"
        );
        // The extra run is all it buys, at any magnitude.
        assert_eq!(stepped(3.01, 12), DriftVerdict::Stable);
        assert_eq!(stepped(10.0, 11), DriftVerdict::Drifting { streak: 11 });
        assert_eq!(stepped(10.0, 12), DriftVerdict::Stable);
        assert_eq!(stepped(100.0, 12), DriftVerdict::Stable);
    }

    #[test]
    fn the_baseline_window_is_bounded_to_the_trailing_runs() {
        // A very old, very slow era must not hold the baseline up forever.
        let mut series = flat(1_000.0, TREND_WINDOW_RUNS * 2);
        series.extend(flat(10.0, TREND_WINDOW_RUNS));
        series.extend(flat(20.0, DRIFT_CONSECUTIVE_RUNS));

        assert_eq!(
            verdict(&series),
            DriftVerdict::Drifting {
                streak: DRIFT_CONSECUTIVE_RUNS
            }
        );
    }

    #[test]
    fn series_are_keyed_by_profile_and_scenario_together() {
        let mut history = Vec::new();
        for index in 0..(TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS) {
            let drifted = index >= TREND_WINDOW_RUNS;
            history.push(record("standard", "quick", index, 10.0));
            history.push(record(
                "standard",
                "full",
                index,
                if drifted { 200.0 } else { 100.0 },
            ));
        }

        let rows = trend_rows(&history, None);
        assert_eq!(rows.len(), 2);
        let full = rows.iter().find(|row| row.profile == "full").unwrap();
        let quick = rows.iter().find(|row| row.profile == "quick").unwrap();
        assert!(full.verdict.is_drifting(), "{:?}", full.verdict);
        assert_eq!(quick.verdict, DriftVerdict::Stable);
        assert_eq!(full.baseline_median_ms, Some(100.0));
        assert_eq!(full.delta_pct, Some(100.0));

        let filtered = trend_rows(&history, Some("quick"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].profile, "quick");
    }

    #[test]
    fn history_round_trips_through_the_file_and_sorts_by_observation_time() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("history.jsonl");

        assert!(
            load_history(&path)
                .expect("missing history is empty")
                .is_empty()
        );

        append_records(&path, &[record("standard", "quick", 2, 30.0)]).expect("append late");
        append_records(&path, &[record("standard", "quick", 1, 20.0)]).expect("append early");

        let loaded = load_history(&path).expect("history loads");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].total_ms, 20.0);
        assert_eq!(loaded[1].total_ms, 30.0);
        assert_eq!(loaded[0].total_p95_ms, Some(25.0));
    }

    #[test]
    fn a_record_missing_geometry_is_rejected_without_a_migration_arm() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.jsonl");
        std::fs::write(
            &path,
            "{\"scenario\":\"standard\",\"profile\":\"quick\",\"commit\":\"abc\",\
             \"run_id\":\"1\",\"recorded_at\":\"2026-01-01T00:00:00Z\",\"total_ms\":10.0}\n",
        )
        .expect("write");

        assert!(load_history(&path).is_err());
        let loaded = load_history_lenient(&path).expect("history scan completes");
        assert!(loaded.records.is_empty());
        assert_eq!(loaded.skipped.len(), 1);
        assert!(loaded.preserved.is_empty());
    }

    #[test]
    fn a_record_from_a_newer_schema_is_excluded_from_verdicts_but_not_dropped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.jsonl");
        let mut future = record("standard", "quick", 1, 10.0);
        future.version = HISTORY_RECORD_VERSION + 1;
        append_records(&path, &[future, record("standard", "quick", 2, 11.0)]).expect("append");

        let loaded = load_history_lenient(&path).expect("history loads");
        assert_eq!(loaded.records.len(), 1);
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
        // Unreadable is not the same as unwanted: it is held, not counted.
        assert_eq!(loaded.preserved.len(), 1);
        assert!(
            loaded.preserved[0].contains(&format!("\"version\":{}", HISTORY_RECORD_VERSION + 1))
        );
    }

    /// An older build restored onto a newer history — a revert push, or a
    /// rerun of a pre-bump commit — must not be the thing that destroys the
    /// newer records, because `main` would save that loss on the very next
    /// run. The rewrite carries them byte-for-byte.
    #[test]
    fn an_older_build_carries_newer_records_through_the_rewrite_verbatim() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.jsonl");

        let mut future = record("standard", "quick", 1, 10.0);
        future.version = HISTORY_RECORD_VERSION + 1;
        let future_line = serde_json::to_string(&future).expect("serialize");
        // A readable record, an unreadable-schema record, and a corrupt line:
        // the rewrite must keep the first, keep the second untouched, and drop
        // only the third.
        std::fs::write(
            &path,
            format!(
                "{}\n{future_line}\nnot json\n",
                serde_json::to_string(&record("standard", "quick", 2, 11.0)).expect("serialize")
            ),
        )
        .expect("write");

        record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

        let rewritten = std::fs::read_to_string(&path).expect("read back");
        let lines = rewritten.lines().collect::<Vec<_>>();
        assert!(
            lines.contains(&future_line.as_str()),
            "newer record must survive byte-identical, got:\n{rewritten}"
        );
        assert!(!rewritten.contains("not json"), "{rewritten}");
        assert_eq!(lines.len(), 2, "{rewritten}");

        // And it is still not readable as a verdict input by this build.
        let loaded = load_history_lenient(&path).expect("history loads");
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.preserved.len(), 1);
    }

    #[test]
    fn a_scratch_file_left_by_a_killed_rewrite_is_swept_on_the_next_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.jsonl");
        // A clean, short history: this run needs no compaction at all, so the
        // rewrite will not incidentally consume the orphan. Only the explicit
        // sweep can remove it, which is the whole point.
        append_records(&path, &[record("standard", "quick", 1, 10.0)]).expect("append");
        std::fs::write(rewrite_temp_path(&path), "stale\n").expect("write orphan");

        record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

        assert!(
            !rewrite_temp_path(&path).exists(),
            "the scratch file is inside the directory CI caches; nothing else sweeps it"
        );
        // The history itself is untouched by the sweep.
        assert_eq!(load_history(&path).expect("history loads").len(), 1);
    }

    #[test]
    fn the_strict_read_refuses_a_history_the_lenient_read_salvages() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.jsonl");
        append_records(&path, &[record("standard", "quick", 1, 10.0)]).expect("append");
        std::fs::write(
            &path,
            format!(
                "{}truncated{{\n",
                std::fs::read_to_string(&path).expect("read")
            ),
        )
        .expect("write");

        let error = load_history(&path).expect_err("strict read must fail loudly");
        assert!(format!("{error:#}").contains("line 2"), "{error:#}");

        let loaded = load_history_lenient(&path).expect("lenient read salvages");
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.skipped.len(), 1);
    }

    #[test]
    fn a_poisoned_history_is_healed_instead_of_carried_forward() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.jsonl");
        append_records(&path, &[record("standard", "quick", 1, 10.0)]).expect("append");
        std::fs::write(
            &path,
            format!(
                "{}not json\n",
                std::fs::read_to_string(&path).expect("read")
            ),
        )
        .expect("write");

        // The run path must return normally *and* leave a clean file behind,
        // or the bad line rides into every future cache entry.
        record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

        let healed = load_history(&path).expect("history is parseable again");
        assert_eq!(healed.len(), 1);
        assert_eq!(healed[0].total_ms, 10.0);
    }

    #[test]
    fn an_unwritable_history_disables_the_signal_without_failing_the_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A directory where the file should be: every write fails, and the run
        // must still return normally.
        let path = dir.path().join("history.jsonl");
        std::fs::create_dir(&path).expect("occupy the path");

        record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

        assert!(run_duration_trend_cli(&path, Some("quick")).is_err());
    }

    #[test]
    fn rewriting_bounds_each_series_without_touching_a_verdict() {
        let readable = TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS;
        assert!(
            RETAINED_RUNS_PER_SERIES > readable,
            "retention must exceed what a verdict reads, or truncation changes verdicts"
        );
        assert!(
            RETAINED_RUNS_PER_SERIES <= 4 * readable,
            "retention must stay a small multiple of what a verdict reads: the history \
             lives in a cache entry every main run touches, so nothing evicts it but this"
        );

        let mut history = Vec::new();
        for index in 0..(RETAINED_RUNS_PER_SERIES * 2) {
            // Two series, so retention is proven to be per-series and not global.
            history.push(record("standard", "quick", index, 10.0));
            history.push(record("rlm", "quick", index, 20.0));
        }
        // The tail that any verdict can see, before and after truncation.
        history.extend(
            (0..DRIFT_CONSECUTIVE_RUNS)
                .map(|index| record("standard", "quick", 10_000 + index, 40.0)),
        );

        let before = verdict(
            &history
                .iter()
                .filter(|record| record.scenario == "standard")
                .map(|record| record.total_ms)
                .collect::<Vec<_>>(),
        );
        let retained = retained_records(&history);
        let after = verdict(
            &retained
                .iter()
                .filter(|record| record.scenario == "standard")
                .map(|record| record.total_ms)
                .collect::<Vec<_>>(),
        );

        assert_eq!(retained.len(), RETAINED_RUNS_PER_SERIES * 2);
        assert_eq!(
            retained
                .iter()
                .filter(|record| record.scenario == "standard")
                .count(),
            RETAINED_RUNS_PER_SERIES
        );
        assert_eq!(before, after);
        assert!(before.is_drifting(), "{before:?}");
    }

    #[test]
    fn a_long_history_is_truncated_on_disk_by_the_run_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.jsonl");
        let overlong = (0..(RETAINED_RUNS_PER_SERIES + 20))
            .map(|index| record("standard", "quick", index, 10.0))
            .collect::<Vec<_>>();
        append_records(&path, &overlong).expect("append");

        record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

        let healed = load_history(&path).expect("history loads");
        assert_eq!(healed.len(), RETAINED_RUNS_PER_SERIES);
    }

    #[test]
    fn the_committed_fixture_demonstrates_every_verdict() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("duration-trend-history.jsonl");
        let history = load_history(&path).expect("fixture loads");
        let rows = trend_rows(&history, Some("quick"));

        let by_scenario = rows
            .iter()
            .map(|row| (row.scenario.as_str(), row.verdict))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(by_scenario["standard"], DriftVerdict::Stable);
        assert_eq!(by_scenario["rlm"], DriftVerdict::Elevated { streak: 1 });
        assert_eq!(
            by_scenario["deep_turn_composition"],
            DriftVerdict::Drifting {
                streak: DRIFT_CONSECUTIVE_RUNS
            }
        );
        assert!(matches!(
            by_scenario["store_reopen"],
            DriftVerdict::InsufficientData { .. }
        ));

        let table = render_trend_table(&rows);
        assert!(table.contains("deep_turn_composition"), "{table}");
        assert!(table.contains("DRIFTING"), "{table}");
        assert!(table.contains("insufficient data"), "{table}");
    }

    #[test]
    fn an_empty_history_renders_a_table_rather_than_a_verdict() {
        let table = render_trend_table(&[]);
        assert!(table.contains("no history records"), "{table}");
    }

    #[test]
    fn same_labels_with_different_geometry_are_distinct_series() {
        let mut debug = record("standard", "quick", 1, 10.0);
        debug.runs = 2;
        debug.warmups = 0;
        debug.turns = 3;
        debug.build_mode = BuildMode::Debug;

        let mut release = record("standard", "quick", 2, 20.0);
        release.runs = 5;
        release.warmups = 1;
        release.turns = 12;
        release.build_mode = BuildMode::Release;

        let rows = trend_rows(&[debug, release], Some("quick"));
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.geometry
            == DurationTrendGeometry {
                runs: 2,
                warmups: 0,
                turns: 3,
                build_mode: BuildMode::Debug,
            }));
        assert!(rows.iter().any(|row| row.geometry
            == DurationTrendGeometry {
                runs: 5,
                warmups: 1,
                turns: 12,
                build_mode: BuildMode::Release,
            }));
    }

    fn record(scenario: &str, profile: &str, index: usize, total_ms: f64) -> DurationHistoryRecord {
        DurationHistoryRecord {
            version: HISTORY_RECORD_VERSION,
            scenario: scenario.to_string(),
            profile: profile.to_string(),
            runs: 2,
            warmups: 0,
            turns: 3,
            build_mode: BuildMode::current(),
            commit: format!("commit{index:04}"),
            run_id: format!("{index}"),
            recorded_at: format!("2026-01-01T{:02}:{:02}:00Z", index / 60, index % 60),
            total_ms,
            total_p95_ms: Some(total_ms + 5.0),
            duration_metrics_ms: BTreeMap::new(),
        }
    }
}
