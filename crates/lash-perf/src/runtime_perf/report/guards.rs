//! Perf-guard budget evaluation: which measurements are compared against a
//! checked-in ceiling, and what a failed comparison is allowed to do.

use serde::Serialize;

use super::budgets::{
    allocation_budget_bytes, phase_wall_clock_budget_ms, process_list_run_allocation_budget_bytes,
    process_list_run_wall_clock_budget_ms, steady_state_turn_allocation_budget_bytes,
    wall_clock_budget_ms,
};
use super::{RuntimePerfScenario, RuntimePerfScenarioSummary};

/// What a guard result is allowed to do when it fails.
///
/// The class is a property of the metric, decided once here rather than
/// re-derived from metric-name spelling by every consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimePerfGuardClass {
    /// Machine-independent inventory: a required phase must be emitted, and an
    /// emitted phase must carry a checked-in budget. Identical on every
    /// machine, so it gates even the PR-time smoke run.
    Inventory,
    /// Allocation ceiling. Load-independent (within 0.03% across runs on a
    /// runner loaded enough to move wall-clock by 30x), so it gates the
    /// release.
    Allocation,
    /// Wall-clock ceiling. Advisory only: on shared runners the same binary
    /// measured 7.67 ms then 1.625 ms against a 0.25 ms phase ceiling, and no
    /// duration-keeping remedy survives that (wide margins detect nothing,
    /// median-of-N still misses under sustained load, isolated hardware does
    /// not exist here). A flaky hard gate trains reruns, which is worse than
    /// no gate — so these report and never fail.
    Duration,
}

impl RuntimePerfGuardClass {
    /// Every class ever defined, so the enforcement rules can be proven
    /// exhaustively rather than sampled.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::Inventory, Self::Allocation, Self::Duration];

    /// Advisory results are reported but never turn into a non-zero exit.
    /// Written as an exhaustive match on purpose: a new guard class must
    /// declare whether it gates instead of silently inheriting "enforced".
    fn is_advisory(self) -> bool {
        match self {
            Self::Inventory | Self::Allocation => false,
            Self::Duration => true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfBudgetResult {
    scenario: String,
    scenario_harness: String,
    metric: String,
    statistic: String,
    class: RuntimePerfGuardClass,
    actual: Option<f64>,
    budget: Option<f64>,
    passed: bool,
    reason: Option<String>,
}

impl RuntimePerfBudgetResult {
    pub(super) fn describe(&self) -> String {
        let actual = self
            .actual
            .map(|value| value.to_string())
            .unwrap_or_else(|| "missing".to_string());
        let budget = self
            .budget
            .map(|value| value.to_string())
            .unwrap_or_else(|| "required".to_string());
        let reason = self.reason.as_deref().unwrap_or("budget exceeded");
        format!(
            "{} {} {} actual={} budget={} ({reason})",
            self.scenario, self.metric, self.statistic, actual, budget
        )
    }
}

/// The failures that must fail the process, already rendered for the error.
/// Empty means the enforced classes all held — advisory exceedances live in
/// [`report_advisory_exceedances`] and never reach here.
pub(super) fn enforcement_failures(
    results: &[RuntimePerfBudgetResult],
    inventory_only: bool,
) -> Vec<String> {
    results
        .iter()
        .filter(|result| gates_run(result, inventory_only))
        .map(RuntimePerfBudgetResult::describe)
        .collect()
}

/// Whether a failed guard result is allowed to fail the process. Advisory
/// (wall-clock) results never are; `--enforce-inventory` narrows the rest to
/// the machine-independent inventory class.
pub(super) fn gates_run(result: &RuntimePerfBudgetResult, inventory_only: bool) -> bool {
    if result.passed || result.class.is_advisory() {
        return false;
    }
    !inventory_only || result.class == RuntimePerfGuardClass::Inventory
}

/// Wall-clock exceedances never fail the run, so the only way they stay
/// actionable is being printed every time, with the measured value next to
/// the budget it missed.
pub(super) fn report_advisory_exceedances(results: &[RuntimePerfBudgetResult]) {
    let advisories = results
        .iter()
        .filter(|result| !result.passed && result.class.is_advisory())
        .map(RuntimePerfBudgetResult::describe)
        .collect::<Vec<_>>();
    if advisories.is_empty() {
        return;
    }
    eprintln!(
        "warning: runtime perf advisory wall-clock budgets exceeded (reported, not gating):\n{}",
        advisories.join("\n")
    );
}

pub(super) fn evaluate_budgets(
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
                class: RuntimePerfGuardClass::Inventory,
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
                class: RuntimePerfGuardClass::Inventory,
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
                    // The ceiling itself is advisory, but its presence is
                    // machine-independent: an emitted phase nobody budgeted is
                    // an unreviewed measurement surface, and that still gates.
                    class: RuntimePerfGuardClass::Inventory,
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
                RuntimePerfGuardClass::Duration,
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
                RuntimePerfGuardClass::Allocation,
                summary.run_turn_alloc_bytes.median,
                process_list_run_allocation_budget_bytes(),
            );
        } else {
            push_max_budget(
                &mut budgets,
                summary,
                "total_alloc_bytes",
                "median",
                RuntimePerfGuardClass::Allocation,
                summary.total_alloc_bytes.median,
                allocation_budget_bytes(*scenario),
            );
        }
        push_max_budget(
            &mut budgets,
            summary,
            "steady_state_turn_alloc_bytes",
            "median",
            RuntimePerfGuardClass::Allocation,
            steady_state_turn_alloc_bytes(summary),
            steady_state_turn_allocation_budget_bytes(*scenario),
        );
        if *scenario == RuntimePerfScenario::ProcessListStress {
            push_max_budget(
                &mut budgets,
                summary,
                "run_turn_ms",
                "median",
                RuntimePerfGuardClass::Duration,
                summary.run_turn_ms.median,
                process_list_run_wall_clock_budget_ms(),
            );
        } else {
            push_max_budget(
                &mut budgets,
                summary,
                "total_ms",
                "median",
                RuntimePerfGuardClass::Duration,
                summary.total_ms.median,
                wall_clock_budget_ms(*scenario),
            );
        }
    }
    budgets
}

