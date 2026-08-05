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

use super::measurement::*;
use super::scenarios::{RuntimePerfScenario, ScenarioHarnessKind};

mod budgets;
use budgets::{
    allocation_budget_bytes, phase_wall_clock_budget_ms, process_list_run_allocation_budget_bytes,
    process_list_run_wall_clock_budget_ms, steady_state_turn_allocation_budget_bytes,
    wall_clock_budget_ms,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfReport {
    created_at: String,
    version: String,
    warmups: usize,
    runs: usize,
    chat_turns: usize,
    worker_stack_bytes: usize,
    stack_profile: StackProfile,
    scenarios: Vec<String>,
    scenario_harnesses: Vec<String>,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfBudgetResult {
    scenario: String,
    scenario_harness: String,
    metric: String,
    statistic: String,
    actual: Option<f64>,
    budget: Option<f64>,
    passed: bool,
    reason: Option<String>,
}

pub(crate) fn default_output_path() -> PathBuf {
    paths::default_report_path("runtime-perf")
}

#[allow(clippy::too_many_arguments)]
pub async fn run_cli(
    out: Option<PathBuf>,
    enable_dhat: bool,
    dhat_out: Option<PathBuf>,
    dhat_frames: Option<usize>,
    worker_stack_bytes: usize,
    runs: usize,
    warmups: usize,
    scenario_filters: Vec<String>,
    chat_turns: usize,
    enforce_budgets: bool,
    enforce_inventory: bool,
    version: &str,
) -> anyhow::Result<()> {
    if dhat_out.is_some() && !enable_dhat {
        anyhow::bail!("--runtime-perf-dhat-out requires --runtime-perf-dhat");
    }
    let scenarios = resolve_scenarios(&scenario_filters)?;
    let runs = runs.max(1);
    let chat_turns = chat_turns.max(1);
    let stack_profile = stack_profile(worker_stack_bytes);

    for _ in 0..warmups {
        for scenario in &scenarios {
            let _ = Box::pin(run_once(*scenario, chat_turns)).await?;
        }
    }

    let out_path = out.unwrap_or_else(default_output_path);
    report_support::ensure_parent_dir(&out_path, "benchmark output")?;
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
            let mut result = Box::pin(run_once(*scenario, chat_turns)).await?;
            result.stack_profile = Some(stack_profile.clone());
            results.push(result);
        }
    }
    dhat::finish_dhat_profiler(profiler);

    let summary = summarize(&results, &scenarios, chat_turns, &stack_profile);
    let scenario_harness_summary = summarize_scenario_harnesses(&results, &scenarios);
    let budget_results = evaluate_budgets(&summary, &scenarios);
    let report = RuntimePerfReport {
        created_at: Utc::now().to_rfc3339(),
        version: version.to_string(),
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
        dhat_out: dhat_out_path.clone(),
        summary,
        scenario_harness_summary,
        budget_results,
        results,
    };

    report_support::write_json_report(&out_path, &report)?;

    println!(
        "{}",
        serde_json::to_string_pretty(&runtime_perf_output_json(&out_path, &report))?
    );
    if enforce_budgets || enforce_inventory {
        let inventory_only = enforce_inventory && !enforce_budgets;
        let failures = report
            .budget_results
            .iter()
            .filter(|budget| !budget.passed)
            .filter(|budget| !inventory_only || is_inventory_result(budget))
            .map(|budget| {
                let actual = budget
                    .actual
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "missing".to_string());
                let budget_value = budget
                    .budget
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "required".to_string());
                let reason = budget.reason.as_deref().unwrap_or("budget exceeded");
                format!(
                    "{} {} {} actual={} budget={} ({reason})",
                    budget.scenario, budget.metric, budget.statistic, actual, budget_value
                )
            })
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            let label = if inventory_only {
                "Runtime perf inventory check failed"
            } else {
                "Runtime perf budget exceeded"
            };
            anyhow::bail!("{label}:\n{}", failures.join("\n"));
        }
    }
    Ok(())
}

/// Machine-independent inventory result (presence check or emitted phase with
/// no checked-in budget), as opposed to a release-calibrated ceiling
/// comparison. Presence checks carry `budget: Some(1.0)`, so the statistic
/// clause is load-bearing — `is_none()` alone would silently drop them.
fn is_inventory_result(result: &RuntimePerfBudgetResult) -> bool {
    result.statistic == "present" || result.budget.is_none()
}

