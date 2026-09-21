use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use lash::usage::SessionUsageReport;
use serde::Serialize;

use crate::perf_support::dhat;
use crate::perf_support::metrics::{
    BasicMetricSummary as RuntimePerfMetricSummary, basic_summary, optional_basic_summary,
};
use crate::perf_support::paths;
use crate::perf_support::report as report_support;
use crate::perf_support::stack::{DEFAULT_STACK_BUDGET_BYTES, StackProfile};
use crate::perf_support::time::round3;

use super::duration_trend;
use super::measurement::*;
use super::scenarios::{RuntimePerfScenario, ScenarioHarnessKind};

mod budgets;
mod guards;
pub use guards::BudgetEnforcement;
use guards::{
    RuntimePerfBudgetResult, enforcement_failures, evaluate_budgets, report_advisory_exceedances,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfReport {
    kind: &'static str,
    created_at: String,
    version: String,
    warmups: usize,
    runs: usize,
    chat_turns: usize,
    worker_stack_bytes: usize,
    stack_profile: StackProfile,
    scenarios: Vec<String>,
    scenario_harnesses: Vec<String>,
    allocation_mode: &'static str,
    dhat_out: Option<PathBuf>,
    results: Vec<RuntimePerfRunResult>,
    summary: Vec<RuntimePerfScenarioSummary>,
    scenario_harness_summary: Vec<RuntimePerfScenarioHarnessSummary>,
    budget_results: Vec<RuntimePerfBudgetResult>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfScenarioHarnessSummary {
    scenario_harness: String,
    scenarios: Vec<String>,
    runs: usize,
    total_ms: RuntimePerfMetricSummary,
    total_alloc_bytes: RuntimePerfMetricSummary,
}

pub(crate) fn default_output_path() -> PathBuf {
    paths::default_report_path("runtime-perf")
}

/// One runtime-perf run, named rather than positional.
///
/// `run_cli` used to take these as 25 positional parameters, eleven of them
/// `usize` and seven of those consecutive, every one already a field of the
/// clap struct that was destructured only to be re-associated by position. A
/// transposition of `checkpoint_messages` and `checkpoint_graph_rows` at the
/// call site type-checked and measured a different shape than the flags said,
/// caught only by `CheckpointCurveConfig::new`'s `graph_rows <= message_count`
/// bail -- and only for that one pair.
///
/// The fields are the CLI's own vocabulary, so `main.rs` builds this by field
/// name straight off `Args`. The library cannot name clap's `Args` -- it lives
/// in the binary crate -- so this is where "pass the struct" lands.
#[derive(Debug, Clone)]
pub struct RuntimePerfRun {
    pub out: Option<PathBuf>,
    pub enable_dhat: bool,
    pub dhat_out: Option<PathBuf>,
    pub dhat_frames: Option<usize>,
    pub worker_stack_bytes: usize,
    pub runs: usize,
    pub warmups: usize,
    pub scenario_filters: Vec<String>,
    pub chat_turns: usize,
    pub contention_workers: usize,
    pub checkpoint_transcript_bytes: usize,
    pub checkpoint_messages: usize,
    pub checkpoint_graph_rows: usize,
    pub checkpoint_components: usize,
    pub high_traffic_population: usize,
    pub high_traffic_arrival_rate: u64,
    pub high_traffic_mix: String,
    pub high_traffic_knee_populations: String,
    pub high_traffic_knee_threshold: f64,
    pub enforcement: BudgetEnforcement,
    pub smoke: bool,
    pub duration_history: Option<PathBuf>,
    pub duration_profile: String,
    pub version: String,
}

pub async fn run_cli(run: RuntimePerfRun) -> anyhow::Result<()> {
    let RuntimePerfRun {
        out,
        enable_dhat,
        dhat_out,
        dhat_frames,
        worker_stack_bytes,
        runs,
        warmups,
        scenario_filters,
        chat_turns,
        contention_workers,
        checkpoint_transcript_bytes,
        checkpoint_messages,
        checkpoint_graph_rows,
        checkpoint_components,
        high_traffic_population,
        high_traffic_arrival_rate,
        high_traffic_mix,
        high_traffic_knee_populations,
        high_traffic_knee_threshold,
        enforcement,
        smoke,
        duration_history,
        duration_profile,
        version,
    } = run;
    if dhat_out.is_some() && !enable_dhat {
        anyhow::bail!("--runtime-perf-dhat-out requires --runtime-perf-dhat");
    }
    let scenarios = resolve_scenarios(&scenario_filters)?;
    let runs = runs.max(1);
    let chat_turns = chat_turns.max(1);
    let duration_geometry =
        duration_trend::DurationTrendGeometry::current(runs, warmups, chat_turns);
    let contention_workers = contention_workers.max(1);
    let checkpoint_curve = CheckpointCurveConfig::new(
        checkpoint_transcript_bytes,
        checkpoint_messages,
        checkpoint_graph_rows,
        checkpoint_components,
    )?;
    let high_traffic = HighTrafficConfig::parse(
        high_traffic_population,
        high_traffic_arrival_rate,
        &high_traffic_mix,
        &high_traffic_knee_populations,
        high_traffic_knee_threshold,
    )?;
    let stack_profile = stack_profile(worker_stack_bytes);

    for _ in 0..warmups {
        for scenario in &scenarios {
            let _ = super::smoke::execute(
                smoke,
                *scenario,
                chat_turns,
                Box::pin(run_once(
                    *scenario,
                    chat_turns,
                    contention_workers,
                    &checkpoint_curve,
                    &high_traffic,
                )),
            )
            .await?;
        }
    }

    let out_path = out.unwrap_or_else(default_output_path);
    report_support::ensure_parent_dir(&out_path, "benchmark output")?;
    // The quick main-push job supplies its cache-backed history explicitly.
    // Full and release runs already write their report into an uploaded perf
    // artifact, so give those runs a sibling ledger without requiring a
    // workflow-specific flag. The profile key keeps that ledger separate from
    // the quick series if a caller later combines the retained artifacts.
    let auto_full_duration_history =
        duration_history.is_none() && is_full_profile(runs, warmups, chat_turns);
    let duration_profile = if auto_full_duration_history && duration_profile == "custom" {
        "full".to_string()
    } else {
        duration_profile
    };
    let duration_history = duration_history
        .or_else(|| auto_full_duration_history.then(|| full_duration_history_path(&out_path)));
    let dhat_out_path = resolve_dhat_output_path(enable_dhat, &out_path, dhat_out);
    dhat::ensure_dhat_parent(dhat_out_path.as_ref())?;

    let profiler = dhat::start_dhat_profiler(
        dhat_out_path.clone(),
        dhat_frames,
        "runtime perf dhat profiling requires a lash-perf build with --features dhat-heap",
    )?;
    let mut results = Vec::with_capacity(runs * scenarios.len());
    for _ in 0..runs {
        for scenario in &scenarios {
            let mut result = super::smoke::execute(
                smoke,
                *scenario,
                chat_turns,
                Box::pin(run_once(
                    *scenario,
                    chat_turns,
                    contention_workers,
                    &checkpoint_curve,
                    &high_traffic,
                )),
            )
            .await?;
            result.stack_profile = Some(stack_profile.clone());
            results.push(result);
        }
    }
    dhat::finish_dhat_profiler(profiler);

    let summary = summarize(&results, &scenarios, chat_turns, &stack_profile);
    let scenario_harness_summary = summarize_scenario_harnesses(&results, &scenarios);
    let budget_results = evaluate_budgets(&summary, &scenarios);
    let report = RuntimePerfReport {
        kind: "runtime-perf",
        created_at: Utc::now().to_rfc3339(),
        version,
        warmups,
        runs,
        chat_turns,
        worker_stack_bytes,
        stack_profile,
        scenarios: scenarios
            .iter()
            .map(|scenario| scenario.name().to_string())
            .collect(),
        scenario_harnesses: selected_scenario_harnesses(&scenarios),
        allocation_mode: crate::ALLOCATION_MODE,
        dhat_out: dhat_out_path.clone(),
        summary,
        scenario_harness_summary,
        budget_results,
        results,
    };

    write_runtime_perf_report(&out_path, &report)?;

    println!(
        "{}",
        serde_json::to_string_pretty(&runtime_perf_output_json(&out_path, &report))?
    );
    report_advisory_exceedances(&report.budget_results);
    // The trend runs before enforcement so a scenario whose allocation ceiling
    // failed still contributes its duration observation: the history is a
    // record of what the machine measured, not of which runs passed.
    if let Some(history_path) = duration_history.as_deref() {
        duration_trend::record_and_report(
            history_path,
            &duration_profile,
            duration_geometry,
            &report.summary,
        );
    }
    let failures = enforcement_failures(&report.budget_results, enforcement);
    if !failures.is_empty() {
        anyhow::bail!("{}:\n{}", enforcement.failure_label(), failures.join("\n"));
    }
    Ok(())
}

fn runtime_perf_output_json(out_path: &Path, report: &RuntimePerfReport) -> serde_json::Value {
    serde_json::json!({
        "out": out_path,
        "dhat_out": report.dhat_out,
        "worker_stack_bytes": report.worker_stack_bytes,
        "stack_profile": report.stack_profile,
        "allocation_mode": report.allocation_mode,
        "scenario_harnesses": report.scenario_harnesses,
        "summary": report.summary,
        "scenario_harness_summary": report.scenario_harness_summary,
        "budget_results": report.budget_results,
    })
}

fn write_runtime_perf_report(path: &Path, report: &RuntimePerfReport) -> anyhow::Result<()> {
    if report.summary.is_empty() {
        anyhow::bail!("refusing to write runtime perf report with an empty summary");
    }
    report_support::write_json_report(path, report)
}

fn resolve_dhat_output_path(
    enable_dhat: bool,
    report_out: &Path,
    dhat_out: Option<PathBuf>,
) -> Option<PathBuf> {
    dhat::resolve_dhat_output_path(enable_dhat, report_out, dhat_out, "runtime-perf")
}

fn stack_profile(worker_stack_bytes: usize) -> StackProfile {
    StackProfile::capture(Some(worker_stack_bytes), Some(DEFAULT_STACK_BUDGET_BYTES))
}

fn is_full_profile(runs: usize, warmups: usize, chat_turns: usize) -> bool {
    runs == 5 && warmups == 1 && chat_turns == 12
}

fn full_duration_history_path(out_path: &Path) -> PathBuf {
    out_path.with_file_name("runtime-duration-history-full.jsonl")
}

fn resolve_scenarios(filters: &[String]) -> anyhow::Result<Vec<RuntimePerfScenario>> {
    report_support::resolve_named_scenarios(
        filters,
        &RuntimePerfScenario::DEFAULTS,
        &RuntimePerfScenario::KNOWN,
        RuntimePerfScenario::parse,
        RuntimePerfScenario::name,
        "runtime perf",
    )
}

fn selected_scenario_harnesses(scenarios: &[RuntimePerfScenario]) -> Vec<String> {
    ScenarioHarnessKind::ALL
        .iter()
        .copied()
        .filter(|kind| {
            scenarios
                .iter()
                .any(|scenario| scenario.scenario_harness() == *kind)
        })
        .map(|kind| kind.name().to_string())
        .collect()
}

fn summarize_scenario_harnesses(
    results: &[RuntimePerfRunResult],
    scenarios: &[RuntimePerfScenario],
) -> Vec<RuntimePerfScenarioHarnessSummary> {
    ScenarioHarnessKind::ALL
        .iter()
        .copied()
        .filter_map(|kind| {
            let scenario_names = scenarios
                .iter()
                .copied()
                .filter(|scenario| scenario.scenario_harness() == kind)
                .map(RuntimePerfScenario::name)
                .collect::<Vec<_>>();
            if scenario_names.is_empty() {
                return None;
            }
            let matching = results
                .iter()
                .filter(|result| scenario_names.iter().any(|name| *name == result.scenario))
                .collect::<Vec<_>>();
            if matching.is_empty() {
                return None;
            }
            Some(RuntimePerfScenarioHarnessSummary {
                scenario_harness: kind.name().to_string(),
                scenarios: scenario_names
                    .iter()
                    .map(|scenario| (*scenario).to_string())
                    .collect(),
                runs: matching.len(),
                total_ms: summarize_metric(
                    matching
                        .iter()
                        .filter_map(|result| {
                            result.stage(stage::TOTAL).map(|stage| stage.duration_ms)
                        })
                        .collect::<Vec<_>>(),
                ),
                total_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .filter_map(|result| {
                            result
                                .stage(stage::TOTAL)
                                .map(|stage| stage.allocations.bytes_allocated as f64)
                        })
                        .collect::<Vec<_>>(),
                ),
            })
        })
        .collect()
}

fn summarize(
    results: &[RuntimePerfRunResult],
    scenarios: &[RuntimePerfScenario],
    chat_turns: usize,
    stack_profile: &StackProfile,
) -> Vec<RuntimePerfScenarioSummary> {
    scenarios
        .iter()
        .filter_map(|scenario| {
            let matching = results
                .iter()
                .filter(|result| result.scenario == scenario.name())
                .collect::<Vec<_>>();
            if matching.is_empty() {
                return None;
            }
            Some(RuntimePerfScenarioSummary {
                scenario: scenario.name().to_string(),
                scenario_harness: scenario.scenario_harness().name().to_string(),
                scenario_harness_rationale: scenario.scenario_harness_rationale().to_string(),
                correctness_coverage_ids: scenario
                    .correctness_coverage_ids()
                    .iter()
                    .map(|id| (*id).to_string())
                    .collect(),
                runs: matching.len(),
                chat_turns,
                stack_profile: stack_profile.clone(),
                stage_summary: summarize_stage_entries(
                    matching.iter().map(|result| &result.stages),
                ),
                rss_growth_kb: summarize_optional_metric(
                    matching
                        .iter()
                        .filter_map(|result| result.memory.rss_growth_kb.map(|value| value as f64))
                        .collect::<Vec<_>>(),
                ),
                hwm_growth_kb: summarize_optional_metric(
                    matching
                        .iter()
                        .filter_map(|result| result.memory.hwm_growth_kb.map(|value| value as f64))
                        .collect::<Vec<_>>(),
                ),
                phase_summary: summarize_phase_profiles(
                    &matching
                        .iter()
                        .map(|result| result.phase_profile.clone())
                        .collect::<Vec<_>>(),
                ),
                first_turn: summarize_turn_group(
                    &matching
                        .iter()
                        .filter_map(|result| result.turns.first().cloned())
                        .collect::<Vec<_>>(),
                ),
                steady_state_turn: summarize_optional_turn_group(
                    &matching
                        .iter()
                        .filter_map(|result| mean_turn_result(&result.turns[1..]))
                        .collect::<Vec<_>>(),
                ),
                last_turn: summarize_turn_group(
                    &matching
                        .iter()
                        .filter_map(|result| result.turns.last().cloned())
                        .collect::<Vec<_>>(),
                ),
                sample_session_nodes: matching[0].session_nodes,
                sample_active_path_messages: matching[0].active_path_messages,
                sample_extra_counters: matching[0].extra_counters.clone(),
                metric_summary: summarize_metric_samples(&matching, |result| {
                    &result.metric_samples
                }),
                metric_summary_ms: summarize_metric_samples(&matching, |result| {
                    &result.metric_samples_ms
                }),
            })
        })
        .collect()
}

fn summarize_metric_samples(
    results: &[&RuntimePerfRunResult],
    samples_for: fn(&RuntimePerfRunResult) -> &BTreeMap<String, Vec<f64>>,
) -> BTreeMap<String, RuntimePerfMetricSummary> {
    let mut samples_by_key = BTreeMap::<String, Vec<f64>>::new();
    for result in results {
        for (key, samples) in samples_for(result) {
            samples_by_key
                .entry(key.clone())
                .or_default()
                .extend(samples.iter().copied());
        }
    }
    samples_by_key
        .into_iter()
        .map(|(key, samples)| (key, summarize_metric(samples)))
        .collect()
}

fn summarize_phase_profiles(
    profiles: &[BTreeMap<String, RuntimePerfPhaseRunResult>],
) -> BTreeMap<String, RuntimePerfPhaseSummary> {
    let mut by_phase: BTreeMap<String, Vec<&RuntimePerfPhaseRunResult>> = BTreeMap::new();
    for profile in profiles {
        for (phase, metrics) in profile {
            by_phase.entry(phase.clone()).or_default().push(metrics);
        }
    }

    by_phase
        .into_iter()
        .map(|(phase, metrics)| {
            let summary = RuntimePerfPhaseSummary {
                samples: summarize_metric(
                    metrics.iter().map(|metric| metric.samples as f64).collect(),
                ),
                duration_ms: summarize_metric(
                    metrics.iter().map(|metric| metric.duration_ms).collect(),
                ),
                alloc_bytes: summarize_metric(
                    metrics
                        .iter()
                        .map(|metric| metric.allocations.bytes_allocated as f64)
                        .collect(),
                ),
                live_bytes: summarize_metric(
                    metrics
                        .iter()
                        .map(|metric| metric.allocations.net_live_bytes as f64)
                        .collect(),
                ),
                rss_growth_kb: summarize_optional_metric(
                    metrics
                        .iter()
                        .filter_map(|metric| metric.rss_growth_kb.map(|value| value as f64))
                        .collect(),
                ),
            };
            (phase, summary)
        })
        .collect()
}

/// Fold every run's (or turn group's) stage maps into per-stage summaries.
/// Only records that contain the key contribute — a stage that did not run
/// has no entry and so never reaches the summary, while a measured `0.0`
/// stays a real sample.
fn summarize_stage_entries<'a>(
    stage_maps: impl IntoIterator<Item = &'a BTreeMap<String, RuntimePerfStageRunResult>>,
) -> BTreeMap<String, RuntimePerfStageSummary> {
    let mut by_stage: BTreeMap<String, Vec<&RuntimePerfStageRunResult>> = BTreeMap::new();
    for stages in stage_maps {
        for (name, entry) in stages {
            by_stage.entry(name.clone()).or_default().push(entry);
        }
    }
    by_stage
        .into_iter()
        .map(|(name, entries)| {
            let summary = RuntimePerfStageSummary {
                duration_ms: summarize_metric(
                    entries.iter().map(|entry| entry.duration_ms).collect(),
                ),
                alloc_bytes: summarize_metric(
                    entries
                        .iter()
                        .map(|entry| entry.allocations.bytes_allocated as f64)
                        .collect(),
                ),
                live_bytes: summarize_metric(
                    entries
                        .iter()
                        .map(|entry| entry.allocations.net_live_bytes as f64)
                        .collect(),
                ),
                rss_after_kb: summarize_optional_metric(
                    entries
                        .iter()
                        .filter_map(|entry| entry.rss_after_kb.map(|value| value as f64))
                        .collect(),
                ),
            };
            (name, summary)
        })
        .collect()
}

