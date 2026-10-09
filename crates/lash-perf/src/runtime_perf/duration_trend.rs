//! Comparable duration history with advisory, acknowledged level shifts.
//!
//! A five-run elevation opens a shift. Its detection evidence survives
//! compaction and recovery; only a reviewed acknowledgement can close it.
//! Host, compiler, allocator, build, storage policy and workload geometry
//! partition observations. This is functional evidence, never certification
//! of shared-host timings. Run totals and pooled operation medians remain
//! separate named populations.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context;
use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};

use crate::perf_support::git;
use crate::perf_support::report as report_support;
use crate::perf_support::time::round3;

use super::measurement::RuntimePerfScenarioSummary;
#[path = "duration_identity.rs"]
mod identity;
pub(crate) use identity::ExperimentIdentity;

/// Maximum preceding observations used to detect an elevation.
pub(crate) const TREND_WINDOW_RUNS: usize = 20;

/// Advisory elevation relative to the preceding median, in percent.
pub(crate) const DRIFT_THRESHOLD_PCT: f64 = 50.0;

/// Consecutive elevated observations required to open a shift.
pub(crate) const DRIFT_CONSECUTIVE_RUNS: usize = 5;

/// Prior runs required before any verdict is issued at all.
///
/// A median over fewer than five points is a coin flip dressed as a baseline,
/// so a short history reports "insufficient data" rather than a verdict.
pub(crate) const MIN_BASELINE_RUNS: usize = 5;

/// Recent observations retained in addition to open-shift detection evidence.
pub(crate) const RETAINED_RUNS_PER_SERIES: usize = 50;

/// Frozen pre-1.0 history baseline; shapes change in place.
pub(crate) const HISTORY_RECORD_VERSION: u32 = 3;

/// The checked-in accepted level shifts. See the module documentation: this
/// file, not the history, is where an acknowledgement lives, because the
/// history is a CI cache entry nobody reviews and anything can evict.
const PERF_DURATION_LEVEL_SHIFTS_JSON: &str =
    include_str!("../../../../scripts/perf_duration_level_shifts.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LevelShiftFile {
    level_shifts: Vec<LevelShift>,
}

/// One accepted level shift: the instant after which a series' older
/// observations stop being comparable, and the reason they stopped.
///
/// The three selectors narrow what the acceptance covers, and an absent
/// selector means "every one of these". A single scenario's rework names its
/// scenario; a runner generation swap names none of them. `commit` and
/// `reason` are both required and both non-empty: a marker nobody can explain
/// is a mute switch on the only wall-clock signal this repository has.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LevelShift {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    scenario: Option<String>,
    #[serde(default)]
    metric: Option<String>,
    /// The commit that moved the level. Audit trail, not the anchor.
    commit: String,
    /// RFC 3339. Observations recorded before this instant leave the window.
    ///
    /// An instant rather than `commit` because most commits never get a perf
    /// run: a commit-anchored marker whose commit never reached the series
    /// would be a silent no-op, which is the one failure mode an
    /// acknowledgement must not have.
    effective_from: String,
    reason: String,
    who: String,
    disposition: ShiftDisposition,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ShiftDisposition {
    Accepted,
    Bug,
}

impl LevelShift {
    fn matches(&self, profile: &str, scenario: &str, metric: &str) -> bool {
        selector_matches(self.profile.as_deref(), profile)
            && selector_matches(self.scenario.as_deref(), scenario)
            && selector_matches(self.metric.as_deref(), metric)
    }

    fn effective_from(&self) -> Option<DateTime<FixedOffset>> {
        DateTime::parse_from_rfc3339(&self.effective_from).ok()
    }

    /// Enough of the commit to recognise it in a table, taken by character so
    /// a hand-edited file cannot split a multi-byte boundary.
    fn short_commit(&self) -> String {
        self.commit.chars().take(9).collect()
    }
}

/// An absent selector covers every value; a present one must match exactly.
/// Deliberately not a pattern or a prefix: a marker is an acknowledgement of a
/// specific measurement, and a glob is how one quietly grows to cover a
/// regression nobody accepted.
fn selector_matches(selector: Option<&str>, value: &str) -> bool {
    selector.is_none_or(|selector| selector == value)
}

