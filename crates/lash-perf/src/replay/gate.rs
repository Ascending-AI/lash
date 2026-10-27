//! The per-merge counted gate: committed per-turn ceilings, depth-slope
//! ceilings and a tolerance against a committed baseline receipt. It binds
//! counted metrics only; wall and CPU time never decide a merge.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::meter::{MetricKind, metric};
use super::receipt::{MetricSummary, ReplayReceipt, WorkloadReceipt};

/// The budgets format this module reads.
pub const BUDGETS_FORMAT: &str = "lash-perf-replay-budgets/1";
/// The baseline format the bless command writes.
pub const BASELINE_FORMAT: &str = "lash-perf-replay-baseline/1";

/// The committed budgets.
pub const BUDGETS: &str = include_str!("../../workloads/replay/budgets.json");
/// The committed baseline the tolerance compares against.
pub const BASELINE: &str = include_str!("../../workloads/replay/baseline.json");

/// The gate's committed policy.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub format: String,
    /// The command that re-blesses `baseline.json`.
    pub rebless: String,
    /// The workloads the gate runs, each at its gate length.
    pub subset: Vec<GateWorkload>,
    /// The tolerance a counted per-turn median has above its baseline.
    pub baseline_tolerance: Tolerance,
    pub budgets: Vec<Budget>,
    /// Budgets main breaches today, each owned by the ticket that fixes it.
    /// A known breach reports instead of failing; one that no longer
    /// breaches fails until its entry is removed.
    #[serde(default)]
    pub known_breaches: Vec<KnownBreach>,
}

/// One workload of the gate's subset.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GateWorkload {
    pub workload: String,
    /// Run only the first this many turns; `None` runs the whole script.
    #[serde(default)]
    pub turns: Option<usize>,
}

/// How far a counted metric may move above its blessed baseline.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tolerance {
    /// The metrics whose depth slope is ratcheted against the baseline, and
    /// how far each may rise: a known breach still fails when it grows. A
    /// `<workload>/<metric>` key overrides the metric's tolerance for one
    /// noisier workload.
    #[serde(default)]
    pub depth_slope: BTreeMap<String, SlopeTolerance>,
}

/// How far a depth slope may rise above its blessed value:
/// `relative * |baseline| + absolute`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlopeTolerance {
    pub relative: f64,
    pub absolute: f64,
}

/// The statistic a budget binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// The per-session-turn median.
    PerTurnMedian,
    /// The maximum round total divided by sessions, never a session tail.
    RoundAverageMax,
    /// The robust (Theil-Sen) slope against history depth.
    HistorySlope,
    /// The robust (Theil-Sen) slope against the frame's depth: the depth
    /// slope of a compacting session.
    FrameSlope,
}

impl Check {
    fn of(self, summary: &MetricSummary) -> Option<f64> {
        match self {
            Self::PerTurnMedian => Some(summary.per_turn_median),
            Self::RoundAverageMax => Some(summary.round_average_max),
            Self::HistorySlope => summary.robust_slope_per_history_turn,
            Self::FrameSlope => summary.robust_slope_per_frame_turn,
        }
    }
}

/// One committed ceiling.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// A workload name, or `*` for every gate workload.
    pub workload: String,
    pub metric: String,
    pub check: Check,
    pub max: f64,
    /// Why this ceiling, in one line.
    pub why: String,
}

/// A budget main breaches, with its owner.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KnownBreach {
    pub workload: String,
    pub metric: String,
    pub check: Check,
    pub owner: String,
    pub why: String,
}

impl Budgets {
    pub fn committed() -> Result<Self> {
        Self::parse(BUDGETS)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let budgets: Self = serde_json::from_str(text).context("parse budgets.json")?;
        ensure!(
            budgets.format == BUDGETS_FORMAT,
            "budgets.json is format `{}`, not `{BUDGETS_FORMAT}`",
            budgets.format
        );
        for budget in &budgets.budgets {
            let def = metric(&budget.metric)
                .with_context(|| format!("a budget names unknown metric `{}`", budget.metric))?;
            ensure!(
                def.kind == MetricKind::Counted,
                "`{}` is timed: the per-merge gate binds counted metrics only",
                budget.metric
            );
        }
        Ok(budgets)
    }
}