fn summarize_turn_group(turns: &[RuntimePerfTurnResult]) -> RuntimePerfTurnSummary {
    RuntimePerfTurnSummary {
        stage_summary: summarize_stage_entries(turns.iter().map(|turn| &turn.stages)),
        rss_growth_kb: summarize_optional_metric(
            turns
                .iter()
                .filter_map(|turn| turn.memory.rss_growth_kb.map(|value| value as f64))
                .collect(),
        ),
        phase_summary: summarize_phase_profiles(
            &turns
                .iter()
                .map(|turn| turn.phase_profile.clone())
                .collect::<Vec<_>>(),
        ),
    }
}

fn summarize_optional_turn_group(
    turns: &[RuntimePerfTurnResult],
) -> Option<RuntimePerfTurnSummary> {
    if turns.is_empty() {
        None
    } else {
        Some(summarize_turn_group(turns))
    }
}

fn mean_turn_result(turns: &[RuntimePerfTurnResult]) -> Option<RuntimePerfTurnResult> {
    if turns.is_empty() {
        return None;
    }

    Some(RuntimePerfTurnResult {
        turn_index: turns[0].turn_index,
        stages: mean_turn_stages(turns),
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: None,
            peak_hwm_before_kb: None,
            peak_hwm_after_kb: None,
            rss_growth_kb: mean_option_i64(turns.iter().map(|turn| turn.memory.rss_growth_kb)),
            hwm_growth_kb: mean_option_i64(turns.iter().map(|turn| turn.memory.hwm_growth_kb)),
        },
        phase_profile: mean_phase_profiles(turns.iter().map(|turn| &turn.phase_profile)),
        turn_usage: mean_token_usage(turns.iter().map(|turn| &turn.turn_usage)),
        usage_delta: SessionUsageReport::default(),
        cumulative_usage: SessionUsageReport::default(),
    })
}

