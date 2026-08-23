use std::collections::BTreeSet;

use super::budgets::{
    assert_complete_runtime_budget, configured_phase_names, configured_scenario_names,
    phase_wall_clock_budget_ms,
};
use super::guards::required_phases;
use super::{RuntimePerfScenario, ScenarioHarnessKind};
use crate::perf_support::stack::StackProfile;
use crate::runtime_perf::measurement::{
    HighTrafficConfig, RuntimePerfPhaseProbe, phase_name, run_once,
};
use crate::runtime_perf::scenarios::DURABLE_CHECKPOINT_CURVE_BYTES;
use lash_core::runtime::RuntimeTurnPhaseProbe;

fn high_traffic_config() -> HighTrafficConfig {
    HighTrafficConfig::parse(
        4,
        0,
        "plain=1,tool=1,queued=1,child=1,wake=1,trigger=1",
        "2,4",
        1.25,
    )
    .expect("valid high-traffic test config")
}

#[test]
fn typed_phase_nesting_records_each_open_span() {
    let probe = RuntimePerfPhaseProbe::default();
    let phase = lash_core::runtime::RuntimeTurnPhase::EffectLoop;

    probe.begin(phase);
    std::thread::sleep(std::time::Duration::from_millis(5));
    probe.begin(phase);
    std::thread::sleep(std::time::Duration::from_millis(5));
    probe.end(phase);
    std::thread::sleep(std::time::Duration::from_millis(5));
    probe.end(phase);

    let completed = probe.take_completed();
    let result = completed
        .get(phase_name(phase))
        .expect("nested typed phase should be recorded");
    assert_eq!(result.samples, 2);
    assert!(
        result.duration_ms > 10.0,
        "nested typed spans should contribute both durations: {result:?}"
    );
}

#[test]
fn typed_and_named_phase_paths_share_one_completion_key() {
    let probe = RuntimePerfPhaseProbe::default();
    let phase = lash_core::runtime::RuntimeTurnPhase::EffectLoop;
    let name = phase_name(phase);

    probe.begin(phase);
    probe.end(phase);
    probe.begin_named(name);
    probe.end_named(name);

    let completed = probe.take_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed.get(name).expect("shared phase name").samples, 2);
}