/// The committed baseline: per-turn medians of every counted metric.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub format: String,
    pub blessed_from: Option<String>,
    pub build: String,
    pub build_profile: String,
    pub allocator: String,
    pub worker_resources: bool,
    pub workloads: BTreeMap<String, BaselineWorkload>,
}

/// One workload's blessed medians and depth slopes, keyed by the script it
/// ran.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineWorkload {
    pub script_digest: String,
    pub backend: String,
    pub turns: usize,
    pub per_turn_median: BTreeMap<String, f64>,
    /// Same-commit maximum minus minimum over five complete runs.
    pub median_noise: BTreeMap<String, f64>,
    /// Observed maximum plus absolute noise, with zero slack for exact counts.
    pub median_ceiling: BTreeMap<String, f64>,
    /// Robust slopes against frame depth (history depth when the workload
    /// never compacts).
    pub depth_slope: BTreeMap<String, f64>,
}

impl Baseline {
    pub fn committed() -> Result<Self> {
        Self::parse(BASELINE)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let baseline: Self = serde_json::from_str(text).context("parse baseline.json")?;
        ensure!(
            baseline.format == BASELINE_FORMAT,
            "baseline.json is format `{}`, not `{BASELINE_FORMAT}`",
            baseline.format
        );
        Ok(baseline)
    }

    /// The baseline `receipt` blesses.
    pub fn bless(receipt: &ReplayReceipt) -> Result<Self> {
        ensure!(
            missing_identity(&receipt.identity).is_none(),
            "cannot bless a receipt with missing counted identity"
        );
        ensure!(
            !receipt.identity.instructions
                && !receipt.identity.sql_shapes
                && !receipt.identity.turn_profile
                && !receipt.identity.traffic
                && receipt.identity.heap_window.is_none(),
            "counted baselines require ordinary replay observation"
        );
        Ok(Self {
            format: BASELINE_FORMAT.to_owned(),
            blessed_from: receipt.identity.commit.clone(),
            build: receipt.identity.build.clone(),
            build_profile: receipt.identity.build_profile.clone(),
            allocator: receipt.identity.allocator.clone(),
            worker_resources: receipt.identity.worker_resources,
            workloads: receipt
                .workloads
                .iter()
                .map(|workload| {
                    (
                        workload.workload.clone(),
                        BaselineWorkload {
                            script_digest: workload.script_digest.clone(),
                            backend: workload.backend.clone(),
                            turns: workload.turns,
                            per_turn_median: counted(workload, |summary| {
                                Some(summary.per_turn_median)
                            }),
                            median_noise: counted(workload, |_| Some(0.0)),
                            median_ceiling: counted(workload, |summary| {
                                Some(summary.per_turn_median)
                            }),
                            depth_slope: counted(workload, |summary| {
                                summary.robust_slope_per_frame_turn
                            }),
                        },
                    )
                })
                .collect(),
        })
    }