/// Validation is part of parsing rather than a separate gate because an
/// invalid marker is indistinguishable from an absent one at read time, and an
/// acknowledgement that silently did not apply is worse than no file at all.
fn parse_level_shifts(json: &str) -> anyhow::Result<Vec<LevelShift>> {
    let file: LevelShiftFile =
        serde_json::from_str(json).context("parsing the accepted level shifts")?;
    for shift in &file.level_shifts {
        if shift.commit.trim().is_empty() {
            anyhow::bail!("an accepted level shift must name the commit that moved the level");
        }
        if shift.who.trim().is_empty() {
            anyhow::bail!("a level shift must name who acknowledged it");
        }
        if shift.reason.trim().is_empty() {
            anyhow::bail!(
                "the accepted level shift at {} must say why it is accepted",
                shift.commit
            );
        }
        if shift.effective_from().is_none() {
            anyhow::bail!(
                "the accepted level shift at {} has an effective_from that is not RFC 3339: {}",
                shift.commit,
                shift.effective_from
            );
        }
        for (label, selector) in [
            ("profile", shift.profile.as_deref()),
            ("scenario", shift.scenario.as_deref()),
            ("metric", shift.metric.as_deref()),
        ] {
            if selector.is_some_and(|selector| selector.trim().is_empty()) {
                anyhow::bail!(
                    "the accepted level shift at {} has an empty {label}; omit the key to cover every {label}",
                    shift.commit
                );
            }
        }
    }
    Ok(file.level_shifts)
}

#[expect(
    clippy::expect_used,
    reason = "the marker file is checked into the repository at the repository root and parsed once at first use, per the message"
)]
fn checked_in_level_shifts() -> &'static [LevelShift] {
    static SHIFTS: OnceLock<Vec<LevelShift>> = OnceLock::new();
    SHIFTS.get_or_init(|| {
        parse_level_shifts(PERF_DURATION_LEVEL_SHIFTS_JSON)
            .expect("scripts/perf_duration_level_shifts.json must contain valid level shifts")
    })
}

/// The marker that governs one series: the newest applicable one.
///
/// Newest rather than first so a series can be accepted twice — a scenario
/// reworked again after an earlier acceptance — without the older marker
/// holding a stale window open.
fn applicable_level_shift<'a>(
    shifts: &'a [LevelShift],
    profile: &str,
    scenario: &str,
    metric: &str,
) -> Option<&'a LevelShift> {
    shifts
        .iter()
        .filter(|shift| {
            shift.disposition == ShiftDisposition::Accepted
                && shift.matches(profile, scenario, metric)
        })
        .max_by_key(|shift| shift.effective_from())
}

/// The acknowledgement that trimmed a series' comparison window, rendered for
/// an operator reading the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineReset {
    pub(crate) commit: String,
    pub(crate) reason: String,
    pub(crate) who: String,
}

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
/// `total_ms` is the scenario summary's median `total` stage duration — the
/// same statistic the advisory duration guard reads — so the trend and the
/// advisory line are never describing two different numbers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurationHistoryRecord {
    /// See [`HISTORY_RECORD_VERSION`].
    pub(crate) version: u32,
    pub(crate) identity: ExperimentIdentity,
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
    /// A detected shift remains open. `streak` counts retained observations
    /// since its onset, even after recovery; it is not a current elevation streak.
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
            Self::Drifting { streak } => write!(
                formatter,
                "DRIFTING (open; {streak} retained observations since onset)"
            ),
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
    pub(crate) identity: ExperimentIdentity,
    pub(crate) current_ms: f64,
    pub(crate) current_p95_ms: Option<f64>,
    pub(crate) baseline_median_ms: Option<f64>,
    pub(crate) delta_pct: Option<f64>,
    pub(crate) verdict: DriftVerdict,
    /// The accepted level shift that trimmed this series' comparison window,
    /// when one applies. Carried onto the row rather than looked up again by
    /// the renderer so the table and the verdict can never disagree about
    /// which window was used.
    pub(crate) baseline_reset: Option<BaselineReset>,
    pub(crate) bug_acknowledgement: Option<BaselineReset>,
}