/// The mean of each stage across the turns that ran it. A stage absent from
/// every turn stays absent; one measured on a subset averages that subset.
fn mean_turn_stages(
    turns: &[RuntimePerfTurnResult],
) -> BTreeMap<String, RuntimePerfStageRunResult> {
    let mut means = BTreeMap::new();
    for name in turns.iter().flat_map(|turn| turn.stages.keys()) {
        if means.contains_key(name) {
            continue;
        }
        let present = turns
            .iter()
            .filter_map(|turn| turn.stage(name))
            .collect::<Vec<_>>();
        let count = present.len() as f64;
        means.insert(
            name.clone(),
            RuntimePerfStageRunResult {
                duration_ms: round3(
                    present.iter().map(|stage| stage.duration_ms).sum::<f64>() / count,
                ),
                allocations: mean_allocation_delta(present.iter().map(|stage| &stage.allocations)),
                rss_after_kb: mean_option_u64(present.iter().map(|stage| stage.rss_after_kb)),
            },
        );
    }
    means
}

fn mean_option_u64(values: impl IntoIterator<Item = Option<u64>>) -> Option<u64> {
    let present = values.into_iter().flatten().collect::<Vec<_>>();
    (!present.is_empty())
        .then(|| (present.iter().sum::<u64>() as f64 / present.len() as f64).round() as u64)
}
fn summarize_metric(values: Vec<f64>) -> RuntimePerfMetricSummary {
    basic_summary(values)
}

