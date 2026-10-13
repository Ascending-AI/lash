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
    /// Fraction of the baseline per-turn median.
    pub relative: f64,
    /// Absolute slack per metric, for medians near zero.
    #[serde(default)]
    pub absolute: BTreeMap<String, f64>,
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
    /// The per-session-turn maximum.
    PerTurnMax,
    /// The robust (Theil-Sen) slope against history depth.
    HistorySlope,
    /// The robust (Theil-Sen) slope against the frame's depth: the depth
    /// slope of a compacting session.
    FrameSlope,
}

impl Check {
    fn of(self, summary: &MetricSummary) -> f64 {
        match self {
            Self::PerTurnMedian => summary.per_turn_median,
            Self::PerTurnMax => summary.per_turn_max,
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
    /// Robust slopes against frame depth (history depth when the workload
    /// never compacts).
    pub depth_slope: BTreeMap<String, f64>,
}

impl Baseline {
    pub fn committed() -> Result<Self> {
        let baseline: Self = serde_json::from_str(BASELINE).context("parse baseline.json")?;
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
        Ok(Self {
            format: BASELINE_FORMAT.to_owned(),
            blessed_from: receipt.identity.commit.clone(),
            build: receipt.identity.build.clone(),
            build_profile: receipt.identity.build_profile.clone(),
            allocator: receipt.identity.allocator.clone(),
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
                            per_turn_median: counted(workload, |summary| summary.per_turn_median),
                            depth_slope: counted(workload, |summary| {
                                summary.robust_slope_per_frame_turn
                            }),
                        },
                    )
                })
                .collect(),
        })
    }
}

fn counted(
    workload: &WorkloadReceipt,
    statistic: impl Fn(&MetricSummary) -> f64,
) -> BTreeMap<String, f64> {
    workload
        .summary
        .iter()
        .filter(|(_, summary)| summary.kind == MetricKind::Counted)
        .map(|(name, summary)| (name.clone(), statistic(summary)))
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
        let observed = budget.check.of(summary);
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
    for (name, base) in &blessed.per_turn_median {
        let Some(summary) = workload.summary.get(name) else {
            verdict.violations.push(format!(
                "{}: `{name}` has a baseline but was not measured",
                workload.workload
            ));
            continue;
        };
        let slack = budgets
            .baseline_tolerance
            .absolute
            .get(name)
            .copied()
            .unwrap_or_default();
        let ceiling = base * (1.0 + budgets.baseline_tolerance.relative) + slack;
        let observed = summary.per_turn_median;
        if observed > ceiling {
            verdict.violations.push(format!(
                "{}: per-turn median `{name}` is {observed} {}, above its baseline {base} \
                 plus tolerance ({ceiling}); fix the regression, or re-bless with `{}` if it is \
                 intended",
                workload.workload, summary.unit, budgets.rebless
            ));
        } else if observed < base * (1.0 - budgets.baseline_tolerance.relative) - slack {
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
        let observed = summary.robust_slope_per_frame_turn;
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
                metrics: BTreeMap::from([(
                    "store_bytes_written".to_owned(),
                    40_000.0 + bytes_per_prior_turn * depth as f64,
                )]),
            })
            .collect();
        ReplayReceipt {
            format: RECEIPT_FORMAT.to_owned(),
            identity: Identity::current(),
            workloads: vec![WorkloadReceipt {
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
                relative: 0.25,
                absolute: BTreeMap::new(),
                depth_slope: BTreeMap::from([(
                    "store_bytes_written".to_owned(),
                    SlopeTolerance {
                        relative: 0.05,
                        absolute: 64.0,
                    },
                )]),
            },
            budgets: vec![Budget {
                workload: "*".to_owned(),
                metric: "store_bytes_written".to_owned(),
                check: Check::HistorySlope,
                max: 64.0,
                why: "a turn writes what it adds, not its history".to_owned(),
            }],
            known_breaches: known
                .then(|| KnownBreach {
                    workload: "chat-short".to_owned(),
                    metric: "store_bytes_written".to_owned(),
                    check: Check::HistorySlope,
                    owner: "FIG-5931".to_owned(),
                    why: "terminal window copy".to_owned(),
                })
                .into_iter()
                .collect(),
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
        };
        let baseline = Baseline::bless(&candidate).unwrap();
        assert!(evaluate(&candidate, &budgets(false), &baseline).passed());
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
            unknown.violations[0].contains("HistorySlope of `store_bytes_written` is 774"),
            "{unknown:?}"
        );
        let twice = evaluate(&receipt(2.0 * 774.0), &budgets(true), &baseline);
        assert!(
            twice
                .violations
                .iter()
                .any(|violation| violation.contains("depth slope of `store_bytes_written` rose")),
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