/// The per-scenario records this run contributes to the history.
pub(crate) fn records_for_run(
    summaries: &[RuntimePerfScenarioSummary],
    profile: &str,
    geometry: DurationTrendGeometry,
    identity: &ExperimentIdentity,
) -> Vec<DurationHistoryRecord> {
    let commit = history_commit();
    let run_id = history_run_id();
    let recorded_at = Utc::now().to_rfc3339();
    summaries
        .iter()
        // A scenario that skipped its run has no `total` stage — emitting a
        // record for it would write a fabricated zero into the history.
        .filter_map(|summary| {
            summary
                .stage_summary
                .get(super::measurement::stage::TOTAL)
                .map(|total| (summary, total))
        })
        .map(|(summary, total)| DurationHistoryRecord {
            version: HISTORY_RECORD_VERSION,
            identity: identity.for_scenario(&summary.scenario),
            scenario: summary.scenario.clone(),
            profile: profile.to_string(),
            runs: geometry.runs,
            warmups: geometry.warmups,
            turns: geometry.turns,
            build_mode: geometry.build_mode,
            commit: commit.clone(),
            run_id: run_id.clone(),
            recorded_at: recorded_at.clone(),
            total_ms: total.duration_ms.median,
            total_p95_ms: Some(total.duration_ms.p95),
            duration_metrics_ms: summary.metric_summary.iter().filter(|(key, _)| key.ends_with("_ms"))
                .map(|(key, value)| (key.clone(), value))
                .chain(summary.metric_summary_ms.iter().map(|(key, value)| (format!("sampled_operations/{key}"), value)))
                .map(|(key, value)| (key, DurationMetricHistoryValue { median_ms: value.median, p95_ms: value.p95 }))
                .collect(),
        })
        .collect()
}

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
    records.sort_by(|left, right| left.recorded_at.cmp(&right.recorded_at));
    Ok(LoadedHistory {
        records,
        preserved,
        skipped,
    })
}