fn runtime_perf_output_json(out_path: &Path, report: &RuntimePerfReport) -> serde_json::Value {
    serde_json::json!({
        "out": out_path,
        "dhat_out": report.dhat_out,
        "worker_stack_bytes": report.worker_stack_bytes,
        "stack_profile": report.stack_profile,
        "scenario_harnesses": report.scenario_harnesses,
        "summary": report.summary,
        "scenario_harness_summary": report.scenario_harness_summary,
        "budget_results": report.budget_results,
    })
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
                        .map(|result| result.total_ms)
                        .collect::<Vec<_>>(),
                ),
                total_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.total.bytes_allocated as f64)
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
                build_runtime_ms: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.build_runtime_ms)
                        .collect::<Vec<_>>(),
                ),
                seed_state_ms: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.seed_state_ms)
                        .collect::<Vec<_>>(),
                ),
                run_turn_ms: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.run_turn_ms)
                        .collect::<Vec<_>>(),
                ),
                await_background_work_ms: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.await_background_work_ms)
                        .collect::<Vec<_>>(),
                ),
                export_state_ms: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.export_state_ms)
                        .collect::<Vec<_>>(),
                ),
                total_ms: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.total_ms)
                        .collect::<Vec<_>>(),
                ),
                rss_after_export_kb: summarize_optional_metric(
                    matching
                        .iter()
                        .filter_map(|result| {
                            result.memory.rss_after_export_kb.map(|value| value as f64)
                        })
                        .collect::<Vec<_>>(),
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
                build_runtime_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.build_runtime.bytes_allocated as f64)
                        .collect::<Vec<_>>(),
                ),
                build_runtime_live_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.build_runtime.net_live_bytes as f64)
                        .collect::<Vec<_>>(),
                ),
                seed_state_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.seed_state.bytes_allocated as f64)
                        .collect::<Vec<_>>(),
                ),
                seed_state_live_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.seed_state.net_live_bytes as f64)
                        .collect::<Vec<_>>(),
                ),
                run_turn_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.run_turn.bytes_allocated as f64)
                        .collect::<Vec<_>>(),
                ),
                run_turn_live_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.run_turn.net_live_bytes as f64)
                        .collect::<Vec<_>>(),
                ),
                await_background_work_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| {
                            result.allocations.await_background_work.bytes_allocated as f64
                        })
                        .collect::<Vec<_>>(),
                ),
                await_background_work_live_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| {
                            result.allocations.await_background_work.net_live_bytes as f64
                        })
                        .collect::<Vec<_>>(),
                ),
                export_state_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.export_state.bytes_allocated as f64)
                        .collect::<Vec<_>>(),
                ),
                export_state_live_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.export_state.net_live_bytes as f64)
                        .collect::<Vec<_>>(),
                ),
                total_alloc_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.total.bytes_allocated as f64)
                        .collect::<Vec<_>>(),
                ),
                total_live_bytes: summarize_metric(
                    matching
                        .iter()
                        .map(|result| result.allocations.total.net_live_bytes as f64)
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
            })
        })
        .collect()
}