fn summarize_optional_metric(values: Vec<f64>) -> Option<RuntimePerfMetricSummary> {
    optional_basic_summary(values)
}
#[cfg(test)]
mod tests {
    use super::budgets::{
        allocation_budget_bytes, phase_wall_clock_budget_ms,
        steady_state_turn_allocation_budget_bytes, wall_clock_budget_ms,
    };
    use super::guards::required_phases;
    use super::*;
    use crate::runtime_perf::openai_compat::openai_compat_sse_body;
    use crate::runtime_perf::providers::benchmark_stream_profile;
    use std::collections::HashSet;

    fn allocation_delta(bytes_allocated: usize) -> RuntimePerfAllocationDelta {
        RuntimePerfAllocationDelta {
            allocations: usize::from(bytes_allocated > 0),
            deallocations: 0,
            reallocations: 0,
            bytes_allocated,
            bytes_deallocated: 0,
            bytes_reallocated: 0,
            net_live_bytes: bytes_allocated as i64,
        }
    }

    fn memory_run() -> RuntimePerfMemoryRunResult {
        RuntimePerfMemoryRunResult {
            rss_before_kb: None,
            peak_hwm_before_kb: None,
            peak_hwm_after_kb: None,
            rss_growth_kb: None,
            hwm_growth_kb: None,
        }
    }

    fn stage_result(duration_ms: f64, bytes_allocated: usize) -> RuntimePerfStageRunResult {
        RuntimePerfStageRunResult::measured(duration_ms, allocation_delta(bytes_allocated), None)
    }

    fn turn_result(total_ms: f64, bytes_allocated: usize) -> RuntimePerfTurnResult {
        RuntimePerfTurnResult {
            turn_index: 0,
            stages: turn_stages(
                stage_result(total_ms / 2.0, bytes_allocated / 2),
                Some(stage_result(total_ms / 4.0, bytes_allocated / 4)),
                stage_result(total_ms, bytes_allocated),
            ),
            memory: memory_run(),
            phase_profile: BTreeMap::new(),
            turn_usage: lash_core::TokenUsage::default(),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        }
    }