    /// Calibrate five complete ordinary receipts, refusing identity or population drift.
    pub fn calibrate(receipts: &[ReplayReceipt]) -> Result<Self> {
        ensure!(
            receipts.len() == 5,
            "count ceilings need five same-commit runs"
        );
        let mut baseline = Self::bless(&receipts[0])?;
        for receipt in &receipts[1..] {
            ensure!(
                receipt.identity.commit == receipts[0].identity.commit
                    && counted_identity_refusal(&receipt.identity, &baseline).is_none(),
                "calibration identity drift"
            );
            let sample = Self::bless(receipt)?;
            ensure!(
                sample.workloads.keys().eq(baseline.workloads.keys()),
                "calibration workload drift"
            );
            for (name, row) in &sample.workloads {
                let first = &baseline.workloads[name];
                ensure!(
                    row.script_digest == first.script_digest
                        && row.turns == first.turns
                        && row.backend == first.backend
                        && row.per_turn_median.keys().eq(first.per_turn_median.keys()),
                    "calibration population drift for {name}"
                );
            }
        }
        for (name, row) in &mut baseline.workloads {
            for (metric, maximum) in &mut row.per_turn_median {
                let values: Vec<_> = receipts
                    .iter()
                    .map(|r| {
                        r.workloads
                            .iter()
                            .find(|w| w.workload == *name)
                            .and_then(|w| w.summary.get(metric))
                            .map(|s| s.per_turn_median)
                            .context("missing calibration metric")
                    })
                    .collect::<Result<_>>()?;
                ensure!(
                    values.iter().all(|v| v.is_finite() && *v >= 0.0),
                    "invalid counted value"
                );
                *maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
                row.median_noise.insert(metric.clone(), *maximum - minimum);
                row.median_ceiling
                    .insert(metric.clone(), *maximum + (*maximum - minimum));
            }
        }
        Ok(baseline)
    }
}

/// The candidate is a fixed population of five independent process runs.
/// Median counted work rejects systematic amplification without treating a
/// sporadic background allocation as foreground work. Maxima and depth slopes
/// keep the worst observation, preserving the independent existing budgets.
pub fn candidate_population(receipts: &[ReplayReceipt]) -> Result<ReplayReceipt> {
    let _ = Baseline::calibrate(receipts)?;
    let mut combined = receipts[0].clone();
    for row in &mut combined.workloads {
        row.round_statistic = "gate: median of five independent per-turn medians; maxima and slopes are worst-of-five; rounds below are the first raw population; sibling .0.json through .4.json retain every raw population".to_owned();
        for (name, summary) in &mut row.summary {
            let values: Vec<_> = receipts
                .iter()
                .map(|r| {
                    r.workloads
                        .iter()
                        .find(|w| w.workload == row.workload)
                        .and_then(|w| w.summary.get(name))
                        .context("missing gate population metric")
                })
                .collect::<Result<_>>()?;
            let mut medians: Vec<_> = values.iter().map(|s| s.per_turn_median).collect();
            medians.sort_by(f64::total_cmp);
            summary.per_turn_median = medians[2];
            summary.round_average_max = values
                .iter()
                .map(|s| s.round_average_max)
                .fold(f64::NEG_INFINITY, f64::max);
            for (target, get) in [
                (
                    &mut summary.robust_slope_per_frame_turn,
                    (|s: &MetricSummary| s.robust_slope_per_frame_turn)
                        as fn(&MetricSummary) -> Option<f64>,
                ),
                (
                    &mut summary.robust_slope_per_history_turn,
                    (|s: &MetricSummary| s.robust_slope_per_history_turn)
                        as fn(&MetricSummary) -> Option<f64>,
                ),
            ] {
                *target = values
                    .iter()
                    .map(|s| get(s))
                    .collect::<Option<Vec<_>>>()
                    .map(|v| v.into_iter().fold(f64::NEG_INFINITY, f64::max));
            }
        }
    }
    Ok(combined)
}

fn counted(
    workload: &WorkloadReceipt,
    statistic: impl Fn(&MetricSummary) -> Option<f64>,
) -> BTreeMap<String, f64> {
    workload
        .summary
        .iter()
        .filter(|(_, summary)| summary.kind == MetricKind::Counted)
        .filter_map(|(name, summary)| statistic(summary).map(|value| (name.clone(), value)))
        .collect()
}

