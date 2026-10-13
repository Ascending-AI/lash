use super::*;

/// The verdict the gate exits on.
#[derive(Debug, Serialize)]
pub(crate) struct GateVerdict {
    pub(crate) gate: &'static str,
    pub(crate) budget: serde_json::Value,
    pub(crate) pass: bool,
    pub(crate) violations: Vec<GateRefusal>,
}

/// The run report: config, environment, every case and the verdict.
#[derive(Debug, Serialize)]
pub(crate) struct LatencyReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) samples_file: Option<std::path::PathBuf>,
    pub(crate) kind: &'static str,
    pub(crate) load_model: crate::offered_load::LoadModel,
    pub(crate) schema: u32,
    pub(crate) crate_version: &'static str,
    pub(crate) environment: serde_json::Value,
    pub(crate) cases: Vec<CaseReport>,
    pub(crate) verdict: GateVerdict,
}

/// Assemble the run report and evaluate the gate.
pub(crate) fn build_report(
    environment: serde_json::Value,
    cases: Vec<CaseReport>,
) -> LatencyReport {
    let mut violations = Vec::new();
    let gate = cases
        .iter()
        .find(|case| case.name == super::super::GATE_CASE);
    match gate {
        None => violations.push(GateRefusal::MissingCase),
        Some(case) => {
            violations.extend(case.population_refusals.iter().cloned());
            if case.samples < super::super::GATE_MIN_SAMPLES {
                violations.push(GateRefusal::InsufficientSamples {
                    samples: case.samples,
                });
            }
            if case.lanes != 16 || case.actual_lanes != 16 {
                violations.push(GateRefusal::LaneCount {
                    requested: case.lanes,
                    actual: case.actual_lanes,
                });
            }
            if case.topology != Topology::SameProcess
                || case.provider != LatencyProviderKind::Text
                || case.follower != FollowerMode::Standard
                || case.busy
            {
                violations.push(GateRefusal::WrongFixture);
            }
            if !case.errors.is_empty() {
                violations.push(GateRefusal::Errors {
                    count: case.errors.len(),
                });
            }
            match &case.phases_ms.overhead {
                None => violations.push(GateRefusal::OverheadPopulation {
                    samples: 0,
                    expected: case.samples,
                }),
                Some(overhead) => {
                    if overhead.n != case.samples {
                        violations.push(GateRefusal::OverheadPopulation {
                            samples: overhead.n,
                            expected: case.samples,
                        });
                    }
                    for (percentile, observed, budget) in [
                        ("p50", overhead.p50, super::super::GATE_OVERHEAD_P50_MS),
                        ("p99", overhead.p99, super::super::GATE_OVERHEAD_P99_MS),
                    ] {
                        if !observed.is_finite() || observed >= budget {
                            violations.push(GateRefusal::OverBudget {
                                percentile,
                                observed,
                                budget,
                            });
                        }
                    }
                }
            }
        }
    }
    LatencyReport {
        samples_file: None,
        kind: "lash.send-latency",
        load_model: crate::offered_load::LoadModel::ServiceDiagnostic,
        schema: 1,
        crate_version: env!("CARGO_PKG_VERSION"),
        environment,
        cases,
        verdict: GateVerdict {
            gate: "latency-gate",
            budget: serde_json::json!({
                "case": super::super::GATE_CASE,
                "overhead_p50_ms_lt": super::super::GATE_OVERHEAD_P50_MS,
                "overhead_p99_ms_lt": super::super::GATE_OVERHEAD_P99_MS,
                "min_samples": super::super::GATE_MIN_SAMPLES,
            }),
            pass: violations.is_empty(),
            violations,
        },
    }
}

/// Refusals retain their axis and population instead of certifying filtered rows.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub(crate) enum GateRefusal {
    MissingCase,
    InsufficientSamples {
        samples: usize,
    },
    LaneCount {
        requested: usize,
        actual: usize,
    },
    WrongFixture,
    Errors {
        count: usize,
    },
    NonAnswered {
        rows: usize,
    },
    MissingClock {
        clock: &'static str,
        rows: usize,
    },
    InvalidClocks {
        rows: usize,
    },
    InvalidPhase {
        phase: &'static str,
        rows: usize,
    },
    OverheadPopulation {
        samples: usize,
        expected: usize,
    },
    OverBudget {
        percentile: &'static str,
        observed: f64,
        budget: f64,
    },
}