/// The trailing [`RETAINED_RUNS_PER_SERIES`] observations of every experiment
/// series, plus the bounded witness for each open shift, in chronological order.
pub(crate) fn retained_records(records: &[DurationHistoryRecord]) -> Vec<DurationHistoryRecord> {
    let mut groups = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        groups
            .entry((
                &record.profile,
                &record.scenario,
                record.runs,
                record.warmups,
                record.turns,
                record.build_mode,
                &record.identity,
            ))
            .or_insert_with(Vec::new)
            .push(index);
    }
    let mut keep = vec![false; records.len()];
    for indices in groups.values() {
        for &index in indices.iter().rev().take(RETAINED_RUNS_PER_SERIES) {
            keep[index] = true;
        }
        let mut metrics = BTreeMap::<&str, Vec<(usize, f64)>>::new();
        for &index in indices {
            let record = &records[index];
            metrics
                .entry("total_ms")
                .or_default()
                .push((index, record.total_ms));
            for (metric, value) in &record.duration_metrics_ms {
                metrics
                    .entry(metric)
                    .or_default()
                    .push((index, value.median_ms));
            }
        }
        for (metric, observations) in metrics {
            let record = &records[indices[0]];
            let reset = applicable_level_shift(
                checked_in_level_shifts(),
                &record.profile,
                &record.scenario,
                metric,
            )
            .and_then(LevelShift::effective_from);
            let observations: Vec<_> = observations
                .into_iter()
                .filter(|(index, _)| {
                    reset.is_none_or(|reset| {
                        DateTime::parse_from_rfc3339(&records[*index].recorded_at)
                            .is_ok_and(|at| at >= reset)
                    })
                })
                .collect();
            let values: Vec<_> = observations.iter().map(|(_, value)| *value).collect();
            if let Some((start, _)) = open_shift(&values) {
                for &(index, _) in &observations
                    [start.saturating_sub(TREND_WINDOW_RUNS)..start + DRIFT_CONSECUTIVE_RUNS]
                {
                    keep[index] = true;
                }
            }
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
/// present in the history, ordered for stable output, judged against the
/// checked-in accepted level shifts.
pub(crate) fn trend_rows(
    history: &[DurationHistoryRecord],
    profile_filter: Option<&str>,
) -> Vec<DurationTrendRow> {
    trend_rows_against(history, profile_filter, checked_in_level_shifts())
}

/// [`trend_rows`] against an explicit marker set.
///
/// Separated so the acceptance behaviour is testable without a file on disk:
/// the checked-in set is empty most of the time, and a signal whose only
/// escape hatch is exercised solely by whatever happens to be committed is a
/// signal nobody has actually tested.
fn trend_rows_against(
    history: &[DurationHistoryRecord],
    profile_filter: Option<&str>,
    shifts: &[LevelShift],
) -> Vec<DurationTrendRow> {
    let mut series = BTreeMap::<
        (
            String,
            String,
            String,
            DurationTrendGeometry,
            ExperimentIdentity,
        ),
        Vec<(String, f64, Option<f64>)>,
    >::new();
    for record in history {
        if let Err(reason) = record.identity.compare(&record.identity) {
            eprintln!(
                "warning: duration comparison refused: {reason:?} for {}",
                record.scenario
            );
            continue;
        }
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
                record.identity.clone(),
            ))
            .or_default()
            .push((
                record.recorded_at.clone(),
                record.total_ms,
                record.total_p95_ms,
            ));
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
                    record.identity.clone(),
                ))
                .or_default()
                .push((
                    record.recorded_at.clone(),
                    value.median_ms,
                    Some(value.p95_ms),
                ));
        }
    }
    series
        .into_iter()
        .filter_map(
            |((profile, scenario, metric, geometry, identity), observations)| {
                let (_, newest_ms, newest_p95_ms) = observations.last()?.clone();
                let shift = applicable_level_shift(shifts, &profile, &scenario, &metric);
                let values = comparison_window(&observations, shift, newest_ms);
                let current_ms = *values.last()?;
                let baseline_median_ms = open_shift(&values)
                    .map(|(_, baseline)| baseline)
                    .or_else(|| baseline_median(&values, values.len() - 1));
                Some(DurationTrendRow {
                    scenario: scenario.clone(),
                    profile: profile.clone(),
                    metric: metric.clone(),
                    geometry,
                    identity,
                    current_ms: round3(current_ms),
                    current_p95_ms: newest_p95_ms.map(round3),
                    baseline_median_ms: baseline_median_ms.map(round3),
                    delta_pct: baseline_median_ms
                        .filter(|median| *median > 0.0)
                        .map(|median| round3((current_ms - median) / median * 100.0)),
                    verdict: verdict(&values),
                    bug_acknowledgement: shifts
                        .iter()
                        .filter(|marker| {
                            marker.disposition == ShiftDisposition::Bug
                                && shift.is_none_or(|accepted| {
                                    marker.effective_from() > accepted.effective_from()
                                })
                                && marker.matches(&profile, &scenario, &metric)
                        })
                        .max_by_key(|marker| marker.effective_from())
                        .map(|marker| BaselineReset {
                            commit: marker.short_commit(),
                            reason: marker.reason.clone(),
                            who: marker.who.clone(),
                        }),
                    baseline_reset: shift.map(|shift| BaselineReset {
                        commit: shift.short_commit(),
                        reason: shift.reason.clone(),
                        who: shift.who.clone(),
                    }),
                })
            },
        )
        .collect()
}

/// The observations a verdict may read: everything, or everything from the
/// accepted shift onwards.
///
/// The newest observation is never trimmed. A marker moves the window a run is
/// judged against; it cannot delete the run itself, so a marker dated ahead of
/// every observation leaves one point and "insufficient data" rather than
/// making the series vanish from the table.
///
/// An observation whose timestamp will not parse is excluded, and only when a
/// marker applies: it cannot be placed on either side of the reset, and
/// keeping it would let unplaceable data hold up a baseline the operator just
/// declared stale.
fn comparison_window(
    observations: &[(String, f64, Option<f64>)],
    shift: Option<&LevelShift>,
    newest_ms: f64,
) -> Vec<f64> {
    let Some(effective_from) = shift.and_then(LevelShift::effective_from) else {
        return observations.iter().map(|(_, median, _)| *median).collect();
    };
    let window = observations
        .iter()
        .filter(|(recorded_at, _, _)| {
            DateTime::parse_from_rfc3339(recorded_at).is_ok_and(|at| at >= effective_from)
        })
        .map(|(_, median, _)| *median)
        .collect::<Vec<_>>();
    if window.is_empty() {
        vec![newest_ms]
    } else {
        window
    }
}