/// The gate's finding on one receipt.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Verdict {
    /// SQL and allocation comparisons share one cohort; neither is certified after an identity refusal.
    pub identity_refusal: Option<CountedIdentityRefusal>,
    /// Breaches that fail the gate.
    pub violations: Vec<String>,
    /// Committed known breaches that still breach.
    pub known_breaches: Vec<String>,
    /// Medians or slopes below their baseline: re-bless to ratchet.
    pub improvements: Vec<String>,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

fn applies(pattern: &str, workload: &str) -> bool {
    pattern == "*" || pattern == workload
}

/// The identity dimension required by the counted baseline.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CountedIdentityDimension {
    Build,
    BuildProfile,
    Allocator,
    Observer,
}

/// Why counted metrics cannot be compared or blessed.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum CountedIdentityRefusal {
    Missing {
        dimension: CountedIdentityDimension,
    },
    Mismatch {
        dimension: CountedIdentityDimension,
        baseline: String,
        candidate: String,
    },
}

fn missing_identity(identity: &super::receipt::Identity) -> Option<CountedIdentityDimension> {
    [
        (CountedIdentityDimension::Build, &identity.build),
        (
            CountedIdentityDimension::BuildProfile,
            &identity.build_profile,
        ),
        (CountedIdentityDimension::Allocator, &identity.allocator),
    ]
    .into_iter()
    .find_map(|(dimension, value)| value.trim().is_empty().then_some(dimension))
}

fn counted_identity_refusal(
    identity: &super::receipt::Identity,
    baseline: &Baseline,
) -> Option<CountedIdentityRefusal> {
    if identity.instructions
        || identity.worker_resources != baseline.worker_resources
        || identity.turn_profile
        || identity.sql_shapes
        || identity.traffic
        || identity.heap_window.is_some()
    {
        return Some(CountedIdentityRefusal::Mismatch {
            dimension: CountedIdentityDimension::Observer,
            baseline: format!(
                "ordinary replay; worker_resources={}",
                baseline.worker_resources
            ),
            candidate: format!(
                "sql_shapes={};turn_profile={};traffic={};heap_window={:?};worker_resources={};instructions={}",
                identity.sql_shapes,
                identity.turn_profile,
                identity.traffic,
                identity.heap_window,
                identity.worker_resources,
                identity.instructions
            ),
        });
    }
    for (dimension, candidate, blessed) in [
        (
            CountedIdentityDimension::Build,
            &identity.build,
            &baseline.build,
        ),
        (
            CountedIdentityDimension::BuildProfile,
            &identity.build_profile,
            &baseline.build_profile,
        ),
        (
            CountedIdentityDimension::Allocator,
            &identity.allocator,
            &baseline.allocator,
        ),
    ] {
        if candidate.trim().is_empty() || blessed.trim().is_empty() {
            return Some(CountedIdentityRefusal::Missing { dimension });
        }
        if candidate != blessed {
            return Some(CountedIdentityRefusal::Mismatch {
                dimension,
                baseline: blessed.clone(),
                candidate: candidate.clone(),
            });
        }
    }
    None
}

/// Check `receipt` against `budgets` and `baseline`.
pub fn evaluate(receipt: &ReplayReceipt, budgets: &Budgets, baseline: &Baseline) -> Verdict {
    let mut verdict = Verdict::default();
    if let Some(refusal) = counted_identity_refusal(&receipt.identity, baseline) {
        verdict
            .violations
            .push(format!("counted baseline identity refused: {refusal:?}"));
        verdict.identity_refusal = Some(refusal);
        return verdict;
    }
    let mut breached_known = vec![false; budgets.known_breaches.len()];
    for gate in &budgets.subset {
        let Some(workload) = receipt
            .workloads
            .iter()
            .find(|workload| workload.workload == gate.workload)
        else {
            verdict
                .violations
                .push(format!("gate workload `{}` did not run", gate.workload));
            continue;
        };
        check_budgets(workload, budgets, &mut breached_known, &mut verdict);
        check_baseline(workload, budgets, baseline, &mut verdict);
    }
    for (known, breached) in budgets.known_breaches.iter().zip(breached_known) {
        let ran = budgets
            .subset
            .iter()
            .any(|gate| applies(&known.workload, &gate.workload));
        if ran && !breached {
            verdict.violations.push(format!(
                "known breach {} {:?} of `{}` (owner {}) no longer breaches: remove it from \
                 budgets.json so the budget binds",
                known.workload, known.check, known.metric, known.owner
            ));
        }
    }
    verdict
}