pub(super) fn push_max_budget(
    budgets: &mut Vec<RuntimePerfBudgetResult>,
    summary: &RuntimePerfScenarioSummary,
    metric: &str,
    statistic: &str,
    class: RuntimePerfGuardClass,
    actual: f64,
    budget: f64,
) {
    let exceeded = actual > budget;
    budgets.push(RuntimePerfBudgetResult {
        scenario: summary.scenario.clone(),
        scenario_harness: summary.scenario_harness.clone(),
        metric: metric.to_string(),
        statistic: statistic.to_string(),
        class,
        actual: Some(actual),
        budget: Some(budget),
        passed: !exceeded,
        reason: exceeded.then(|| {
            if class.is_advisory() {
                "metric exceeded advisory guard budget (reported, not gating)".to_string()
            } else {
                "metric exceeded checked-in guard budget".to_string()
            }
        }),
    });
}

pub(super) fn required_phases(scenario: RuntimePerfScenario) -> &'static [&'static str] {
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
        RuntimePerfScenario::CheckpointStateHotPaths => &[
            "checkpoint_state.initial_capture",
            "checkpoint_state.dirty_binding_update",
            "checkpoint_state.incremental_capture",
            "checkpoint_state.measure_budget",
            "checkpoint_state.component_commit",
            "checkpoint_state.component_load",
            "checkpoint_state.execution_restore",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_perf::report::{stack_profile, summarize, tests::run_result};

    fn guard_result(class: RuntimePerfGuardClass, passed: bool) -> RuntimePerfBudgetResult {
        RuntimePerfBudgetResult {
            scenario: "s".to_string(),
            scenario_harness: "h".to_string(),
            metric: "m".to_string(),
            statistic: "median".to_string(),
            class,
            actual: Some(7.67),
            budget: Some(0.25),
            passed,
            reason: None,
        }
    }

    #[test]
    fn every_guard_class_declares_its_enforcement_under_both_flag_modes() {
        use RuntimePerfGuardClass::{Allocation, Duration, Inventory};

        for class in RuntimePerfGuardClass::ALL {
            assert!(
                !gates_run(&guard_result(class, true), false),
                "{class:?}: a passing result never fails the run"
            );
            assert!(
                !gates_run(&guard_result(class, true), true),
                "{class:?}: a passing result never fails the inventory run"
            );
        }

        // Inventory checks are machine-independent, so they gate both the
        // PR-time inventory run and the release budget run.
        assert!(gates_run(&guard_result(Inventory, false), true));
        assert!(gates_run(&guard_result(Inventory, false), false));
        // Allocation ceilings are release-calibrated: enforced by
        // --enforce-budgets, skipped by --enforce-inventory.
        assert!(!gates_run(&guard_result(Allocation, false), true));
        assert!(gates_run(&guard_result(Allocation, false), false));
        // Wall-clock ceilings are advisory in every mode (FIG-1385).
        assert!(!gates_run(&guard_result(Duration, false), true));
        assert!(!gates_run(&guard_result(Duration, false), false));
    }

    #[test]
    fn advisory_exceedances_carry_the_measurement_next_to_the_budget() {
        let described = guard_result(RuntimePerfGuardClass::Duration, false).describe();

        assert!(described.contains("actual=7.67"), "{described}");
        assert!(described.contains("budget=0.25"), "{described}");
    }

    #[test]
    fn exceeded_wall_clock_budgets_stay_visible_as_failed_advisory_results() {
        let scenario = RuntimePerfScenario::DeepTurnComposition;
        let results = vec![run_result(scenario, 10.0, 100)];
        let stack_profile = stack_profile(2 * 1024 * 1024);
        let summaries = summarize(&results, &[scenario], 1, &stack_profile);
        let summary = summaries.first().expect("one scenario summary");
        let mut budgets = Vec::new();

        push_max_budget(
            &mut budgets,
            summary,
            "phase:rlm_process.load_artifact:duration_ms",
            "median",
            RuntimePerfGuardClass::Duration,
            7.67,
            0.25,
        );

        let result = budgets.first().expect("one advisory result");
        assert!(!result.passed);
        assert_eq!(result.class, RuntimePerfGuardClass::Duration);
        assert_eq!(
            result.reason.as_deref(),
            Some("metric exceeded advisory guard budget (reported, not gating)")
        );
    }
}
