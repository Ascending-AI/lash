use super::*;

const RUNTIME_PERF_TURN_TIMEOUT_ENV: &str = "LASH_RUNTIME_PERF_TURN_TIMEOUT_MS";
const DEFAULT_RUNTIME_PERF_TURN_TIMEOUT: Duration = Duration::from_secs(10);

/// One high-traffic operation kind. `Display`/`FromStr` are the single
/// definition of the `load-kind:<kind>` prompt vocabulary: the measurement
/// side renders it into the prompt and the provider side parses it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HighTrafficOperationKind {
    Plain,
    Tool,
    Queued,
    Child,
    Wake,
    Trigger,
}

impl HighTrafficOperationKind {
    pub(crate) const ALL: [Self; 6] = [
        Self::Plain,
        Self::Tool,
        Self::Queued,
        Self::Child,
        Self::Wake,
        Self::Trigger,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Tool => "tool",
            Self::Queued => "queued",
            Self::Child => "child",
            Self::Wake => "wake",
            Self::Trigger => "trigger",
        }
    }
}

impl std::fmt::Display for HighTrafficOperationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for HighTrafficOperationKind {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CheckpointCurveConfig {
    pub(crate) transcript_bytes: usize,
    pub(crate) message_count: usize,
    pub(crate) graph_rows: usize,
    pub(crate) component_count: usize,
}

impl CheckpointCurveConfig {
    pub(crate) fn new(
        transcript_bytes: usize,
        message_count: usize,
        graph_rows: usize,
        component_count: usize,
    ) -> anyhow::Result<Self> {
        if transcript_bytes < 2_048 {
            anyhow::bail!("checkpoint transcript bytes must be at least 2048");
        }
        if message_count == 0 {
            anyhow::bail!("checkpoint message count must be positive");
        }
        if transcript_bytes < message_count {
            anyhow::bail!(
                "checkpoint transcript bytes must be at least the checkpoint message count"
            );
        }
        if graph_rows <= message_count {
            anyhow::bail!(
                "checkpoint graph rows must exceed the checkpoint message count to include the initial frame"
            );
        }
        if component_count < 4 {
            anyhow::bail!("checkpoint component count must be at least 4");
        }
        Ok(Self {
            transcript_bytes,
            message_count,
            graph_rows,
            component_count,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HighTrafficConfig {
    pub(crate) population: usize,
    pub(crate) arrival_rate: u64,
    pub(crate) mix: [u64; HighTrafficOperationKind::ALL.len()],
    pub(crate) knee_populations: Vec<usize>,
    pub(crate) knee_threshold: f64,
}

impl HighTrafficConfig {
    pub(crate) fn parse(
        population: usize,
        arrival_rate: u64,
        mix: &str,
        knee_populations: &str,
        knee_threshold: f64,
    ) -> anyhow::Result<Self> {
        let mut weights = [0; HighTrafficOperationKind::ALL.len()];
        for entry in mix.split(',').filter(|entry| !entry.trim().is_empty()) {
            let (kind, weight) = entry.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid high-traffic mix entry `{entry}`; expected kind=weight")
            })?;
            let index = HighTrafficOperationKind::ALL
                .iter()
                .position(|candidate| candidate.as_str() == kind.trim())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown high-traffic mix kind `{}`; expected one of {}",
                        kind.trim(),
                        HighTrafficOperationKind::ALL
                            .iter()
                            .map(|kind| kind.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            weights[index] = weight.trim().parse::<u64>().map_err(|_| {
                anyhow::anyhow!("invalid high-traffic mix weight in `{entry}`; expected an integer")
            })?;
        }
        if weights.iter().all(|weight| *weight == 0) {
            anyhow::bail!("high-traffic mix must contain at least one positive weight");
        }

        let knee_populations = knee_populations
            .split(',')
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                value.trim().parse::<usize>().map_err(|_| {
                    anyhow::anyhow!(
                        "invalid knee population `{value}`; expected a positive integer"
                    )
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if knee_populations.is_empty() || knee_populations.contains(&0) {
            anyhow::bail!("knee populations must contain positive integers");
        }
        if !knee_threshold.is_finite() || knee_threshold <= 1.0 {
            anyhow::bail!("knee threshold must be finite and greater than 1.0");
        }

        Ok(Self {
            population: population.max(1),
            arrival_rate,
            mix: weights,
            knee_populations,
            knee_threshold,
        })
    }

    pub(crate) fn operation_kind(&self, ordinal: usize) -> HighTrafficOperationKind {
        let total = self.mix.iter().sum::<u64>();
        let mut selected = ordinal as u64 % total;
        for (index, weight) in self.mix.iter().copied().enumerate() {
            if selected < weight {
                return HighTrafficOperationKind::ALL[index];
            }
            selected -= weight;
        }
        unreachable!("positive high-traffic weight total always selects a kind")
    }
}

/// Stage keys shared by the run- and turn-level `stages` maps.
///
/// The map is the contract: a stage the run never reached has no entry.
/// `0.0` is a real measurement, so encoding "did not run" as a zero-filled
/// field made a skipped stage indistinguishable from an instant one — and
/// every aggregation over it silently averaged the placeholder in.
pub(crate) mod stage {
    /// Runtime build before any state seeding.
    pub(crate) const BUILD_RUNTIME: &str = "build_runtime";
    /// State seeding between build and the first turn.
    pub(crate) const SEED_STATE: &str = "seed_state";
    /// Turn execution. At run level this is the sum over all turns.
    pub(crate) const RUN_TURN: &str = "run_turn";
    /// Draining background work after the turn.
    pub(crate) const AWAIT_BACKGROUND_WORK: &str = "await_background_work";
    /// Exporting the resulting state.
    pub(crate) const EXPORT_STATE: &str = "export_state";
    /// The whole run or turn, measured as its own span.
    pub(crate) const TOTAL: &str = "total";
}

/// One measured stage of a run or a turn: its wall clock, the allocation
/// delta it caused, and the RSS sampled at its boundary.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfStageRunResult {
    pub(crate) duration_ms: f64,
    pub(crate) allocations: RuntimePerfAllocationDelta,
    pub(crate) rss_after_kb: Option<u64>,
}

impl RuntimePerfStageRunResult {
    pub(crate) fn measured(
        duration_ms: f64,
        allocations: RuntimePerfAllocationDelta,
        rss_after_kb: Option<u64>,
    ) -> Self {
        Self {
            duration_ms,
            allocations,
            rss_after_kb,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfRunResult {
    pub(crate) scenario: String,
    pub(crate) scenario_harness: String,
    pub(crate) chat_turns: usize,
    pub(crate) stack_profile: Option<StackProfile>,
    /// The stages this run measured, keyed by `stage::*` name
    /// (`build_runtime`, `seed_state`, `run_turn`, `await_background_work`,
    /// `export_state`, `total`). A stage that did not run has no entry.
    pub(crate) stages: BTreeMap<String, RuntimePerfStageRunResult>,
    pub(crate) session_nodes: usize,
    pub(crate) active_path_messages: usize,
    pub(crate) extra_counters: BTreeMap<String, u64>,
    pub(crate) metric_samples: BTreeMap<String, Vec<f64>>,
    pub(crate) metric_samples_ms: BTreeMap<String, Vec<f64>>,
    /// Run-scoped memory: the opening sample and whole-run growth. Stage
    /// boundary readings live on the stage entries themselves.
    pub(crate) memory: RuntimePerfMemoryRunResult,
    pub(crate) phase_profile: BTreeMap<String, RuntimePerfPhaseRunResult>,
    pub(crate) turns: Vec<RuntimePerfTurnResult>,
    pub(crate) cumulative_usage: SessionUsageReport,
}

impl RuntimePerfRunResult {
    /// The stage entry by `stage::*` name, when the run reached it.
    pub(crate) fn stage(&self, name: &str) -> Option<&RuntimePerfStageRunResult> {
        self.stages.get(name)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfTurnResult {
    pub(crate) turn_index: usize,
    /// `stage::RUN_TURN`, `stage::AWAIT_BACKGROUND_WORK` and `stage::TOTAL`.
    pub(crate) stages: BTreeMap<String, RuntimePerfStageRunResult>,
    pub(crate) memory: RuntimePerfMemoryRunResult,
    pub(crate) phase_profile: BTreeMap<String, RuntimePerfPhaseRunResult>,
    pub(crate) turn_usage: TokenUsage,
    pub(crate) usage_delta: SessionUsageReport,
    pub(crate) cumulative_usage: SessionUsageReport,
}

impl RuntimePerfTurnResult {
    /// The stage entry by `stage::*` name, when the turn reached it.
    pub(crate) fn stage(&self, name: &str) -> Option<&RuntimePerfStageRunResult> {
        self.stages.get(name)
    }
}

/// A turn's stage map: `run_turn` and the `total` envelope always ran, while
/// `await_background_work` records only when the producer actually drained.
pub(crate) fn turn_stages(
    run_turn: RuntimePerfStageRunResult,
    await_background_work: Option<RuntimePerfStageRunResult>,
    total: RuntimePerfStageRunResult,
) -> BTreeMap<String, RuntimePerfStageRunResult> {
    let mut stages = BTreeMap::from([
        (stage::RUN_TURN.to_string(), run_turn),
        (stage::TOTAL.to_string(), total),
    ]);
    if let Some(await_stage) = await_background_work {
        stages.insert(stage::AWAIT_BACKGROUND_WORK.to_string(), await_stage);
    }
    stages
}

/// The run-level entry for a turn-level stage: summed duration and
/// allocations across the turns that ran it, and the last such turn's
/// boundary RSS. `None` when no turn reached the stage — the run records no
/// entry rather than a zero.
pub(crate) fn summed_turn_stage(
    turns: &[RuntimePerfTurnResult],
    name: &str,
) -> Option<RuntimePerfStageRunResult> {
    let present = turns
        .iter()
        .filter_map(|turn| turn.stage(name))
        .collect::<Vec<_>>();
    if present.is_empty() {
        return None;
    }
    Some(RuntimePerfStageRunResult {
        duration_ms: round3(present.iter().map(|stage| stage.duration_ms).sum()),
        allocations: sum_allocation_deltas(present.iter().map(|stage| &stage.allocations)),
        rss_after_kb: present.last().and_then(|stage| stage.rss_after_kb),
    })
}

/// A run's stage map. `measured` carries the stages the run instrumented
/// directly; the turn-level stages are then folded in from `turns` under
/// `run_turn`/`await_background_work` unless the run already named them.
pub(crate) fn run_stages(
    measured: impl IntoIterator<Item = (&'static str, RuntimePerfStageRunResult)>,
    turns: &[RuntimePerfTurnResult],
) -> BTreeMap<String, RuntimePerfStageRunResult> {
    let mut stages = measured
        .into_iter()
        .map(|(name, result)| (name.to_string(), result))
        .collect::<BTreeMap<_, _>>();
    for name in [stage::RUN_TURN, stage::AWAIT_BACKGROUND_WORK] {
        if let Some(entry) = summed_turn_stage(turns, name) {
            stages.entry(name.to_string()).or_insert(entry);
        }
    }
    stages
}

pub(super) async fn runtime_perf_timed<T, F>(
    scenario: RuntimePerfScenario,
    turn_index: usize,
    phase: &str,
    cancel: Option<CancellationToken>,
    future: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let timeout = runtime_perf_turn_timeout();
    match crate::runtime_perf::smoke::with_budget(timeout, future).await {
        Ok(result) => result,
        Err(_) => {
            if let Some(cancel) = cancel {
                cancel.cancel();
            }
            anyhow::bail!(
                "runtime perf scenario {} turn {} {phase} timed out after {} ms; profiling aborts instead of looping. Override with {RUNTIME_PERF_TURN_TIMEOUT_ENV}.",
                scenario.name(),
                turn_index + 1,
                timeout.as_millis()
            );
        }
    }
}

fn runtime_perf_turn_timeout() -> Duration {
    std::env::var(RUNTIME_PERF_TURN_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_RUNTIME_PERF_TURN_TIMEOUT)
}

/// Memory scoped to the whole run or turn: the opening sample, the closing
/// peak, and the growth between them. Stage-boundary RSS lives on the stage
/// entries — a stage that did not run has no boundary to read.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfMemoryRunResult {
    pub(crate) rss_before_kb: Option<u64>,
    pub(crate) peak_hwm_before_kb: Option<u64>,
    pub(crate) peak_hwm_after_kb: Option<u64>,
    pub(crate) rss_growth_kb: Option<i64>,
    pub(crate) hwm_growth_kb: Option<i64>,
}

/// The whole-span memory record between two samples.
pub(crate) fn memory_span(
    before: ProcessMemorySample,
    after: ProcessMemorySample,
) -> RuntimePerfMemoryRunResult {
    RuntimePerfMemoryRunResult {
        rss_before_kb: before.rss_kb,
        peak_hwm_before_kb: before.hwm_kb,
        peak_hwm_after_kb: after.hwm_kb,
        rss_growth_kb: diff_opt_i64(before.rss_kb, after.rss_kb),
        hwm_growth_kb: diff_opt_i64(before.hwm_kb, after.hwm_kb),
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfAllocationDelta {
    pub(crate) allocations: usize,
    pub(crate) deallocations: usize,
    pub(crate) reallocations: usize,
    pub(crate) bytes_allocated: usize,
    pub(crate) bytes_deallocated: usize,
    pub(crate) bytes_reallocated: isize,
    pub(crate) net_live_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfPhaseRunResult {
    pub(crate) samples: usize,
    pub(crate) duration_ms: f64,
    pub(crate) allocations: RuntimePerfAllocationDelta,
    pub(crate) rss_growth_kb: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfPhaseSummary {
    pub(crate) samples: RuntimePerfMetricSummary,
    pub(crate) duration_ms: RuntimePerfMetricSummary,
    pub(crate) alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) live_bytes: RuntimePerfMetricSummary,
    pub(crate) rss_growth_kb: Option<RuntimePerfMetricSummary>,
}

/// One stage summarized across runs.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfStageSummary {
    pub(crate) duration_ms: RuntimePerfMetricSummary,
    pub(crate) alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) live_bytes: RuntimePerfMetricSummary,
    /// RSS at the stage boundary, when the runs sampled it.
    pub(crate) rss_after_kb: Option<RuntimePerfMetricSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfScenarioSummary {
    pub(crate) scenario: String,
    pub(crate) scenario_harness: String,
    pub(crate) scenario_harness_rationale: String,
    pub(crate) correctness_coverage_ids: Vec<String>,
    pub(crate) runs: usize,
    pub(crate) chat_turns: usize,
    pub(crate) stack_profile: StackProfile,
    /// Per-stage summaries keyed by `stage::*` name. Only runs that reached
    /// the stage contribute, so a skipped stage has no summary entry either.
    pub(crate) stage_summary: BTreeMap<String, RuntimePerfStageSummary>,
    pub(crate) rss_growth_kb: Option<RuntimePerfMetricSummary>,
    pub(crate) hwm_growth_kb: Option<RuntimePerfMetricSummary>,
    pub(crate) phase_summary: BTreeMap<String, RuntimePerfPhaseSummary>,
    pub(crate) first_turn: RuntimePerfTurnSummary,
    pub(crate) steady_state_turn: Option<RuntimePerfTurnSummary>,
    pub(crate) last_turn: RuntimePerfTurnSummary,
    pub(crate) sample_session_nodes: usize,
    pub(crate) sample_active_path_messages: usize,
    pub(crate) sample_extra_counters: BTreeMap<String, u64>,
    pub(crate) metric_summary: BTreeMap<String, RuntimePerfMetricSummary>,
    pub(crate) metric_summary_ms: BTreeMap<String, RuntimePerfMetricSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfTurnSummary {
    /// Per-stage summaries across the grouped turns, keyed by `stage::*`.
    pub(crate) stage_summary: BTreeMap<String, RuntimePerfStageSummary>,
    pub(crate) rss_growth_kb: Option<RuntimePerfMetricSummary>,
    pub(crate) phase_summary: BTreeMap<String, RuntimePerfPhaseSummary>,
}

#[cfg(test)]
mod completion_smoke_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn smoke_waits_for_completion_beyond_the_unchanged_measurement_deadline() {
        for smoke in [true, false] {
            let cancel = CancellationToken::new();
            let result = crate::runtime_perf::smoke::execute(
                smoke,
                RuntimePerfScenario::Standard,
                1,
                runtime_perf_timed(
                    RuntimePerfScenario::Standard,
                    0,
                    "run_turn",
                    Some(cancel.clone()),
                    async {
                        tokio::time::sleep(Duration::from_secs(11)).await;
                        Ok(())
                    },
                ),
            )
            .await;
            if smoke {
                result.expect("smoke completion has no elapsed-time verdict");
                assert!(!cancel.is_cancelled());
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("timed out after 10000 ms")
                );
                assert!(cancel.is_cancelled());
            }
        }
    }
}