fn evaluate_budgets(
    summaries: &[RuntimePerfScenarioSummary],
    scenarios: &[RuntimePerfScenario],
) -> Vec<RuntimePerfBudgetResult> {
    let mut budgets = Vec::new();
    for scenario in scenarios {
        let Some(summary) = summaries
            .iter()
            .find(|summary| summary.scenario == scenario.name())
        else {
            budgets.push(RuntimePerfBudgetResult {
                scenario: scenario.name().to_string(),
                scenario_harness: scenario.scenario_harness().name().to_string(),
                metric: "scenario_output".to_string(),
                statistic: "present".to_string(),
                actual: None,
                budget: Some(1.0),
                passed: false,
                reason: Some("scenario did not produce a summary".to_string()),
            });
            continue;
        };

        for phase in required_phases(*scenario) {
            let passed = summary.phase_summary.contains_key(*phase);
            budgets.push(RuntimePerfBudgetResult {
                scenario: scenario.name().to_string(),
                scenario_harness: scenario.scenario_harness().name().to_string(),
                metric: format!("phase:{phase}"),
                statistic: "present".to_string(),
                actual: if passed { Some(1.0) } else { None },
                budget: Some(1.0),
                passed,
                reason: (!passed).then(|| "required phase metrics were missing".to_string()),
            });
        }

        for (phase, metrics) in &summary.phase_summary {
            let Some(budget_ms) = phase_wall_clock_budget_ms(*scenario, phase) else {
                budgets.push(RuntimePerfBudgetResult {
                    scenario: summary.scenario.clone(),
                    scenario_harness: summary.scenario_harness.clone(),
                    metric: format!("phase:{phase}:duration_ms"),
                    statistic: "median".to_string(),
                    actual: Some(metrics.duration_ms.median),
                    budget: None,
                    passed: false,
                    reason: Some("emitted phase has no checked-in wall-clock budget".to_string()),
                });
                continue;
            };
            push_max_budget(
                &mut budgets,
                summary,
                &format!("phase:{phase}:duration_ms"),
                "median",
                metrics.duration_ms.median,
                budget_ms,
            );
        }

        if *scenario == RuntimePerfScenario::ProcessListStress {
            // The 1,537-row fixture is populated through TestLocalProcessRegistry,
            // whose test-only map-clone writes are quadratic. Keep that separately
            // reported setup span out of the product-facing release guard.
            push_max_budget(
                &mut budgets,
                summary,
                "run_turn_alloc_bytes",
                "median",
                summary.run_turn_alloc_bytes.median,
                process_list_run_allocation_budget_bytes(),
            );
        } else {
            push_max_budget(
                &mut budgets,
                summary,
                "total_alloc_bytes",
                "median",
                summary.total_alloc_bytes.median,
                allocation_budget_bytes(*scenario),
            );
        }
        push_max_budget(
            &mut budgets,
            summary,
            "steady_state_turn_alloc_bytes",
            "median",
            steady_state_turn_alloc_bytes(summary),
            steady_state_turn_allocation_budget_bytes(*scenario),
        );
        if *scenario == RuntimePerfScenario::ProcessListStress {
            push_max_budget(
                &mut budgets,
                summary,
                "run_turn_ms",
                "median",
                summary.run_turn_ms.median,
                process_list_run_wall_clock_budget_ms(),
            );
        } else {
            push_max_budget(
                &mut budgets,
                summary,
                "total_ms",
                "median",
                summary.total_ms.median,
                wall_clock_budget_ms(*scenario),
            );
        }
    }
    budgets
}

fn push_max_budget(
    budgets: &mut Vec<RuntimePerfBudgetResult>,
    summary: &RuntimePerfScenarioSummary,
    metric: &str,
    statistic: &str,
    actual: f64,
    budget: f64,
) {
    budgets.push(RuntimePerfBudgetResult {
        scenario: summary.scenario.clone(),
        scenario_harness: summary.scenario_harness.clone(),
        metric: metric.to_string(),
        statistic: statistic.to_string(),
        actual: Some(actual),
        budget: Some(budget),
        passed: actual <= budget,
        reason: (actual > budget).then(|| "metric exceeded checked-in guard budget".to_string()),
    });
}