fn check_budgets(
    workload: &WorkloadReceipt,
    budgets: &Budgets,
    breached_known: &mut [bool],
    verdict: &mut Verdict,
) {
    for budget in budgets
        .budgets
        .iter()
        .filter(|budget| applies(&budget.workload, &workload.workload))
    {
        let Some(summary) = workload.summary.get(&budget.metric) else {
            verdict.violations.push(format!(
                "{}: `{}` was not measured; the budget cannot be checked",
                workload.workload, budget.metric
            ));
            continue;
        };
        let Some(observed) = budget.check.of(summary) else {
            verdict.violations.push(format!(
                "{}: {:?} of `{}` is unknown; insufficient distinct depths",
                workload.workload, budget.check, budget.metric
            ));
            continue;
        };
        if observed <= budget.max {
            continue;
        }
        let finding = format!(
            "{}: {:?} of `{}` is {observed} {}, over its ceiling {} ({})",
            workload.workload, budget.check, budget.metric, summary.unit, budget.max, budget.why
        );
        let known = budgets.known_breaches.iter().position(|known| {
            applies(&known.workload, &workload.workload)
                && known.metric == budget.metric
                && known.check == budget.check
        });
        match known {
            Some(index) => {
                breached_known[index] = true;
                let owner = &budgets.known_breaches[index];
                verdict.known_breaches.push(format!(
                    "{finding}; known breach owned by {}: {}",
                    owner.owner, owner.why
                ));
            }
            None => verdict.violations.push(finding),
        }
    }
}