    pub(super) fn run_result(
        scenario: RuntimePerfScenario,
        total_ms: f64,
        bytes_allocated: usize,
    ) -> RuntimePerfRunResult {
        let turn = turn_result(total_ms, bytes_allocated);
        let mut run = RunRecorder::start(scenario, 1);
        run.record_stage(stage::BUILD_RUNTIME, stage_result(1.0, 1));
        run.record_stage(stage::SEED_STATE, stage_result(1.0, 2));
        run.record_stage(stage::EXPORT_STATE, stage_result(1.0, 3));
        run.record_turn(turn);
        run.finish(RunTail {
            session_nodes: 1,
            active_path_messages: 1,
            memory: Some(memory_run()),
            phase_profile: Some(BTreeMap::new()),
            total_stage: Some(stage_result(total_ms, bytes_allocated)),
            ..RunTail::default()
        })
    }

    #[test]
    fn runtime_perf_scenario_metadata_is_single_source_for_lookup_and_grouping() {
        assert_eq!(
            RuntimePerfScenario::METADATA.len(),
            RuntimePerfScenario::KNOWN.len()
        );
        assert_eq!(
            RuntimePerfScenario::KNOWN,
            RuntimePerfScenario::METADATA.map(|metadata| metadata.scenario)
        );

        let mut seen_scenarios = HashSet::new();
        let mut seen_names = HashSet::new();
        let mut seen_harnesses = HashSet::new();
        for metadata in RuntimePerfScenario::METADATA {
            assert!(
                seen_scenarios.insert(metadata.scenario),
                "duplicate runtime perf scenario metadata for {:?}",
                metadata.scenario
            );
            assert!(
                seen_names.insert(metadata.name),
                "duplicate runtime perf scenario name `{}`",
                metadata.name
            );
            seen_harnesses.insert(metadata.scenario_harness);
            assert!(
                !metadata.harness_rationale.trim().is_empty(),
                "{} must explain its scenario harness classification",
                metadata.name
            );
            assert_eq!(
                RuntimePerfScenario::parse(metadata.name),
                Some(metadata.scenario)
            );
            assert_eq!(metadata.scenario.name(), metadata.name);
            assert_eq!(metadata.scenario.execution_mode(), metadata.execution_mode);
            assert_eq!(
                metadata.scenario.scenario_harness(),
                metadata.scenario_harness
            );
            for coverage_id in metadata.correctness_coverage_ids {
                assert!(
                    coverage_id.starts_with("runtime_scenario_")
                        || coverage_id.starts_with("standard_protocol_scenario_")
                        || coverage_id.starts_with("rlm_protocol_scenario_")
                        || coverage_id.starts_with("rlm_prompt_history_")
                        || coverage_id.starts_with("agent_scenario_"),
                    "{} links to non-canonical correctness coverage id {}",
                    metadata.name,
                    coverage_id
                );
            }
        }
        for kind in ScenarioHarnessKind::ALL {
            assert!(
                seen_harnesses.contains(&kind),
                "missing at least one scenario for {}",
                kind.name()
            );
        }
        for ambiguous in [
            RuntimePerfScenario::OpenAiResponsesSseParse,
            RuntimePerfScenario::DirectLlmClient,
            RuntimePerfScenario::OpenAiCompatStream,
            RuntimePerfScenario::ToolDiscoverySearch,
        ] {
            let metadata = RuntimePerfScenario::METADATA
                .iter()
                .find(|metadata| metadata.scenario == ambiguous)
                .expect("ambiguous scenario metadata");
            assert!(
                metadata.harness_rationale.len() > metadata.scenario_harness.name().len(),
                "{} needs a real classification rationale",
                metadata.name
            );
        }
    }

    #[test]
    fn durable_scenario_metadata_matches_representative_turn_family() {
        let durable_scenarios: Vec<_> = RuntimePerfScenario::METADATA
            .iter()
            .filter(|metadata| metadata.durability.is_durable())
            .map(|metadata| metadata.scenario)
            .collect();
        let family = RuntimePerfScenario::DURABLE_REPRESENTATIVE_TURNS;
        let missing: Vec<_> = durable_scenarios
            .iter()
            .copied()
            .filter(|scenario| !family.contains(scenario))
            .collect();
        let non_durable_family: Vec<_> = family
            .iter()
            .copied()
            .filter(|scenario| !scenario.is_durable())
            .collect();

        assert!(
            missing.is_empty() && non_durable_family.is_empty(),
            "DURABLE_REPRESENTATIVE_TURNS must exactly contain metadata-marked durable scenarios: durable_count={}, family_count={}, missing={missing:?}, non_durable_family={non_durable_family:?}",
            durable_scenarios.len(),
            family.len(),
        );
        assert_eq!(
            durable_scenarios.len(),
            family.len(),
            "DURABLE_REPRESENTATIVE_TURNS must not contain duplicate scenarios"
        );
    }