fn required_phases(scenario: RuntimePerfScenario) -> &'static [&'static str] {
    match scenario {
        RuntimePerfScenario::OpenAiResponsesSseParse => &[
            "openai_responses_sse_parse.parse_payload",
            "openai_responses_sse_parse.project_parts",
        ],
        RuntimePerfScenario::DirectLlmClient => &["direct_llm_client.complete"],
        RuntimePerfScenario::ProcessListStress => &[
            "process_list_stress.list_live",
            "process_list_stress.list_all",
            "process_list_stress.list_global",
            "process_list_stress.signal_append",
            "process_list_stress.wait_roundtrip",
            "process_list_stress.env_spec_hash",
            "process_list_stress.render_live_json",
            "process_list_stress.render_all_json",
        ],
        RuntimePerfScenario::QueuedWorkClaimStress => &[
            "queued_work.claim_session_lease",
            "queued_work.enqueue_mixed_batch",
            "queued_work.claim_session_command",
            "queued_work.complete_session_command",
            "queued_work.claim_join_turn_work",
            "queued_work.abandon_join_claim",
            "queued_work.reclaim_by_batch_ids",
            "queued_work.complete_join_turn_work",
            "queued_work.claim_exclusive_turn_work",
            "queued_work.complete_exclusive_turn_work",
            "queued_work.list_pending",
        ],
        RuntimePerfScenario::TurnInputIngressInterrupt => &[
            "turn_input_ingress.claim_session_lease",
            "turn_input_ingress.enqueue_active",
            "turn_input_ingress.enqueue_next",
            "turn_input_ingress.claim_active_initial",
            "turn_input_ingress.abandon_active_claim",
            "turn_input_ingress.reclaim_active_inputs",
            "turn_input_ingress.complete_active_and_defer",
            "turn_input_ingress.claim_next_turn_inputs",
            "turn_input_ingress.abandon_next_claim",
            "turn_input_ingress.reclaim_next_turn_inputs",
            "turn_input_ingress.complete_next_turn_inputs",
            "turn_input_ingress.list_pending",
        ],
        RuntimePerfScenario::DeepTurnComposition => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "rlm_lashlang.compile_link",
            "rlm_lashlang.execute",
            "rlm_process.start",
            "rlm_process.await_handle",
            "process.start_child",
            "process.await_handle",
        ],
        RuntimePerfScenario::TurnStartGate => &["turn_cancel.start_gate"],
        RuntimePerfScenario::TurnCancelRoundTrip => &[
            "turn_cancel.start_gate",
            "turn_cancel.request_to_token_to_seal",
        ],
        RuntimePerfScenario::IngressClaimProjection => &[
            "turn_cancel.start_gate",
            "turn_input_ingress.enqueue_to_claim_to_projection",
            "rlm_lashlang.execute",
        ],
        RuntimePerfScenario::TurnCheckpoint => &[
            "standard_llm_checkpoint",
            "standard_parallel_tools_checkpoint",
            "rlm_exec_checkpoint",
        ],
        RuntimePerfScenario::LiveReplayPressure => &[
            "live_replay.append",
            "live_replay.current_cursor_parse",
            "live_replay.replay_after_cursor",
            "live_replay.subscribe_buffered",
            "live_replay.trim_by_capacity",
            "live_replay.gap_handling",
        ],
        RuntimePerfScenario::StoreHardeningHotPaths => &[
            "store_hardening.identity.process_registration",
            "store_hardening.identity.trigger_occurrence",
            "store_hardening.identity.usage_delta",
            "store_hardening.postgres.open_preverified",
            "store_hardening.postgres.open_enforce",
            "store_hardening.memory.claim_session_lease",
            "store_hardening.memory.claim_queued_work",
            "store_hardening.memory.complete_queued_work",
            "store_hardening.memory.attachment_intent",
            "store_hardening.memory.attachment_adopt",
            "store_hardening.memory.append_receipt_usage_fresh",
            "store_hardening.memory.append_receipt_replay",
            "store_hardening.sqlite.claim_session_lease",
            "store_hardening.sqlite.claim_queued_work",
            "store_hardening.sqlite.complete_queued_work",
            "store_hardening.sqlite.attachment_intent",
            "store_hardening.sqlite.attachment_adopt",
            "store_hardening.sqlite.append_receipt_usage_fresh",
            "store_hardening.sqlite.append_receipt_replay",
            "store_hardening.postgres.claim_session_lease",
            "store_hardening.postgres.claim_queued_work",
            "store_hardening.postgres.complete_queued_work",
            "store_hardening.postgres.attachment_intent",
            "store_hardening.postgres.attachment_adopt",
            "store_hardening.postgres.append_receipt_usage_fresh",
            "store_hardening.postgres.append_receipt_replay",
            "store_hardening.memory.prune_terminal_processes",
            "store_hardening.sqlite.prune_terminal_processes",
            "store_hardening.postgres.prune_terminal_processes",
        ],
        RuntimePerfScenario::Rlm => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "plugin_hook.context_transform.rlm_protocol",
            "rlm_lashlang.rehydrate_projected_globals",
            "rlm_lashlang.resolve_projected_bindings",
        ],
        RuntimePerfScenario::Standard => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "plugin_hook.context_transform.rolling_history",
        ],
        RuntimePerfScenario::StoreReopen => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "store_reopen.persisted_load",
            "store_reopen.runtime_hydration",
            "store_reopen.store_factory_create",
        ],
        RuntimePerfScenario::SqliteStoreReopen => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "sqlite_store_reopen.runtime_reopen",
        ],
        RuntimePerfScenario::TraceJsonlExtended => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "trace_jsonl.inspect_files",
        ],
        RuntimePerfScenario::RlmAsyncToolCompletion => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "rlm_lashlang.compile_link",
            "rlm_lashlang.execute",
        ],
        RuntimePerfScenario::RlmTriggerMailPipeline => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "rlm_lashlang.compile_link",
            "rlm_lashlang.store_module_artifact",
            "rlm_lashlang.execute",
            "trigger.occurrence_to_delivery",
        ],
        RuntimePerfScenario::RlmLargePrint => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "rlm_lashlang.compile_link",
            "rlm_lashlang.execute",
            "rlm_lashlang.print_project",
        ],
        RuntimePerfScenario::RlmObliqueStackMix => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "rlm_lashlang.compile_link",
            "rlm_lashlang.execute",
            "rlm_lashlang.print_project",
            "rlm_process.prepare_start",
            "rlm_process.start",
            "rlm_process.await_handle",
            "process.await_handle",
            "rlm_process.load_artifact",
            "rlm_process.resolve_environment",
            "rlm_process.compile",
            "rlm_process.build_context",
            "rlm_process.execute",
            "rlm_process.shutdown",
        ],
        RuntimePerfScenario::RlmStreamedPairedLashlang => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "rlm_lashlang.compile_link",
            "rlm_lashlang.execute",
        ],
        RuntimePerfScenario::RlmProcessHandles
        | RuntimePerfScenario::RlmProcessAsyncToolCompletion => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
            "rlm_lashlang.compile_link",
            "rlm_lashlang.store_module_artifact",
            "rlm_lashlang.execute",
            "rlm_process.prepare_start",
            "rlm_process.start",
            "rlm_process.await_handle",
            "rlm_process.load_artifact",
            "rlm_process.resolve_environment",
            "rlm_process.compile",
            "rlm_process.build_context",
            "rlm_process.execute",
            "rlm_process.shutdown",
        ],
        RuntimePerfScenario::EmbedStandard | RuntimePerfScenario::EmbedRlm => &[],
        _ => &[
            "context_transform",
            "before_turn_hooks",
            "prompt_build",
            "effect_loop",
            "prepared_turn",
            "committed_turn",
            "post_commit_delivery",
        ],
    }
}

