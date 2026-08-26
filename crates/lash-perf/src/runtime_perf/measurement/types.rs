const RUNTIME_PERF_TURN_TIMEOUT_ENV: &str = "LASH_RUNTIME_PERF_TURN_TIMEOUT_MS";
const DEFAULT_RUNTIME_PERF_TURN_TIMEOUT: Duration = Duration::from_secs(10);

const HIGH_TRAFFIC_KINDS: [&str; 6] = ["plain", "tool", "queued", "child", "wake", "trigger"];

#[derive(Clone, Debug)]
pub(crate) struct HighTrafficConfig {
    pub(crate) population: usize,
    pub(crate) arrival_rate: u64,
    pub(crate) mix: [u64; HIGH_TRAFFIC_KINDS.len()],
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
        let mut weights = [0; HIGH_TRAFFIC_KINDS.len()];
        for entry in mix.split(',').filter(|entry| !entry.trim().is_empty()) {
            let (kind, weight) = entry.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid high-traffic mix entry `{entry}`; expected kind=weight")
            })?;
            let index = HIGH_TRAFFIC_KINDS
                .iter()
                .position(|candidate| *candidate == kind.trim())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown high-traffic mix kind `{}`; expected one of {}",
                        kind.trim(),
                        HIGH_TRAFFIC_KINDS.join(", ")
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

    pub(crate) fn operation_kind(&self, ordinal: usize) -> &'static str {
        let total = self.mix.iter().sum::<u64>();
        let mut selected = ordinal as u64 % total;
        for (index, weight) in self.mix.iter().copied().enumerate() {
            if selected < weight {
                return HIGH_TRAFFIC_KINDS[index];
            }
            selected -= weight;
        }
        unreachable!("positive high-traffic weight total always selects a kind")
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfRunResult {
    pub(crate) scenario: String,
    pub(crate) scenario_harness: String,
    pub(crate) chat_turns: usize,
    pub(crate) stack_profile: Option<StackProfile>,
    pub(crate) build_runtime_ms: f64,
    pub(crate) seed_state_ms: f64,
    pub(crate) run_turn_ms: f64,
    pub(crate) await_background_work_ms: f64,
    pub(crate) export_state_ms: f64,
    pub(crate) total_ms: f64,
    pub(crate) session_nodes: usize,
    pub(crate) active_path_messages: usize,
    pub(crate) extra_counters: BTreeMap<String, u64>,
    pub(crate) metric_samples: BTreeMap<String, Vec<f64>>,
    pub(crate) metric_samples_ms: BTreeMap<String, Vec<f64>>,
    pub(crate) memory: RuntimePerfMemoryRunResult,
    pub(crate) allocations: RuntimePerfAllocationRunResult,
    pub(crate) phase_profile: BTreeMap<String, RuntimePerfPhaseRunResult>,
    pub(crate) turns: Vec<RuntimePerfTurnResult>,
    pub(crate) cumulative_usage: SessionUsageReport,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfTurnResult {
    pub(crate) turn_index: usize,
    pub(crate) run_turn_ms: f64,
    pub(crate) await_background_work_ms: f64,
    pub(crate) total_ms: f64,
    pub(crate) memory: RuntimePerfTurnMemoryRunResult,
    pub(crate) allocations: RuntimePerfTurnAllocationRunResult,
    pub(crate) phase_profile: BTreeMap<String, RuntimePerfPhaseRunResult>,
    pub(crate) turn_usage: TokenUsage,
    pub(crate) usage_delta: SessionUsageReport,
    pub(crate) cumulative_usage: SessionUsageReport,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfTurnMemoryRunResult {
    pub(crate) rss_before_kb: Option<u64>,
    pub(crate) rss_after_turn_kb: Option<u64>,
    pub(crate) rss_after_await_kb: Option<u64>,
    pub(crate) peak_hwm_before_kb: Option<u64>,
    pub(crate) peak_hwm_after_await_kb: Option<u64>,
    pub(crate) rss_growth_kb: Option<i64>,
    pub(crate) hwm_growth_kb: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfTurnAllocationRunResult {
    pub(crate) run_turn: RuntimePerfAllocationDelta,
    pub(crate) await_background_work: RuntimePerfAllocationDelta,
    pub(crate) total: RuntimePerfAllocationDelta,
}

async fn runtime_perf_timed<T, F>(
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
    match tokio::time::timeout(timeout, future).await {
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfMemoryRunResult {
    pub(crate) rss_before_kb: Option<u64>,
    pub(crate) rss_after_build_kb: Option<u64>,
    pub(crate) rss_after_seed_kb: Option<u64>,
    pub(crate) rss_after_turn_kb: Option<u64>,
    pub(crate) rss_after_await_kb: Option<u64>,
    pub(crate) rss_after_export_kb: Option<u64>,
    pub(crate) peak_hwm_before_kb: Option<u64>,
    pub(crate) peak_hwm_after_export_kb: Option<u64>,
    pub(crate) rss_growth_kb: Option<i64>,
    pub(crate) hwm_growth_kb: Option<i64>,
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
pub(crate) struct RuntimePerfAllocationRunResult {
    pub(crate) build_runtime: RuntimePerfAllocationDelta,
    pub(crate) seed_state: RuntimePerfAllocationDelta,
    pub(crate) run_turn: RuntimePerfAllocationDelta,
    pub(crate) await_background_work: RuntimePerfAllocationDelta,
    pub(crate) export_state: RuntimePerfAllocationDelta,
    pub(crate) total: RuntimePerfAllocationDelta,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfScenarioSummary {
    pub(crate) scenario: String,
    pub(crate) scenario_harness: String,
    pub(crate) scenario_harness_rationale: String,
    pub(crate) correctness_coverage_ids: Vec<String>,
    pub(crate) runs: usize,
    pub(crate) chat_turns: usize,
    pub(crate) stack_profile: StackProfile,
    pub(crate) build_runtime_ms: RuntimePerfMetricSummary,
    pub(crate) seed_state_ms: RuntimePerfMetricSummary,
    pub(crate) run_turn_ms: RuntimePerfMetricSummary,
    pub(crate) await_background_work_ms: RuntimePerfMetricSummary,
    pub(crate) export_state_ms: RuntimePerfMetricSummary,
    pub(crate) total_ms: RuntimePerfMetricSummary,
    pub(crate) rss_after_export_kb: Option<RuntimePerfMetricSummary>,
    pub(crate) rss_growth_kb: Option<RuntimePerfMetricSummary>,
    pub(crate) hwm_growth_kb: Option<RuntimePerfMetricSummary>,
    pub(crate) build_runtime_alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) build_runtime_live_bytes: RuntimePerfMetricSummary,
    pub(crate) seed_state_alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) seed_state_live_bytes: RuntimePerfMetricSummary,
    pub(crate) run_turn_alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) run_turn_live_bytes: RuntimePerfMetricSummary,
    pub(crate) await_background_work_alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) await_background_work_live_bytes: RuntimePerfMetricSummary,
    pub(crate) export_state_alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) export_state_live_bytes: RuntimePerfMetricSummary,
    pub(crate) total_alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) total_live_bytes: RuntimePerfMetricSummary,
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
    pub(crate) total_ms: RuntimePerfMetricSummary,
    pub(crate) run_turn_ms: RuntimePerfMetricSummary,
    pub(crate) await_background_work_ms: RuntimePerfMetricSummary,
    pub(crate) rss_growth_kb: Option<RuntimePerfMetricSummary>,
    pub(crate) total_alloc_bytes: RuntimePerfMetricSummary,
    pub(crate) total_live_bytes: RuntimePerfMetricSummary,
    pub(crate) phase_summary: BTreeMap<String, RuntimePerfPhaseSummary>,
}