    #[test]
    fn runtime_perf_report_serializes_scenario_harness_groups() {
        let scenarios = vec![
            RuntimePerfScenario::TurnCheckpoint,
            RuntimePerfScenario::Standard,
            RuntimePerfScenario::Rlm,
            RuntimePerfScenario::RlmProcessHandles,
        ];
        let results = vec![
            run_result(RuntimePerfScenario::TurnCheckpoint, 10.0, 100),
            run_result(RuntimePerfScenario::Standard, 20.0, 200),
            run_result(RuntimePerfScenario::Rlm, 30.0, 300),
            run_result(RuntimePerfScenario::RlmProcessHandles, 40.0, 400),
        ];
        let stack_profile = stack_profile(2 * 1024 * 1024);
        let summary = summarize(&results, &scenarios, 1, &stack_profile);
        let scenario_harness_summary = summarize_scenario_harnesses(&results, &scenarios);
        let report = RuntimePerfReport {
            kind: "runtime-perf",
            created_at: "test".to_string(),
            version: "test".to_string(),
            warmups: 0,
            runs: 1,
            chat_turns: 1,
            worker_stack_bytes: 2 * 1024 * 1024,
            stack_profile,
            scenarios: scenarios
                .iter()
                .map(|scenario| scenario.name().to_string())
                .collect(),
            scenario_harnesses: selected_scenario_harnesses(&scenarios),
            allocation_mode: crate::ALLOCATION_MODE,
            dhat_out: None,
            results,
            summary,
            scenario_harness_summary,
            budget_results: Vec::new(),
        };

        let report_json = serde_json::to_value(&report).expect("report serializes");
        let dir = tempfile::tempdir().expect("temp dir");
        let empty_path = dir.path().join("empty-summary.json");
        let mut empty_report = report.clone();
        empty_report.summary.clear();
        let error = write_runtime_perf_report(&empty_path, &empty_report)
            .expect_err("summary-less reports must not be written");
        assert!(format!("{error:#}").contains("summary"), "{error:#}");
        assert!(!empty_path.exists(), "summary-less report was written");
        assert_eq!(report_json["kind"], "runtime-perf");
        assert_eq!(
            report_json["scenario_harnesses"],
            serde_json::json!([
                "Runtime Scenario",
                "Standard Protocol Scenario",
                "RLM Protocol Scenario",
                "Agent Scenario"
            ])
        );
        assert_eq!(report_json["allocation_mode"], crate::ALLOCATION_MODE);
        assert_eq!(
            report_json["scenario_harness_summary"][0]["scenario_harness"],
            "Runtime Scenario"
        );
        assert_eq!(
            report_json["scenario_harness_summary"][0]["scenarios"],
            serde_json::json!(["turn_checkpoint"])
        );
        assert_eq!(
            report_json["summary"][0]["scenario_harness"],
            "Runtime Scenario"
        );
        assert!(
            report_json["summary"][0]["scenario_harness_rationale"]
                .as_str()
                .is_some_and(|value| value.contains("runtime checkpoint"))
        );
        assert_eq!(
            report_json["summary"][0]["correctness_coverage_ids"],
            serde_json::json!([
                "runtime_scenario_drains_command_before_turn_work_and_commits_checkpoint"
            ])
        );
        assert_eq!(
            report_json["results"][0]["scenario_harness"],
            "Runtime Scenario"
        );

        let output_json = runtime_perf_output_json(Path::new("runtime-perf.json"), &report);
        assert_eq!(
            output_json["scenario_harnesses"],
            report_json["scenario_harnesses"]
        );
        assert_eq!(
            output_json["scenario_harness_summary"],
            report_json["scenario_harness_summary"]
        );
        assert_eq!(output_json["allocation_mode"], crate::ALLOCATION_MODE);
        assert!(output_json.get("kind").is_none());

        let output_golden = serde_json::json!({
            "scenario_harnesses": [
                "Runtime Scenario",
                "Standard Protocol Scenario",
                "RLM Protocol Scenario",
                "Agent Scenario"
            ],
            "scenario_harness_summary": [
                {
                    "scenario_harness": "Runtime Scenario",
                    "scenarios": ["turn_checkpoint"],
                    "runs": 1
                },
                {
                    "scenario_harness": "Standard Protocol Scenario",
                    "scenarios": ["standard"],
                    "runs": 1
                },
                {
                    "scenario_harness": "RLM Protocol Scenario",
                    "scenarios": ["rlm"],
                    "runs": 1
                },
                {
                    "scenario_harness": "Agent Scenario",
                    "scenarios": ["rlm_process_handles"],
                    "runs": 1
                }
            ],
            "summary": [
                {
                    "scenario": "turn_checkpoint",
                    "scenario_harness": "Runtime Scenario",
                    "correctness_coverage_ids": [
                        "runtime_scenario_drains_command_before_turn_work_and_commits_checkpoint"
                    ]
                },
                {
                    "scenario": "standard",
                    "scenario_harness": "Standard Protocol Scenario",
                    "correctness_coverage_ids": [
                        "standard_protocol_scenario_projects_initial_request"
                    ]
                },
                {
                    "scenario": "rlm",
                    "scenario_harness": "RLM Protocol Scenario",
                    "correctness_coverage_ids": [
                        "rlm_protocol_scenario_prose_only_response_finishes_by_default"
                    ]
                },
                {
                    "scenario": "rlm_process_handles",
                    "scenario_harness": "Agent Scenario",
                    "correctness_coverage_ids": [
                        "agent_scenario_nested_process_start_await"
                    ]
                }
            ]
        });
        let output_projection = serde_json::json!({
            "scenario_harnesses": output_json["scenario_harnesses"].clone(),
            "scenario_harness_summary": output_json["scenario_harness_summary"]
                .as_array()
                .expect("scenario harness summary array")
                .iter()
                .map(|entry| serde_json::json!({
                    "scenario_harness": entry["scenario_harness"].clone(),
                    "scenarios": entry["scenarios"].clone(),
                    "runs": entry["runs"].clone(),
                }))
                .collect::<Vec<_>>(),
            "summary": output_json["summary"]
                .as_array()
                .expect("summary array")
                .iter()
                .map(|entry| serde_json::json!({
                    "scenario": entry["scenario"].clone(),
                    "scenario_harness": entry["scenario_harness"].clone(),
                    "correctness_coverage_ids": entry["correctness_coverage_ids"].clone(),
                }))
                .collect::<Vec<_>>(),
        });
        assert_eq!(output_projection, output_golden);
    }

    #[test]
    fn runtime_perf_ledger_keeps_the_summary_median_and_p95() {
        let scenario = RuntimePerfScenario::TurnCheckpoint;
        let stack_profile = stack_profile(2 * 1024 * 1024);
        let mut result = run_result(scenario, 10.0, 100);
        result
            .metric_samples
            .insert("process.cpu_ms".to_string(), vec![4.0, 6.0]);
        result
            .metric_samples
            .insert("runtime.worker_busy_ms".to_string(), vec![2.0, 3.0]);
        result
            .metric_samples
            .insert("process.cpu_utilization".to_string(), vec![0.5]);
        let summary = summarize(&[result], &[scenario], 1, &stack_profile)
            .into_iter()
            .next()
            .expect("summary exists");

        let records = duration_trend::records_for_run(
            std::slice::from_ref(&summary),
            "full",
            duration_trend::DurationTrendGeometry::current(1, 0, 1),
        );
        assert_eq!(records.len(), 1);
        let total = summary
            .stage_summary
            .get(stage::TOTAL)
            .expect("total stage summary");
        assert_eq!(records[0].total_ms, total.duration_ms.median);
        assert_eq!(records[0].total_p95_ms, Some(total.duration_ms.p95));
        assert_eq!(
            records[0].duration_metrics_ms["process.cpu_ms"].median_ms,
            5.0
        );
        assert!(
            records[0]
                .duration_metrics_ms
                .contains_key("runtime.worker_busy_ms")
        );
        assert!(
            !records[0]
                .duration_metrics_ms
                .contains_key("process.cpu_utilization")
        );
        assert_ne!(total.duration_ms.p95, 0.0);
    }