fn steady_state_turn_alloc_bytes(summary: &RuntimePerfScenarioSummary) -> f64 {
    summary
        .steady_state_turn
        .as_ref()
        .unwrap_or(&summary.last_turn)
        .total_alloc_bytes
        .median
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

fn summarize_turn_group(turns: &[RuntimePerfTurnResult]) -> RuntimePerfTurnSummary {
    RuntimePerfTurnSummary {
        total_ms: summarize_metric(turns.iter().map(|turn| turn.total_ms).collect()),
        run_turn_ms: summarize_metric(turns.iter().map(|turn| turn.run_turn_ms).collect()),
        await_background_work_ms: summarize_metric(
            turns
                .iter()
                .map(|turn| turn.await_background_work_ms)
                .collect(),
        ),
        rss_growth_kb: summarize_optional_metric(
            turns
                .iter()
                .filter_map(|turn| turn.memory.rss_growth_kb.map(|value| value as f64))
                .collect(),
        ),
        total_alloc_bytes: summarize_metric(
            turns
                .iter()
                .map(|turn| turn.allocations.total.bytes_allocated as f64)
                .collect(),
        ),
        total_live_bytes: summarize_metric(
            turns
                .iter()
                .map(|turn| turn.allocations.total.net_live_bytes as f64)
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

    let count = turns.len() as f64;
    Some(RuntimePerfTurnResult {
        turn_index: turns[0].turn_index,
        run_turn_ms: round3(turns.iter().map(|turn| turn.run_turn_ms).sum::<f64>() / count),
        await_background_work_ms: round3(
            turns
                .iter()
                .map(|turn| turn.await_background_work_ms)
                .sum::<f64>()
                / count,
        ),
        total_ms: round3(turns.iter().map(|turn| turn.total_ms).sum::<f64>() / count),
        memory: RuntimePerfTurnMemoryRunResult {
            rss_before_kb: None,
            rss_after_turn_kb: None,
            rss_after_await_kb: None,
            peak_hwm_before_kb: None,
            peak_hwm_after_await_kb: None,
            rss_growth_kb: mean_option_i64(turns.iter().map(|turn| turn.memory.rss_growth_kb)),
            hwm_growth_kb: mean_option_i64(turns.iter().map(|turn| turn.memory.hwm_growth_kb)),
        },
        allocations: RuntimePerfTurnAllocationRunResult {
            run_turn: mean_allocation_delta(turns.iter().map(|turn| &turn.allocations.run_turn)),
            await_background_work: mean_allocation_delta(
                turns
                    .iter()
                    .map(|turn| &turn.allocations.await_background_work),
            ),
            total: mean_allocation_delta(turns.iter().map(|turn| &turn.allocations.total)),
        },
        phase_profile: mean_phase_profiles(turns.iter().map(|turn| &turn.phase_profile)),
        turn_usage: mean_token_usage(turns.iter().map(|turn| &turn.turn_usage)),
        usage_delta: SessionUsageReport::default(),
        cumulative_usage: SessionUsageReport::default(),
    })
}
fn summarize_metric(values: Vec<f64>) -> RuntimePerfMetricSummary {
    basic_summary(values)
}

fn summarize_optional_metric(values: Vec<f64>) -> Option<RuntimePerfMetricSummary> {
    optional_basic_summary(values)
}
#[cfg(test)]
mod tests {
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

    fn allocation_run(bytes_allocated: usize) -> RuntimePerfAllocationRunResult {
        RuntimePerfAllocationRunResult {
            build_runtime: allocation_delta(1),
            seed_state: allocation_delta(2),
            run_turn: allocation_delta(bytes_allocated / 2),
            await_background_work: allocation_delta(bytes_allocated / 4),
            export_state: allocation_delta(3),
            total: allocation_delta(bytes_allocated),
        }
    }

    fn memory_run() -> RuntimePerfMemoryRunResult {
        RuntimePerfMemoryRunResult {
            rss_before_kb: None,
            rss_after_build_kb: None,
            rss_after_seed_kb: None,
            rss_after_turn_kb: None,
            rss_after_await_kb: None,
            rss_after_export_kb: None,
            peak_hwm_before_kb: None,
            peak_hwm_after_export_kb: None,
            rss_growth_kb: None,
            hwm_growth_kb: None,
        }
    }

    fn turn_memory_run() -> RuntimePerfTurnMemoryRunResult {
        RuntimePerfTurnMemoryRunResult {
            rss_before_kb: None,
            rss_after_turn_kb: None,
            rss_after_await_kb: None,
            peak_hwm_before_kb: None,
            peak_hwm_after_await_kb: None,
            rss_growth_kb: None,
            hwm_growth_kb: None,
        }
    }

    fn turn_result(total_ms: f64, bytes_allocated: usize) -> RuntimePerfTurnResult {
        RuntimePerfTurnResult {
            turn_index: 0,
            run_turn_ms: total_ms / 2.0,
            await_background_work_ms: total_ms / 4.0,
            total_ms,
            memory: turn_memory_run(),
            allocations: RuntimePerfTurnAllocationRunResult {
                run_turn: allocation_delta(bytes_allocated / 2),
                await_background_work: allocation_delta(bytes_allocated / 4),
                total: allocation_delta(bytes_allocated),
            },
            phase_profile: BTreeMap::new(),
            turn_usage: lash_core::TokenUsage::default(),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        }
    }

    fn run_result(
        scenario: RuntimePerfScenario,
        total_ms: f64,
        bytes_allocated: usize,
    ) -> RuntimePerfRunResult {
        RuntimePerfRunResult {
            scenario: scenario.name().to_string(),
            scenario_harness: scenario.scenario_harness().name().to_string(),
            chat_turns: 1,
            stack_profile: None,
            build_runtime_ms: 1.0,
            seed_state_ms: 1.0,
            run_turn_ms: total_ms / 2.0,
            await_background_work_ms: total_ms / 4.0,
            export_state_ms: 1.0,
            total_ms,
            session_nodes: 1,
            active_path_messages: 1,
            extra_counters: BTreeMap::new(),
            memory: memory_run(),
            allocations: allocation_run(bytes_allocated),
            phase_profile: BTreeMap::new(),
            turns: vec![turn_result(total_ms, bytes_allocated)],
            cumulative_usage: SessionUsageReport::default(),
        }
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
            dhat_out: None,
            results,
            summary,
            scenario_harness_summary,
            budget_results: Vec::new(),
        };

        let report_json = serde_json::to_value(&report).expect("report serializes");
        assert_eq!(
            report_json["scenario_harnesses"],
            serde_json::json!([
                "Runtime Scenario",
                "Standard Protocol Scenario",
                "RLM Protocol Scenario",
                "Agent Scenario"
            ])
        );
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
    fn defaults_are_database_free_and_all_covers_every_runtime_path() {
        assert_eq!(
            resolve_scenarios(&[]).unwrap(),
            RuntimePerfScenario::DEFAULTS.to_vec()
        );
        assert!(
            !RuntimePerfScenario::DEFAULTS.contains(&RuntimePerfScenario::StoreHardeningHotPaths)
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
    fn async_completion_scenarios_have_specific_guard_budgets() {
        assert_eq!(
            allocation_budget_bytes(RuntimePerfScenario::RlmAsyncToolCompletion),
            145_000_000.0
        );
        assert_eq!(
            steady_state_turn_allocation_budget_bytes(RuntimePerfScenario::RlmAsyncToolCompletion),
            11_000_000.0
        );
        assert_eq!(
            allocation_budget_bytes(RuntimePerfScenario::RlmProcessAsyncToolCompletion),
            290_000_000.0
        );
        assert_eq!(
            steady_state_turn_allocation_budget_bytes(
                RuntimePerfScenario::RlmProcessAsyncToolCompletion
            ),
            23_000_000.0
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
            "rlm_process.prepare_start",
            "rlm_process.start",
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
        assert_eq!(
            allocation_budget_bytes(RuntimePerfScenario::RlmTriggerMailPipeline),
            260_000_000.0
        );
        assert!(
            steady_state_turn_allocation_budget_bytes(RuntimePerfScenario::RlmTriggerMailPipeline)
                <= 20_000_000.0
        );
        assert_eq!(
            wall_clock_budget_ms(RuntimePerfScenario::RlmTriggerMailPipeline),
            520.0
        );
        assert_eq!(
            phase_wall_clock_budget_ms(
                RuntimePerfScenario::RlmTriggerMailPipeline,
                "trigger.occurrence_to_delivery",
            ),
            Some(55.0)
        );
    }

    #[test]
    fn turn_lifecycle_hot_paths_have_specific_phase_budgets() {
        for (scenario, phase, budget_ms) in [
            (
                RuntimePerfScenario::TurnStartGate,
                "turn_cancel.start_gate",
                1_200.0,
            ),
            (
                RuntimePerfScenario::TurnCancelRoundTrip,
                "turn_cancel.request_to_token_to_seal",
                60.0,
            ),
            (
                RuntimePerfScenario::IngressClaimProjection,
                "turn_input_ingress.enqueue_to_claim_to_projection",
                195.0,
            ),
        ] {
            assert!(required_phases(scenario).contains(&phase));
            assert_eq!(phase_wall_clock_budget_ms(scenario, phase), Some(budget_ms));
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
            "rlm_process.prepare_start",
            "rlm_process.start",
            "rlm_process.await_handle",
            "rlm_process.execute",
            "process.await_handle",
        ] {
            assert!(
                phases.contains(&expected),
                "missing required phase {expected}"
            );
        }
        assert_eq!(
            allocation_budget_bytes(RuntimePerfScenario::RlmObliqueStackMix),
            1_950_000_000.0
        );
        assert_eq!(
            steady_state_turn_allocation_budget_bytes(RuntimePerfScenario::RlmObliqueStackMix),
            165_000_000.0
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