pub(crate) fn verdict(series: &[f64]) -> DriftVerdict {
    let Some(current_index) = series.len().checked_sub(1) else {
        return DriftVerdict::InsufficientData { runs: 0 };
    };
    if baseline_median(series, current_index).is_none() {
        return DriftVerdict::InsufficientData { runs: series.len() };
    }
    if let Some((start, _)) = open_shift(series) {
        return DriftVerdict::Drifting {
            streak: series.len() - start,
        };
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

/// Earliest unacknowledged detection and the baseline before its first elevated run.
/// Replaying retained evidence keeps the finding open even after recovery.
fn open_shift(series: &[f64]) -> Option<(usize, f64)> {
    let mut streak = 0;
    for (index, value) in series.iter().enumerate() {
        if baseline_median(series, index)
            .is_some_and(|baseline| exceeds_threshold(*value, baseline))
        {
            streak += 1;
        } else {
            streak = 0;
        }
        if streak >= DRIFT_CONSECUTIVE_RUNS {
            let start = index + 1 - streak;
            return baseline_median(series, start).map(|baseline| (start, baseline));
        }
    }
    None
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
        "quantities: total_ms = scenario run envelope; *_ms = per-run metric; sampled_operations/* = pooled operation samples within one receipt; baseline = median of receipt medians; all durations in ms\n\
         runtime perf duration trend (advisory; baseline = preceding median, frozen while a shift is open, \
         drift = >{DRIFT_THRESHOLD_PCT:.0}% for {DRIFT_CONSECUTIVE_RUNS} consecutive runs; \
         reset_at = reviewed accepted level shift, scripts/perf_duration_level_shifts.json)\n"
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
    let reset_width = rows
        .iter()
        .filter_map(|row| row.baseline_reset.as_ref())
        .map(|reset| reset.commit.len())
        .max()
        .unwrap_or(1)
        .max("reset_at".len());
    out.push_str(&format!(
        "  {:scenario_width$}  {:profile_width$}  {:metric_width$}  {:geometry_width$}  {:>12}  {:>12}  {:>12}  {:>9}  {:reset_width$}  {}\n",
        "scenario", "profile", "metric", "geometry", "current_median_ms", "current_p95_ms", "baseline_median_ms", "delta", "reset_at", "verdict"
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
        let reset = row
            .baseline_reset
            .as_ref()
            .map(|reset| reset.commit.clone())
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "  {:scenario_width$}  {:profile_width$}  {:metric_width$}  {:geometry_width$}  {:>12.3}  {:>12}  {:>12}  {:>9}  {:reset_width$}  {}\n",
            row.scenario, row.profile, row.metric, geometry, row.current_ms, p95, median, delta, reset, row.verdict
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        if let Some(marker) = &row.bug_acknowledgement {
            out.push_str(&format!(
                "  open bug: {} {} {} reported by {} at {}: {}\n",
                row.profile, row.scenario, row.metric, marker.who, marker.commit, marker.reason
            ));
        }
        out.push_str(&format!(
            "  population {index}: {}\n",
            serde_json::to_string(&row.identity).unwrap_or_default()
        ));
        for other in &rows[..index] {
            if row.profile == other.profile
                && row.scenario == other.scenario
                && row.metric == other.metric
            {
                let comparable = row.identity.compare(&other.identity).and_then(|()| {
                    if row.geometry == other.geometry {
                        Ok(())
                    } else {
                        Err(identity::ComparisonRefusal::IdentityMismatch(
                            identity::IdentityDimension::Geometry,
                        ))
                    }
                });
                if let Err(reason) = comparable {
                    out.push_str(&format!(
                        "  comparison refused: {reason:?} (populations are separate)\n"
                    ));
                }
            }
        }
    }
    out.push_str(&render_accepted_level_shifts(rows));
    out
}

/// The acceptances that trimmed a window in this table, one line each.
///
/// The `reset_at` column says a window moved; this says who accepted it and
/// why. Without it a short baseline is indistinguishable from an evicted
/// cache, and an operator has to go read a JSON file to tell the two apart.
fn render_accepted_level_shifts(rows: &[DurationTrendRow]) -> String {
    let mut reasons = BTreeMap::<&str, (&str, &str, usize)>::new();
    for reset in rows.iter().filter_map(|row| row.baseline_reset.as_ref()) {
        let entry = reasons.entry(reset.commit.as_str()).or_insert((
            reset.reason.as_str(),
            reset.who.as_str(),
            0,
        ));
        entry.2 += 1;
    }
    if reasons.is_empty() {
        return String::new();
    }
    let mut out = String::from("  accepted level shifts in effect:\n");
    for (commit, (reason, who, series)) in reasons {
        out.push_str(&format!(
            "    {commit}: {who} accepted: {reason} ({series} series measured against a window that starts there)\n"
        ));
    }
    out
}

/// Deliberately returns nothing: no caller can turn drift into an exit code.
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
            "runtime perf duration drift: {} {} ({}) is {} against the frozen pre-shift median of {} ms ({})",
            row.scenario,
            row.metric,
            row.profile,
            delta,
            row.baseline_median_ms
                .map(|value| format!("{value:.3}"))
                .unwrap_or_else(|| "-".to_string()),
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
         A detected shift stays open until a committed acknowledgement accepts it."
    );
}

/// Append this run's observations, then print the trend table and any drift
/// warning.
///
/// The history lives in a CI cache the perf run does not own, and an unwritable cache entry is
/// an infrastructure fact, not a statement about the code under test — turning it into a red
/// main would make this advisory signal gate the build by the back door.
/// It is still never a silent all-clear: the failure is named on stderr in place of the table,
/// and the standalone `duration-trend` command exits non-zero on the same input so a human can
/// see it deliberately.
pub(crate) fn record_and_report(
    path: &Path,
    profile: &str,
    geometry: DurationTrendGeometry,
    identity: &ExperimentIdentity,
    summaries: &[RuntimePerfScenarioSummary],
) {
    if let Err(error) = record_and_render(path, profile, geometry, identity, summaries) {
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
    identity: &ExperimentIdentity,
    summaries: &[RuntimePerfScenarioSummary],
) -> anyhow::Result<()> {
    // Sweep any scratch file a previously killed rewrite left behind. It sits
    // inside the directory the CI cache saves, so nothing else would ever
    // remove it.
    let _ = std::fs::remove_file(rewrite_temp_path(path));

    append_records(
        path,
        &records_for_run(summaries, profile, geometry, identity),
    )?;
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

/// Export the same identity-partitioned history for offline review.
pub fn export_duration_history_csv(
    history: &Path,
    profile: Option<&str>,
    out: &Path,
) -> anyhow::Result<()> {
    let records = load_history(history)?;
    let mut csv = String::from(
        "recorded_at,commit,run_id,profile,workload,host,compiler,allocator,build_profile,backend,configured_durability,configured_geometry,runs,warmups,turns,build_mode,quantity,unit,window,statistic,value\n",
    );
    for record in records
        .iter()
        .filter(|record| profile.is_none_or(|profile| profile == record.profile))
    {
        let identity = &record.identity;
        let common = [
            record.recorded_at.clone(),
            record.commit.clone(),
            record.run_id.clone(),
            record.profile.clone(),
            record.scenario.clone(),
            identity.host.clone(),
            identity.compiler.clone(),
            identity.allocator.clone(),
            identity.build_profile.clone(),
            identity.backend.clone(),
            identity.configured_durability.clone(),
            identity.configured_geometry.clone(),
            record.runs.to_string(),
            record.warmups.to_string(),
            record.turns.to_string(),
            record.build_mode.to_string(),
        ];
        let mut observations = vec![("total_ms".to_string(), "median", record.total_ms)];
        if let Some(value) = record.total_p95_ms {
            observations.push(("total_ms".to_string(), "p95", value));
        }
        for (metric, values) in &record.duration_metrics_ms {
            observations.push((metric.clone(), "median", values.median_ms));
            observations.push((metric.clone(), "p95", values.p95_ms));
        }
        for (metric, statistic, value) in observations {
            let mut fields = common.to_vec();
            fields.extend([
                metric,
                "ms".to_string(),
                "one_perf_receipt".to_string(),
                statistic.to_string(),
                value.to_string(),
            ]);
            csv.push_str(
                &fields
                    .iter()
                    .map(|field| format!("\"{}\"", field.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            csv.push('\n');
        }
    }
    std::fs::write(out, csv).context("writing duration history CSV")
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
#[path = "duration_trend_tests.rs"]
mod tests;