fn check_baseline(
    workload: &WorkloadReceipt,
    budgets: &Budgets,
    baseline: &Baseline,
    verdict: &mut Verdict,
) {
    let Some(blessed) = baseline.workloads.get(&workload.workload) else {
        verdict.violations.push(format!(
            "{}: no blessed baseline; run `{}`",
            workload.workload, budgets.rebless
        ));
        return;
    };
    if blessed.script_digest != workload.script_digest
        || blessed.backend != workload.backend
        || blessed.turns != workload.turns
    {
        verdict.violations.push(format!(
            "{}: the baseline was blessed for another script, backend or length; run `{}`",
            workload.workload, budgets.rebless
        ));
        return;
    }
    let measured = counted(workload, |s| Some(s.per_turn_median));
    if !measured.keys().eq(blessed.per_turn_median.keys())
        || !blessed
            .median_ceiling
            .keys()
            .eq(blessed.per_turn_median.keys())
        || !blessed
            .median_noise
            .keys()
            .eq(blessed.per_turn_median.keys())
    {
        verdict.violations.push(format!(
            "{}: counted metric population differs from its baseline",
            workload.workload
        ));
        return;
    }
    for (name, base) in &blessed.per_turn_median {
        let Some(summary) = workload.summary.get(name) else {
            verdict.violations.push(format!(
                "{}: `{name}` has a baseline but was not measured",
                workload.workload
            ));
            continue;
        };
        let Some(&ceiling) = blessed.median_ceiling.get(name) else {
            verdict
                .violations
                .push(format!("{}: missing ceiling for {name}", workload.workload));
            continue;
        };
        let slack = blessed.median_noise.get(name).copied().unwrap_or_default();
        if ![ceiling, slack, *base, summary.per_turn_median]
            .into_iter()
            .all(|v| v.is_finite() && v >= 0.0)
            || ceiling < *base
        {
            verdict.violations.push(format!(
                "{}: invalid counted ceiling or observation for {name}",
                workload.workload
            ));
            continue;
        }
        let observed = summary.per_turn_median;
        if observed > ceiling {
            verdict.violations.push(format!(
                "{}: per-turn median `{name}` is {observed} {}, above its baseline {base} \
                 plus tolerance ({ceiling}); fix the regression, or re-bless with `{}` if it is \
                 intended",
                workload.workload, summary.unit, budgets.rebless
            ));
        } else if observed < base - slack {
            verdict.improvements.push(format!(
                "{}: per-turn median `{name}` fell to {observed} from {base}; re-bless to ratchet",
                workload.workload
            ));
        }
    }
    for (name, tolerance) in &budgets.baseline_tolerance.depth_slope {
        if name.contains('/') {
            continue;
        }
        let tolerance = budgets
            .baseline_tolerance
            .depth_slope
            .get(&format!("{}/{name}", workload.workload))
            .unwrap_or(tolerance);
        let (Some(base), Some(summary)) =
            (blessed.depth_slope.get(name), workload.summary.get(name))
        else {
            verdict.violations.push(format!(
                "{}: the depth slope of `{name}` has no baseline or was not measured",
                workload.workload
            ));
            continue;
        };
        let slack = tolerance.relative * base.abs() + tolerance.absolute;
        let Some(observed) = summary.robust_slope_per_frame_turn else {
            verdict.violations.push(format!(
                "{}: the depth slope of `{name}` is unknown; insufficient distinct depths",
                workload.workload
            ));
            continue;
        };
        if observed > base + slack {
            verdict.violations.push(format!(
                "{}: the depth slope of `{name}` rose to {observed} {} per prior turn from its \
                 baseline {base} (tolerance {slack}): per-turn work grows faster with history; \
                 fix the regression, or re-bless with `{}` if it is intended",
                workload.workload, summary.unit, budgets.rebless
            ));
        } else if observed < base - slack {
            verdict.improvements.push(format!(
                "{}: the depth slope of `{name}` fell to {observed} from {base}; re-bless to \
                 ratchet",
                workload.workload
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::receipt::{Identity, RECEIPT_FORMAT, RoundRecord, summarize};
    use super::*;

    fn receipt(bytes_per_prior_turn: f64) -> ReplayReceipt {
        let rounds: Vec<RoundRecord> = (0..20)
            .map(|depth| RoundRecord {
                turn: depth + 1,
                history_depth: depth,
                frame_depth: depth,
                sessions: 1,
                setup: false,
                traffic: None,
                session_wall_ms: Vec::new(),
                metrics: BTreeMap::from([(
                    "sql_bytes_written".to_owned(),
                    40_000.0 + bytes_per_prior_turn * depth as f64,
                )]),
            })
            .collect();
        ReplayReceipt {
            format: RECEIPT_FORMAT.to_owned(),
            identity: Identity::current(),
            workloads: vec![WorkloadReceipt {
                experiment: Identity::current().experiment(serde_json::json!({"fixture": true})),
                workload: "chat-short".to_owned(),
                script_digest: "digest".to_owned(),
                protocol: "standard".to_owned(),
                backend: "sqlite-file".to_owned(),
                sessions: 1,
                turns: 20,
                compact_every: None,
                units: BTreeMap::new(),
                round_statistic: String::new(),
                summary: summarize(&rounds),
                rounds,
                compactions: Vec::new(),
            }],
        }
    }

    fn budgets(known: bool) -> Budgets {
        Budgets {
            format: BUDGETS_FORMAT.to_owned(),
            rebless: "just perf-replay-bless".to_owned(),
            subset: vec![GateWorkload {
                workload: "chat-short".to_owned(),
                turns: None,
            }],
            baseline_tolerance: Tolerance {
                depth_slope: BTreeMap::from([(
                    "sql_bytes_written".to_owned(),
                    SlopeTolerance {
                        relative: 0.05,
                        absolute: 64.0,
                    },
                )]),
            },
            budgets: vec![Budget {
                workload: "*".to_owned(),
                metric: "sql_bytes_written".to_owned(),
                check: Check::HistorySlope,
                max: 64.0,
                why: "a turn writes what it adds, not its history".to_owned(),
            }],
            known_breaches: known
                .then(|| KnownBreach {
                    workload: "chat-short".to_owned(),
                    metric: "sql_bytes_written".to_owned(),
                    check: Check::HistorySlope,
                    owner: "FIG-5931".to_owned(),
                    why: "terminal window copy".to_owned(),
                })
                .into_iter()
                .collect(),
        }
    }

    /// FIG-6620: an unknown slope cannot satisfy a counted ceiling or baseline.
    #[test]
    fn insufficient_depth_cannot_pass_a_slope_budget() {
        let mut candidate = receipt(0.0);
        let baseline = Baseline::bless(&candidate).unwrap();
        let workload = &mut candidate.workloads[0];
        workload.rounds.truncate(1);
        workload.summary = summarize(&workload.rounds);
        let verdict = evaluate(&candidate, &budgets(false), &baseline);
        assert!(!verdict.passed());
        assert!(
            verdict
                .violations
                .iter()
                .any(|finding| finding.contains("unknown"))
        );
    }

    /// FIG-6620: optional observers cannot share an ordinary counted baseline.
    #[test]
    fn counted_baselines_refuse_mixed_observer_modes() {
        let candidate = receipt(0.0);
        let baseline = Baseline::bless(&candidate).unwrap();
        for mode in 0..6 {
            let mut observed = candidate.clone();
            observed.identity.sql_shapes = mode == 0;
            observed.identity.turn_profile = mode == 5;
            observed.identity.traffic = mode == 1;
            observed.identity.heap_window = (mode == 2).then_some([1, 2]);
            observed.identity.instructions = mode == 3;
            observed.identity.worker_resources = mode == 4;
            assert_eq!(Baseline::bless(&observed).is_err(), mode != 4);
            assert!(matches!(
                evaluate(&observed, &budgets(false), &baseline).identity_refusal,
                Some(CountedIdentityRefusal::Mismatch {
                    dimension: CountedIdentityDimension::Observer,
                    ..
                })
            ));
        }
    }

    /// REPLAY-IDENTITY: counted medians belong to their build and allocator,
    /// including SQL-only populations; a debug baseline cannot certify release.
    #[test]
    fn counted_baselines_refuse_another_build_or_allocator() {
        let mut candidate = receipt(0.0);
        candidate.identity = Identity {
            commit: None,
            dirty: false,
            build: "debug".to_owned(),
            build_profile: "counted-test;debug_assertions=true".to_owned(),
            allocator: "stats_alloc".to_owned(),
            host: "test".to_owned(),
            cpus: 1,
            load1_at_start: None,
            started_at: "2026-10-13T00:00:00Z".to_owned(),
            ..Identity::current()
        };
        let baseline = Baseline::bless(&candidate).unwrap();
        assert!(evaluate(&candidate, &budgets(false), &baseline).passed());
        let mut population = vec![candidate.clone(); 5];
        for (index, receipt) in population.iter_mut().enumerate() {
            receipt.workloads[0]
                .summary
                .get_mut("sql_bytes_written")
                .unwrap()
                .per_turn_median += if index == 0 { 1000.0 } else { 1.0 };
        }
        assert!(
            !evaluate(
                &candidate_population(&population).unwrap(),
                &budgets(false),
                &baseline
            )
            .passed(),
            "one systematic counted unit passed the five-population gate"
        );
        let mut extra = candidate.clone();
        extra.workloads[0]
            .summary
            .get_mut("sql_bytes_written")
            .unwrap()
            .per_turn_median += 1.0;
        assert!(
            !evaluate(&extra, &budgets(false), &baseline).passed(),
            "one extra exact counted unit passed"
        );
        candidate.identity.build = "release".to_owned();
        let verdict = evaluate(&candidate, &budgets(false), &baseline);
        println!(
            "replay-identity-refusals.json={}",
            serde_json::to_string(&verdict).unwrap()
        );
        assert!(
            !verdict.passed(),
            "debug baseline certified release: {verdict:?}"
        );
        candidate.identity.build = "debug".to_owned();
        candidate.identity.allocator = "dhat-heap+stats_alloc".to_owned();
        assert!(matches!(
            evaluate(&candidate, &budgets(false), &baseline).identity_refusal,
            Some(CountedIdentityRefusal::Mismatch {
                dimension: CountedIdentityDimension::Allocator,
                ..
            })
        ));
        candidate.identity.allocator = baseline.allocator.clone();
        candidate
            .identity
            .build_profile
            .push_str(";another-optimization-profile");
        assert!(matches!(
            evaluate(&candidate, &budgets(false), &baseline).identity_refusal,
            Some(CountedIdentityRefusal::Mismatch {
                dimension: CountedIdentityDimension::BuildProfile,
                ..
            })
        ));
        candidate.identity.build_profile = baseline.build_profile.clone();
        for dimension in [
            CountedIdentityDimension::Build,
            CountedIdentityDimension::BuildProfile,
            CountedIdentityDimension::Allocator,
        ] {
            let mut missing = candidate.clone();
            match dimension {
                CountedIdentityDimension::Build => missing.identity.build.clear(),
                CountedIdentityDimension::BuildProfile => missing.identity.build_profile.clear(),
                CountedIdentityDimension::Allocator => missing.identity.allocator.clear(),
                CountedIdentityDimension::Observer => unreachable!("observer uses typed flags"),
            }
            assert!(!evaluate(&missing, &budgets(false), &baseline).passed());
            assert!(Baseline::bless(&missing).is_err());
        }
        let mut absent = serde_json::to_value(&baseline).unwrap();
        absent.as_object_mut().unwrap().remove("allocator");
        assert!(serde_json::from_value::<Baseline>(absent).is_err());
    }

    /// REPLAY-GATE: bytes written that grow with history depth (the
    /// FIG-5501 terminal-window copy's shape) fail the slope budget, a
    /// committed known breach reports instead of failing, and a known breach
    /// that no longer breaches fails until its entry is removed. A known
    /// breach is ratcheted: a second copy on top of it still fails.
    #[test]
    fn a_depth_slope_fails_unless_it_is_a_known_breach_that_still_breaches() {
        let flat = receipt(0.0);
        let copying = receipt(774.0);
        let baseline = Baseline::bless(&copying).unwrap();
        let unknown = evaluate(&copying, &budgets(false), &baseline);
        assert!(!unknown.passed());
        assert!(
            unknown.violations[0].contains("HistorySlope of `sql_bytes_written` is 774"),
            "{unknown:?}"
        );
        let twice = evaluate(&receipt(2.0 * 774.0), &budgets(true), &baseline);
        assert!(
            twice
                .violations
                .iter()
                .any(|violation| violation.contains("depth slope of `sql_bytes_written` rose")),
            "{twice:?}"
        );
        let known = evaluate(&copying, &budgets(true), &baseline);
        assert!(
            known
                .violations
                .iter()
                .all(|violation| !violation.contains("HistorySlope")),
            "{known:?}"
        );
        assert_eq!(known.known_breaches.len(), 1);
        assert!(known.passed(), "{known:?}");
        let fixed = evaluate(&flat, &budgets(true), &baseline);
        assert!(!fixed.passed());
        assert!(
            fixed.violations[0].contains("no longer breaches"),
            "{fixed:?}"
        );
        assert!(evaluate(&flat, &budgets(false), &Baseline::bless(&flat).unwrap()).passed());
    }
}