#[test]
fn ending_an_unstarted_phase_is_a_no_op() {
    let probe = RuntimePerfPhaseProbe::default();

    probe.end(lash_core::runtime::RuntimeTurnPhase::EffectLoop);
    probe.end_named("unstarted");

    assert!(probe.take_completed().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_sqlite_scenarios_report_phases_and_store_calls() {
    for scenario in [
        RuntimePerfScenario::DurableStandardToolTurnSqlite,
        RuntimePerfScenario::DurableRlmCheckpointTurnSqlite,
        RuntimePerfScenario::DurableAgentChildTurnSqlite,
    ] {
        let result = Box::pin(run_once(scenario, 1, &high_traffic_config()))
            .await
            .unwrap_or_else(|error| panic!("{} failed: {error:#}", scenario.name()));
        assert!(
            !result.phase_profile.is_empty(),
            "{} emitted no phase measurements",
            scenario.name()
        );
        assert!(
            result
                .extra_counters
                .get("store_calls.total")
                .is_some_and(|calls| *calls > 0),
            "{} emitted no decorated store calls: {:?}",
            scenario.name(),
            result.extra_counters
        );

        for (call_key, calls) in result.extra_counters.iter().filter(|(key, _)| {
            key.starts_with("store_calls.") && key.as_str() != "store_calls.total"
        }) {
            let operation = call_key
                .strip_prefix("store_calls.")
                .expect("filtered store call key");
            let family = format!("store.op.{operation}.observed_micros");
            assert!(
                !family.contains("transaction") && !family.contains("pool_wait"),
                "decorator latency key overclaims its bracket: {family}"
            );
            assert_eq!(
                result.extra_counters.get(&format!("{family}.count")),
                Some(calls),
                "{family} count must match the existing decorator call count"
            );
            assert!(
                result
                    .extra_counters
                    .contains_key(&format!("{family}.total")),
                "{family} is missing total observed microseconds"
            );
            assert_eq!(
                result.metric_samples.get(&family).map(Vec::len),
                Some(*calls as usize),
                "{family} must retain one latency sample per decorated call"
            );
        }

        let summaries = super::summarize(
            std::slice::from_ref(&result),
            std::slice::from_ref(&scenario),
            1,
            &StackProfile::capture(None, None),
        );
        for family in result
            .metric_samples
            .keys()
            .filter(|key| key.starts_with("store.op."))
        {
            let latency = &summaries[0].metric_summary[family];
            assert!(latency.p50 >= 0.0);
            assert!(latency.p95 >= latency.p50);
            assert!(latency.max >= latency.p95);
        }

        for key in [
            "process.cpu_ms",
            "process.cpu_utilization",
            "runtime.workers",
            "runtime.global_queue_depth_max",
        ] {
            assert!(
                result.metric_samples.contains_key(key),
                "{} omitted scheduler metric {key}",
                scenario.name()
            );
        }
        let cpu_ms = result.metric_samples["process.cpu_ms"][0];
        let available_cores =
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get) as f64;
        assert!(
            cpu_ms <= result.total_ms * available_cores,
            "process CPU {cpu_ms} ms exceeds {} ms across {available_cores} cores",
            result.total_ms
        );
        assert!(result.metric_samples["process.cpu_utilization"][0] >= 0.0);
        assert!(result.metric_samples["runtime.workers"][0] >= 1.0);

        #[cfg(target_has_atomic = "64")]
        {
            for key in [
                "runtime.worker_busy_ms",
                "runtime.busy_fraction",
                "runtime.worker_park_count",
            ] {
                assert!(
                    result.metric_samples.contains_key(key),
                    "{} omitted 64-bit scheduler metric {key}",
                    scenario.name()
                );
            }
            let busy_fraction = result.metric_samples["runtime.busy_fraction"][0];
            assert!(
                (0.0..=1.0).contains(&busy_fraction),
                "busy fraction out of bounds: {busy_fraction}"
            );
        }
        #[cfg(not(target_has_atomic = "64"))]
        for key in [
            "runtime.worker_busy_ms",
            "runtime.busy_fraction",
            "runtime.worker_park_count",
        ] {
            assert!(
                !result.metric_samples.contains_key(key),
                "{} must omit unavailable 64-bit scheduler metric {key}",
                scenario.name()
            );
        }

        let report = serde_json::to_string(&result).expect("runtime perf result serializes");
        assert!(
            !report.contains("turn.cpu"),
            "per-turn CPU attribution must never appear in a report"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_sqlite_checkpoint_curve_reports_each_target_size() {
    let result = Box::pin(run_once(
        RuntimePerfScenario::DurableCheckpointCurveSqlite,
        DURABLE_CHECKPOINT_CURVE_BYTES.len(),
        &high_traffic_config(),
    ))
    .await
    .expect("durable checkpoint curve should run");

    for target_bytes in DURABLE_CHECKPOINT_CURVE_BYTES {
        let checkpoint_bytes = result
            .extra_counters
            .get(&format!("checkpoint_curve.{target_bytes}.checkpoint_bytes"))
            .copied()
            .unwrap_or_else(|| panic!("missing checkpoint measurement for {target_bytes}"));
        assert!(
            checkpoint_bytes >= target_bytes as u64
                && checkpoint_bytes <= target_bytes as u64 + 16 * 1024,
            "checkpoint size for target {target_bytes} was {checkpoint_bytes}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn high_traffic_sqlite_smoke_reports_load_structure() {
    let result = Box::pin(run_once(
        RuntimePerfScenario::HighTrafficLoadSqlite,
        1,
        &high_traffic_config(),
    ))
    .await
    .expect("high-traffic SQLite load smoke should run");

    assert!(
        result
            .extra_counters
            .get("throughput.turns_per_second_milli")
            .is_some_and(|value| *value > 0)
    );
    assert!(result.phase_profile.contains_key("wait.store_transaction"));
    assert!(result.phase_profile.contains_key("wait.queue_enqueue"));
    assert!(
        result
            .extra_counters
            .contains_key("load.phase.committed_turn.p95_micros")
    );
    assert!(
        result
            .extra_counters
            .get("queue_depth.samples")
            .is_some_and(|value| *value > 0)
    );
    assert!(
        result
            .extra_counters
            .keys()
            .any(|key| key.starts_with("wait.top."))
    );
    assert_eq!(
        result
            .extra_counters
            .get("wait.arrival_pacing_lateness.observable"),
        Some(&0)
    );
    assert!(
        !result
            .extra_counters
            .keys()
            .any(|key| key.starts_with("knee."))
    );
    assert!(
        !result
            .extra_counters
            .keys()
            .any(|key| key.contains("facade_mutex") || key.contains("driver_dispatch"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn high_traffic_sqlite_knee_smoke_reports_each_step() {
    let result = Box::pin(run_once(
        RuntimePerfScenario::HighTrafficKneeSqlite,
        1,
        &high_traffic_config(),
    ))
    .await
    .expect("high-traffic SQLite knee smoke should run");

    assert_eq!(
        result.extra_counters.get("knee.step.0.population"),
        Some(&2)
    );
    assert_eq!(
        result.extra_counters.get("knee.step.1.population"),
        Some(&4)
    );
    assert!(
        result
            .extra_counters
            .contains_key("knee.step.1.p95_vs_base_ratio_milli")
    );
    assert!(
        result
            .extra_counters
            .contains_key("knee.step.1.throughput_vs_linear_ratio_milli")
    );
    assert!(
        result
            .extra_counters
            .contains_key("knee.step.1.wait.store_transaction.micros")
    );
    assert!(
        result
            .extra_counters
            .contains_key("knee.step.1.wait.claim_scan.micros")
    );
    let durable_samples = result
        .extra_counters
        .keys()
        .filter(|key| key.contains("queue_depth.durable.sample."))
        .count();
    assert_eq!(durable_samples, result.turns.len());
    assert_eq!(
        result
            .turns
            .iter()
            .map(|turn| turn.turn_index)
            .collect::<BTreeSet<_>>()
            .len(),
        result.turns.len()
    );
    assert!(
        result
            .turns
            .iter()
            .any(|turn| { turn.turn_index >= 100_000_000 && turn.turn_index < 200_000_000 })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn high_traffic_trigger_waits_for_terminal_delivery() {
    let config = HighTrafficConfig::parse(1, 0, "trigger=1", "1,2", 1.25)
        .expect("valid trigger-only config");
    let result = Box::pin(run_once(
        RuntimePerfScenario::HighTrafficLoadSqlite,
        1,
        &config,
    ))
    .await
    .expect("trigger-only high-traffic operation should observe terminal delivery");

    assert_eq!(
        result.extra_counters.get("turn_mix.trigger.completed"),
        Some(&1)
    );
    assert_eq!(result.turns.len(), 1);
}

#[test]
fn turn_scenarios_require_the_typed_commit_phase_metrics() {
    for scenario in [
        RuntimePerfScenario::DeepTurnComposition,
        RuntimePerfScenario::RlmAsyncToolCompletion,
        RuntimePerfScenario::RlmTriggerMailPipeline,
        RuntimePerfScenario::RlmLargePrint,
        RuntimePerfScenario::RlmObliqueStackMix,
        RuntimePerfScenario::RlmStreamedPairedLashlang,
        RuntimePerfScenario::RlmProcessHandles,
        RuntimePerfScenario::Standard,
    ] {
        let phases = required_phases(scenario);
        for expected in ["prepared_turn", "committed_turn", "post_commit_delivery"] {
            assert!(
                phases.contains(&expected),
                "{} is missing required phase {expected}",
                scenario.name()
            );
        }
        for removed in [
            "finalize_turn",
            "persist_turn",
            "final_commit",
            "post_persist_hooks",
        ] {
            assert!(
                !phases.contains(&removed),
                "{} still requires removed phase {removed}",
                scenario.name()
            );
        }
    }
}

#[test]
fn every_required_phase_has_a_checked_in_wall_clock_budget() {
    for scenario in RuntimePerfScenario::KNOWN {
        if !scenario.has_guard_budget() {
            continue;
        }
        for phase in required_phases(scenario) {
            assert!(
                phase_wall_clock_budget_ms(scenario, phase).is_some(),
                "{} requires unbudgeted phase {phase}",
                scenario.name()
            );
        }
    }
}

#[test]
fn typed_runtime_phase_inventory_is_required_and_budgeted() {
    let standard = required_phases(RuntimePerfScenario::Standard);
    for phase in lash_core::runtime::RuntimeTurnPhase::ALL {
        let name = phase_name(*phase);
        assert!(
            standard.contains(&name),
            "new typed runtime phase {name} is not required by the standard scenario"
        );
        assert!(
            phase_wall_clock_budget_ms(RuntimePerfScenario::Standard, name).is_some(),
            "new typed runtime phase {name} has no wall-clock budget"
        );
    }
}

#[test]
fn runtime_phase_inventory_is_closed_in_both_directions() {
    let known_scenarios = RuntimePerfScenario::KNOWN
        .iter()
        .filter(|scenario| scenario.has_guard_budget())
        .map(|scenario| scenario.name())
        .collect::<BTreeSet<_>>();
    let budgeted_scenarios = configured_scenario_names().collect::<BTreeSet<_>>();
    assert_eq!(
        budgeted_scenarios, known_scenarios,
        "every budgeted phase must be owned by a known runtime perf scenario"
    );

    for scenario in RuntimePerfScenario::KNOWN {
        if !scenario.has_guard_budget() {
            continue;
        }
        assert_complete_runtime_budget(scenario);
        let required = required_phases(scenario)
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let budgeted = configured_phase_names(scenario).collect::<BTreeSet<_>>();
        assert!(
            required.is_subset(&budgeted),
            "{} is missing budgets for required phases: {:?}",
            scenario.name(),
            required.difference(&budgeted).collect::<Vec<_>>()
        );
    }
}

#[test]
fn materially_different_shared_phases_use_scenario_budgets() {
    assert!(
        phase_wall_clock_budget_ms(
            RuntimePerfScenario::RlmObliqueStackMix,
            "rlm_lashlang.execute",
        ) > phase_wall_clock_budget_ms(RuntimePerfScenario::Rlm, "rlm_lashlang.execute")
    );
    assert!(
        phase_wall_clock_budget_ms(RuntimePerfScenario::RlmLargeToolCatalog, "effect_loop")
            > phase_wall_clock_budget_ms(RuntimePerfScenario::Rlm, "effect_loop")
    );
}

#[test]
fn fig_1910_catalog_variants_are_opt_in_but_guarded_witnesses() {
    for scenario in [
        RuntimePerfScenario::RlmToolCatalogCold,
        RuntimePerfScenario::RlmToolCatalogWarm,
    ] {
        assert!(!RuntimePerfScenario::DEFAULTS.contains(&scenario));
        assert!(scenario.has_guard_budget());
    }
}

#[test]
fn runtime_perf_direct_counterparts_link_to_correctness_coverage() {
    for scenario in [
        RuntimePerfScenario::Standard,
        RuntimePerfScenario::StandardToolCalls,
        RuntimePerfScenario::Rlm,
        RuntimePerfScenario::RlmProcessHandles,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion,
        RuntimePerfScenario::RlmSubagentSpawn,
        RuntimePerfScenario::TurnCheckpoint,
        RuntimePerfScenario::QueuedWorkClaimStress,
        RuntimePerfScenario::TurnInputIngressInterrupt,
    ] {
        assert!(
            !scenario.correctness_coverage_ids().is_empty(),
            "{} has a direct correctness counterpart but no coverage link",
            scenario.name()
        );
    }
}

#[test]
fn runtime_perf_runtime_scenario_rationales_explain_lower_layer_ownership() {
    for scenario in [
        RuntimePerfScenario::OpenAiResponsesSseParse,
        RuntimePerfScenario::DirectLlmClient,
        RuntimePerfScenario::ProcessListStress,
        RuntimePerfScenario::ScopedEffectController,
        RuntimePerfScenario::StoreReopen,
        RuntimePerfScenario::SqliteStoreReopen,
        RuntimePerfScenario::TurnCheckpoint,
        RuntimePerfScenario::LiveReplayPressure,
        RuntimePerfScenario::QueuedWorkClaimStress,
        RuntimePerfScenario::TurnInputIngressInterrupt,
        RuntimePerfScenario::StoreHardeningHotPaths,
    ] {
        let metadata = RuntimePerfScenario::METADATA
            .iter()
            .find(|metadata| metadata.scenario == scenario)
            .expect("Runtime Scenario perf metadata");
        assert_eq!(
            metadata.scenario_harness,
            ScenarioHarnessKind::RuntimeScenario
        );
        assert!(
            metadata.harness_rationale.contains("below")
                && metadata.harness_rationale.contains("protocol")
                && metadata.harness_rationale.contains("facade"),
            "{} must explain why its Runtime Scenario classification remains below protocol/facade ownership: {}",
            metadata.name,
            metadata.harness_rationale
        );
    }
}
