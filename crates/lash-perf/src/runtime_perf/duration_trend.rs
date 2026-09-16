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
//! nobody looks at in that window becomes the new normal silently.
//!
//! # Accepted level shifts
//!
//! The other half of that transition is the *intended* step: a change that
//! makes a scenario legitimately slower, or a runner generation swap. The
//! signal reads it as drift and says so for six main runs, and those six runs
//! of expected red are what teaches an operator to stop reading the section --
//! which costs the next real regression its only audience.
//!
//! [`LevelShift`] is the acknowledgement. A marker in
//! `scripts/perf_duration_level_shifts.json` names the commit that shifted the
//! level, the instant from which observations are comparable again, and why it
//! is accepted; every observation recorded before that instant leaves the
//! series' comparison window. The series then behaves exactly like a fresh
//! one: no verdict until [`MIN_BASELINE_RUNS`] observations have accumulated
//! past the marker, so the accepted step stops warning on the very next run,
//! and a *further* shift on top of it trips the signal again once it has built
//! its own streak.
//!
//! The marker is checked in rather than written into the history because the
//! history is a CI cache entry: it can be evicted, it is not reviewed, and
//! nothing in it can be pointed at afterwards. A file in the tree is reviewed
//! in the pull request that causes the shift, carries a mandatory reason, and
//! survives a `gh cache delete`. Anchoring is by timestamp, not by commit,
//! because a perf run does not exist for most commits -- a commit-anchored
//! marker whose commit never reached the series would be a silent no-op.
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
use std::sync::OnceLock;

use anyhow::Context;
use chrono::{DateTime, FixedOffset, Utc};
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

/// Parse and validate a marker file.
///
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
        .filter(|shift| shift.matches(profile, scenario, metric))
        .max_by_key(|shift| shift.effective_from())
}

/// The acknowledgement that trimmed a series' comparison window, rendered for
/// an operator reading the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineReset {
    pub(crate) commit: String,
    pub(crate) reason: String,
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
    /// The accepted level shift that trimmed this series' comparison window,
    /// when one applies. Carried onto the row rather than looked up again by
    /// the renderer so the table and the verdict can never disagree about
    /// which window was used.
    pub(crate) baseline_reset: Option<BaselineReset>,
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
        (String, String, String, DurationTrendGeometry),
        Vec<(String, f64, Option<f64>)>,
    >::new();
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
        .filter_map(|((profile, scenario, metric, geometry), observations)| {
            let (_, newest_ms, newest_p95_ms) = observations.last()?.clone();
            let shift = applicable_level_shift(shifts, &profile, &scenario, &metric);
            let values = comparison_window(&observations, shift, newest_ms);
            let current_ms = *values.last()?;
            let baseline_median_ms = baseline_median(&values, values.len() - 1);
            Some(DurationTrendRow {
                scenario,
                profile,
                metric,
                geometry,
                current_ms: round3(current_ms),
                current_p95_ms: newest_p95_ms.map(round3),
                baseline_median_ms: baseline_median_ms.map(round3),
                delta_pct: baseline_median_ms
                    .filter(|median| *median > 0.0)
                    .map(|median| round3((current_ms - median) / median * 100.0)),
                verdict: verdict(&values),
                baseline_reset: shift.map(|shift| BaselineReset {
                    commit: shift.short_commit(),
                    reason: shift.reason.clone(),
                }),
            })
        })
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
         drift = >{DRIFT_THRESHOLD_PCT:.0}% for {DRIFT_CONSECUTIVE_RUNS} consecutive runs; \
         reset_at = accepted level shift, scripts/perf_duration_level_shifts.json)\n"
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
        "scenario", "profile", "metric", "geometry", "current_ms", "p95_ms", "median_ms", "delta", "reset_at", "verdict"
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
    out.push_str(&render_accepted_level_shifts(rows));
    out
}

/// The acceptances that trimmed a window in this table, one line each.
///
/// The `reset_at` column says a window moved; this says who accepted it and
/// why. Without it a short baseline is indistinguishable from an evicted
/// cache, and an operator has to go read a JSON file to tell the two apart.
fn render_accepted_level_shifts(rows: &[DurationTrendRow]) -> String {
    let mut reasons = BTreeMap::<&str, (&str, usize)>::new();
    for reset in rows.iter().filter_map(|row| row.baseline_reset.as_ref()) {
        let entry = reasons
            .entry(reset.commit.as_str())
            .or_insert((reset.reason.as_str(), 0));
        entry.1 += 1;
    }
    if reasons.is_empty() {
        return String::new();
    }
    let mut out = String::from("  accepted level shifts in effect:\n");
    for (commit, (reason, series)) in reasons {
        out.push_str(&format!(
            "    {commit}: {reason} ({series} series measured against a window that starts there)\n"
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
#[path = "duration_trend_tests.rs"]
mod tests;