    #[test]
    fn skipped_and_measured_zero_stages_are_distinguished_in_records_and_summary() {
        let scenario = RuntimePerfScenario::TurnCheckpoint;
        let stack_profile = stack_profile(2 * 1024 * 1024);

        // A run that measured every stage, with `export_state` genuinely
        // measuring zero, next to a run that never reached `export_state`
        // at all.
        let mut measured_zero = run_result(scenario, 10.0, 100);
        measured_zero
            .stages
            .insert(stage::EXPORT_STATE.to_string(), stage_result(0.0, 0));
        let mut missing = run_result(scenario, 10.0, 100);
        missing.stages.remove(stage::EXPORT_STATE);

        let serialized = serde_json::to_value(&measured_zero).expect("run result serializes");
        assert!(serialized["stages"]["export_state"]["duration_ms"].is_f64());
        assert_eq!(serialized["stages"]["export_state"]["duration_ms"], 0.0);
        let serialized = serde_json::to_value(&missing).expect("run result serializes");
        assert!(serialized["stages"].get("export_state").is_none());

        let summary = summarize(&[measured_zero, missing], &[scenario], 1, &stack_profile)
            .into_iter()
            .next()
            .expect("summary exists");
        // Only the run that measured export_state contributes — the median of
        // one sample is that sample, not an average against a phantom zero.
        let export = summary
            .stage_summary
            .get(stage::EXPORT_STATE)
            .expect("export_state was measured on one run");
        assert_eq!(export.duration_ms.median, 0.0);
        assert_eq!(export.duration_ms.min, export.duration_ms.max);
    }

    #[test]
    fn skipped_result_emits_no_stage_records() {
        let skipped = skipped_runtime_perf_result(RuntimePerfScenario::StoreHardeningHotPaths, 1);
        assert!(skipped.stages.is_empty());
        assert!(skipped.turns.iter().all(|turn| turn.stages.is_empty()));

        let stack_profile = stack_profile(2 * 1024 * 1024);
        let scenario = RuntimePerfScenario::StoreHardeningHotPaths;
        let summary = summarize(&[skipped], &[scenario], 1, &stack_profile)
            .into_iter()
            .next()
            .expect("summary exists");
        assert!(summary.stage_summary.is_empty());
        // No `total` stage means no duration-trend record — a skipped run must
        // not write a fake 0.0 into the history.
        let records = duration_trend::records_for_run(
            std::slice::from_ref(&summary),
            "full",
            duration_trend::DurationTrendGeometry::current(1, 0, 1),
        );
        assert!(records.is_empty());
    }

    #[test]
    fn full_profile_history_uses_a_retained_sibling_of_the_report() {
        assert!(is_full_profile(5, 1, 12));
        assert!(!is_full_profile(2, 0, 3));
        assert_eq!(
            full_duration_history_path(Path::new(".benchmarks/perf-guard/runtime.json")),
            Path::new(".benchmarks/perf-guard/runtime-duration-history-full.jsonl")
        );
    }

    #[test]
    fn defaults_are_database_free_and_all_covers_every_runtime_path() {
        assert_eq!(
            resolve_scenarios(&[]).unwrap(),
            RuntimePerfScenario::DEFAULTS.to_vec()
        );
        assert!(
            !RuntimePerfScenario::DEFAULTS.contains(&RuntimePerfScenario::StoreHardeningHotPaths)
        );
        assert!(
            !RuntimePerfScenario::DEFAULTS.contains(&RuntimePerfScenario::WriterContention2Workers)
        );
        assert!(
            !RuntimePerfScenario::DEFAULTS
                .contains(&RuntimePerfScenario::AsyncProcessSettlement2Children)
        );
        assert_eq!(
            resolve_scenarios(&["all".to_string()]).unwrap(),
            RuntimePerfScenario::KNOWN.to_vec()
        );
        assert_eq!(
            resolve_scenarios(&["standard".to_string(), "all".to_string()])
                .unwrap()
                .len(),
            RuntimePerfScenario::KNOWN.len()
        );
    }

    #[test]
    fn named_metric_summary_preserves_signed_samples_for_percentiles() {
        let scenario = RuntimePerfScenario::WriterContention2Workers;
        let mut result = run_result(scenario, 1.0, 1);
        result.metric_samples_ms.insert(
            "writer_contention.same_session.wait_ms".to_string(),
            vec![-4.0, -2.0, 8.0, 16.0],
        );
        let summary = summarize(
            &[result],
            &[scenario],
            1,
            &StackProfile::capture(Some(2 * 1024 * 1024), None),
        );
        let wait = &summary[0].metric_summary_ms["writer_contention.same_session.wait_ms"];

        assert_eq!(wait.min, -4.0);
        assert_eq!(wait.p50, 3.0);
        assert_eq!(wait.p95, 14.8);
    }

    #[test]
    fn async_completion_scenarios_have_specific_guard_budgets() {
        assert!(
            allocation_budget_bytes(RuntimePerfScenario::RlmAsyncToolCompletion)
                < allocation_budget_bytes(RuntimePerfScenario::RlmProcessAsyncToolCompletion)
        );
        assert!(
            steady_state_turn_allocation_budget_bytes(RuntimePerfScenario::RlmAsyncToolCompletion)
                < steady_state_turn_allocation_budget_bytes(
                    RuntimePerfScenario::RlmProcessAsyncToolCompletion
                )
        );
        assert!(wall_clock_budget_ms(RuntimePerfScenario::RlmAsyncToolCompletion) < 10_000.0);
        assert!(
            wall_clock_budget_ms(RuntimePerfScenario::RlmProcessAsyncToolCompletion) < 10_000.0
        );
    }