impl std::fmt::Display for GateRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCase => write!(f, "gated case `fast` did not run"),
            Self::InsufficientSamples { samples } => {
                write!(f, "gated case `fast` ran {samples} samples (< 10000)")
            }
            Self::LaneCount { requested, actual } => write!(
                f,
                "fast requires 16 lanes: requested {requested}, actual {actual}"
            ),
            Self::WrongFixture => write!(
                f,
                "fast requires the idle same-process text-provider fixture"
            ),
            Self::Errors { count } => write!(f, "fast has {count} errors"),
            Self::NonAnswered { rows } => write!(f, "fast has {rows} non-answered rows"),
            Self::MissingClock { clock, rows } => {
                write!(f, "fast has {rows} rows missing clock `{clock}`")
            }
            Self::InvalidClocks { rows } => {
                write!(f, "fast has {rows} rows with invalid causal clocks")
            }
            Self::InvalidPhase { phase, rows } => {
                write!(f, "fast has {rows} rows missing or invalid phase `{phase}`")
            }
            Self::OverheadPopulation { samples, expected } => write!(
                f,
                "fast overhead has {samples} samples, expected {expected}"
            ),
            Self::OverBudget {
                percentile,
                observed,
                budget,
            } => write!(
                f,
                "overhead {percentile} {observed:.1} ms >= {budget:.1} ms budget"
            ),
        }
    }
}

pub(super) fn population_refusals(samples: &[Sample]) -> Vec<GateRefusal> {
    let mut refusals = Vec::new();
    let non_answered = samples.iter().filter(|s| s.status != "answered").count();
    if non_answered > 0 {
        refusals.push(GateRefusal::NonAnswered { rows: non_answered });
    }
    let errors = samples
        .iter()
        .filter(|s| s.error.is_some() || s.poller_timed_out)
        .count();
    if errors > 0 {
        refusals.push(GateRefusal::Errors { count: errors });
    }
    for (clock, pick) in [
        (
            "accepted",
            (|s: &Sample| s.clocks.accepted) as fn(&Sample) -> Option<Stamp>,
        ),
        ("admission", |s: &Sample| s.clocks.admission),
        ("first-delta", |s: &Sample| s.clocks.first_delta),
        ("terminal", |s: &Sample| s.clocks.terminal),
    ] {
        let rows = samples.iter().filter(|s| pick(s).is_none()).count();
        if rows > 0 {
            refusals.push(GateRefusal::MissingClock { clock, rows });
        }
    }
    let rows = samples.iter().filter(|s| {
        let clocks = [Some(s.clocks.request), s.clocks.accepted, s.clocks.admission,
            s.clocks.first_delta, s.clocks.terminal, Some(s.clocks.completion)];
        clocks.iter().flatten().any(|c| c.unix_ns == 0 || c.monotonic_ns == 0)
            || clocks.windows(2).any(|p| matches!((p[0], p[1]), (Some(a), Some(b)) if a.monotonic_ns > b.monotonic_ns))
    }).count();
    if rows > 0 {
        refusals.push(GateRefusal::InvalidClocks { rows });
    }
    for (phase, pick) in [
        (
            "request-to-accept",
            (|s: &Sample| Some(s.request_to_accept_ms)) as fn(&Sample) -> Option<f64>,
        ),
        ("accept-to-admission", |s: &Sample| s.accept_to_admission_ms),
        ("admission-to-first-delta", |s: &Sample| {
            s.admission_to_first_delta_ms
        }),
        ("admission-to-settled", |s: &Sample| {
            s.admission_to_settled_ms
        }),
        ("settled-to-complete", |s: &Sample| s.settled_to_complete_ms),
        ("send-to-completion", |s: &Sample| {
            Some(s.send_to_completion_ms)
        }),
        ("provider", |s: &Sample| s.provider_ms),
        ("overhead", |s: &Sample| s.overhead_ms),
    ] {
        let rows = samples
            .iter()
            .filter(|s| pick(s).is_none_or(|v| !v.is_finite() || v < 0.0))
            .count();
        if rows > 0 {
            refusals.push(GateRefusal::InvalidPhase { phase, rows });
        }
    }
    refusals
}