    #[test]
    fn rlm_process_async_completion_requires_named_phase_metrics() {
        let phases = required_phases(RuntimePerfScenario::RlmProcessAsyncToolCompletion);
        for expected in [
            "rlm_lashlang.compile_link",
            "rlm_lashlang.store_module_artifact",
            "rlm_lashlang.execute",
            // `rlm_process.prepare_start` and `rlm_process.start` are not
            // listed: #1529 deleted the `start_process` execution-host method
            // they named when the TypeScript process special forms gave way to
            // the catalogue-rendered `processes.start`, so no code emits them.
            "rlm_process.await_handle",
            "rlm_process.load_artifact",
            "rlm_process.resolve_environment",
            "rlm_process.compile",
            "rlm_process.build_context",
            "rlm_process.execute",
            "rlm_process.shutdown",
        ] {
            assert!(
                phases.contains(&expected),
                "missing required phase {expected}"
            );
        }
    }

    #[test]
    fn rlm_trigger_mail_pipeline_has_specific_guard_budgets_and_phases() {
        let phases = required_phases(RuntimePerfScenario::RlmTriggerMailPipeline);
        for expected in [
            "rlm_lashlang.compile_link",
            "rlm_lashlang.store_module_artifact",
            "rlm_lashlang.execute",
            "trigger.occurrence_to_delivery",
        ] {
            assert!(
                phases.contains(&expected),
                "missing required phase {expected}"
            );
        }
        assert!(
            allocation_budget_bytes(RuntimePerfScenario::RlmTriggerMailPipeline) <= 400_000_000.0
        );
        assert!(
            steady_state_turn_allocation_budget_bytes(RuntimePerfScenario::RlmTriggerMailPipeline)
                <= 50_000_000.0
        );
        let total_budget_ms = wall_clock_budget_ms(RuntimePerfScenario::RlmTriggerMailPipeline);
        assert!(total_budget_ms.is_finite() && total_budget_ms > 0.0);
        assert!(
            phase_wall_clock_budget_ms(
                RuntimePerfScenario::RlmTriggerMailPipeline,
                "trigger.occurrence_to_delivery",
            )
            .is_some_and(|budget_ms| budget_ms > 0.0 && budget_ms < total_budget_ms)
        );
    }

    #[test]
    fn turn_lifecycle_hot_paths_have_specific_phase_budgets() {
        for (scenario, phase) in [
            (RuntimePerfScenario::TurnStartGate, "turn_cancel.start_gate"),
            (
                RuntimePerfScenario::TurnCancelRoundTrip,
                "turn_cancel.request_to_token_to_seal",
            ),
            (
                RuntimePerfScenario::IngressClaimProjection,
                "turn_input_ingress.enqueue_to_claim_to_projection",
            ),
        ] {
            assert!(required_phases(scenario).contains(&phase));
            let total_budget_ms = wall_clock_budget_ms(scenario);
            assert!(
                phase_wall_clock_budget_ms(scenario, phase)
                    .is_some_and(|budget_ms| budget_ms > 0.0 && budget_ms < total_budget_ms)
            );
        }
    }

    #[test]
    fn rlm_large_print_requires_projector_phase_metrics() {
        let phases = required_phases(RuntimePerfScenario::RlmLargePrint);
        for expected in [
            "rlm_lashlang.compile_link",
            "rlm_lashlang.execute",
            "rlm_lashlang.print_project",
        ] {
            assert!(
                phases.contains(&expected),
                "missing required phase {expected}"
            );
        }
        assert!(allocation_budget_bytes(RuntimePerfScenario::RlmLargePrint) <= 1_000_000_000.0);
        assert!(
            steady_state_turn_allocation_budget_bytes(RuntimePerfScenario::RlmLargePrint)
                <= 750_000_000.0
        );
    }

    #[test]
    fn rlm_oblique_stack_mix_requires_stack_sensitive_phase_metrics() {
        let phases = required_phases(RuntimePerfScenario::RlmObliqueStackMix);
        for expected in [
            "rlm_lashlang.compile_link",
            "rlm_lashlang.execute",
            "rlm_lashlang.print_project",
            // See the note above: #1529 retired the two process-start phases.
            "rlm_process.await_handle",
            "rlm_process.execute",
            "process.await_handle",
        ] {
            assert!(
                phases.contains(&expected),
                "missing required phase {expected}"
            );
        }
        assert!(
            allocation_budget_bytes(RuntimePerfScenario::RlmObliqueStackMix) <= 2_000_000_000.0
        );
        assert!(
            steady_state_turn_allocation_budget_bytes(RuntimePerfScenario::RlmObliqueStackMix)
                <= 170_000_000.0
        );
        assert!(wall_clock_budget_ms(RuntimePerfScenario::RlmObliqueStackMix) <= 20_000.0);
    }

    #[test]
    fn streamed_paired_lashlang_requires_lashlang_phase_metrics() {
        let phases = required_phases(RuntimePerfScenario::RlmStreamedPairedLashlang);
        for expected in ["rlm_lashlang.compile_link", "rlm_lashlang.execute"] {
            assert!(
                phases.contains(&expected),
                "missing required phase {expected}"
            );
        }
        assert!(
            allocation_budget_bytes(RuntimePerfScenario::RlmStreamedPairedLashlang)
                <= 128_000_000.0
        );
        assert!(
            steady_state_turn_allocation_budget_bytes(
                RuntimePerfScenario::RlmStreamedPairedLashlang
            ) <= 64_000_000.0
        );
    }

    #[test]
    fn openai_compat_stream_fixture_uses_chat_completions_sse_shape() {
        let profile = benchmark_stream_profile(RuntimePerfScenario::OpenAiCompatStream);
        let body = String::from_utf8(openai_compat_sse_body(&profile)).unwrap();
        assert!(body.contains(r#""object":"chat.completion.chunk""#));
        assert!(body.contains(r#""choices""#));
        assert!(body.contains(r#""delta":{"content":"#));
        assert!(body.contains(r#""usage":{"#));
        assert!(!body.contains(r#""type":"response.output_text.delta""#));
    }
}

#[cfg(test)]
#[path = "report_phase_tests.rs"]
mod phase_tests;
