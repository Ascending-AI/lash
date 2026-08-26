use std::collections::BTreeSet;

use super::budgets::{
    assert_complete_runtime_budget, configured_phase_names, configured_scenario_names,
    phase_wall_clock_budget_ms,
};
use super::guards::required_phases;
use super::{RuntimePerfScenario, ScenarioHarnessKind};
use crate::perf_support::stack::StackProfile;
use crate::runtime_perf::measurement::{
    CHECKPOINT_HASH_PASSES_PER_CHANGED_BODY_FLOOR, CheckpointCurveAxis, CheckpointCurveConfig,
    HighTrafficConfig, RuntimePerfPhaseProbe, checkpoint_curve_points, phase_name, run_once,
};
use lash_core::runtime::RuntimeTurnPhaseProbe;

const STABLE_DURABLE_PHASES: [&str; 5] = [
    "prepared_turn",
    "committed_turn",
    "post_commit_delivery",
    "effect_loop",
    "context_transform",
];

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

fn checkpoint_curve_config() -> CheckpointCurveConfig {
    CheckpointCurveConfig::new(8 * 1024, 2, 4, 8).expect("valid checkpoint curve test config")
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
    for scenario in RuntimePerfScenario::DURABLE_REPRESENTATIVE_TURNS
        .into_iter()
        .filter(|scenario| !scenario.uses_postgres())
    {
        let result = Box::pin(run_once(
            scenario,
            1,
            4,
            &checkpoint_curve_config(),
            &high_traffic_config(),
        ))
        .await
        .unwrap_or_else(|error| panic!("{} failed: {error:#}", scenario.name()));
        for phase in STABLE_DURABLE_PHASES {
            assert!(
                result.phase_profile.contains_key(phase),
                "{} omitted stable phase {phase}: {:?}",
                scenario.name(),
                result.phase_profile.keys().collect::<Vec<_>>()
            );
        }
        for counter in [
            "store_calls.commit_runtime_state",
            "store_calls.load_session",
            "durable_commit.logical_bytes",
            "durable_commit.logical_rows",
        ] {
            assert!(
                result
                    .extra_counters
                    .get(counter)
                    .is_some_and(|value| *value > 0),
                "{} omitted positive {counter}: {:?}",
                scenario.name(),
                result.extra_counters
            );
        }
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
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].runs, 1);
        for phase in STABLE_DURABLE_PHASES {
            assert!(
                summaries[0].phase_summary.contains_key(phase),
                "{} summary omitted stable phase {phase}",
                scenario.name()
            );
        }
        for family in result
            .metric_samples
            .keys()
            .filter(|key| key.starts_with("store.op."))
        {
            assert!(
                summaries[0].metric_summary.contains_key(family),
                "{} summary omitted latency family {family}",
                scenario.name()
            );
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

#[test]
fn durable_representative_turn_inventory_is_backend_complete_and_opt_in() {
    assert_eq!(
        RuntimePerfScenario::DURABLE_REPRESENTATIVE_TURNS,
        [
            RuntimePerfScenario::DurableStandardToolTurnSqlite,
            RuntimePerfScenario::DurableStandardToolTurnPostgres,
            RuntimePerfScenario::DurableRlmCheckpointTurnSqlite,
            RuntimePerfScenario::DurableRlmCheckpointTurnPostgres,
            RuntimePerfScenario::DurableAgentChildTurnSqlite,
            RuntimePerfScenario::DurableAgentChildTurnPostgres,
        ]
    );

    for pair in RuntimePerfScenario::DURABLE_REPRESENTATIVE_TURNS
        .as_chunks::<2>()
        .0
    {
        assert!(!pair[0].uses_postgres());
        assert!(pair[1].uses_postgres());
        assert_eq!(pair[0].execution_mode(), pair[1].execution_mode());
    }

    for scenario in RuntimePerfScenario::DURABLE_REPRESENTATIVE_TURNS {
        let metadata = RuntimePerfScenario::METADATA
            .iter()
            .find(|metadata| metadata.scenario == scenario)
            .unwrap_or_else(|| panic!("{} is missing metadata", scenario.name()));
        assert_eq!(
            metadata.scenario_harness,
            ScenarioHarnessKind::RuntimeScenario
        );
        assert!(!RuntimePerfScenario::DEFAULTS.contains(&scenario));
    }
}

#[test]
fn durable_queued_work_contention_inventory_is_backend_complete_and_opt_in() {
    let scenarios = [
        RuntimePerfScenario::DurableQueuedWorkContentionSqlite,
        RuntimePerfScenario::DurableQueuedWorkContentionPostgres,
    ];
    assert!(!scenarios[0].uses_postgres());
    assert!(scenarios[1].uses_postgres());
    for scenario in scenarios {
        assert!(scenario.is_durable());
        assert!(scenario.is_queued_work_contention());
        assert!(!RuntimePerfScenario::DEFAULTS.contains(&scenario));
        let metadata = RuntimePerfScenario::METADATA
            .iter()
            .find(|metadata| metadata.scenario == scenario)
            .unwrap_or_else(|| panic!("{} is missing metadata", scenario.name()));
        assert_eq!(
            metadata.scenario_harness,
            ScenarioHarnessKind::RuntimeScenario
        );
        assert!(metadata.harness_rationale.contains("quiet box"));
    }
}

#[test]
fn durable_checkpoint_curve_inventory_is_backend_complete_and_opt_in() {
    let scenarios = [
        RuntimePerfScenario::DurableCheckpointCurveSqlite,
        RuntimePerfScenario::DurableCheckpointCurvePostgres,
    ];
    assert!(!scenarios[0].uses_postgres());
    assert!(scenarios[1].uses_postgres());
    for scenario in scenarios {
        assert!(scenario.is_durable());
        assert!(scenario.is_checkpoint_curve());
        assert!(!RuntimePerfScenario::DEFAULTS.contains(&scenario));
        let metadata = RuntimePerfScenario::METADATA
            .iter()
            .find(|metadata| metadata.scenario == scenario)
            .unwrap_or_else(|| panic!("{} is missing metadata", scenario.name()));
        assert_eq!(
            metadata.scenario_harness,
            ScenarioHarnessKind::RuntimeScenario
        );
        assert!(metadata.harness_rationale.contains("CLI-configurable"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_queued_work_contention_sqlite_smoke_reports_structure_and_counters() {
    let workers = 4;
    let turns = 2;
    let result = Box::pin(run_once(
        RuntimePerfScenario::DurableQueuedWorkContentionSqlite,
        turns,
        workers,
        &checkpoint_curve_config(),
        &high_traffic_config(),
    ))
    .await
    .expect("durable queued-work contention SQLite smoke should run");

    assert_eq!(
        result.extra_counters["durable_contention.workers"],
        workers as u64
    );
    assert_eq!(
        result.extra_counters["durable_contention.completed_batches"],
        (workers * turns) as u64
    );
    assert_eq!(
        result.extra_counters["durable_contention.remaining_batches"],
        0
    );
    assert_eq!(
        result.extra_counters["durable_contention.pool_wait_observable"],
        0
    );
    assert!(
        !result
            .extra_counters
            .contains_key("durable_contention.pool_wait_micros"),
        "pool wait value must be absent when it is unobservable"
    );
    for counter in [
        "durable_contention.claim_attempts",
        "durable_contention.claim_refusals",
        "durable_contention.successful_claims",
        "durable_contention.renewals",
        "durable_contention.abandons",
        "durable_contention.reclaims",
        "durable_contention.reclaim_conflicts",
        "durable_contention.store_contention_retries",
        "durable_contention.lease_probe_busy",
        "durable_contention.cas_failures",
    ] {
        assert!(
            result.extra_counters.contains_key(counter),
            "missing {counter}"
        );
    }
    let completed = result.extra_counters["durable_contention.completed_batches"];
    let claim_attempts = result.extra_counters["durable_contention.claim_attempts"];
    let claim_refusals = result.extra_counters["durable_contention.claim_refusals"];
    let cas_failures = result.extra_counters["durable_contention.cas_failures"];
    assert!(
        workers == 1 || claim_attempts > completed,
        "multiple workers must poll concurrently: claim_attempts={claim_attempts}, completed_batches={completed}"
    );
    assert!(
        workers == 1 || claim_refusals + cas_failures > 0,
        "multiple workers must witness contention: claim_refusals={claim_refusals}, cas_failures={cas_failures}"
    );

    let successful_claims = result.extra_counters["durable_contention.successful_claims"];
    let renewals = result.extra_counters["durable_contention.renewals"];
    let abandons = result.extra_counters["durable_contention.abandons"];
    let reclaims = result.extra_counters["durable_contention.reclaims"];
    let reclaim_conflicts = result.extra_counters["durable_contention.reclaim_conflicts"];
    assert_eq!(
        abandons,
        reclaims + reclaim_conflicts,
        "every abandon must end in a reclaim or reclaim conflict"
    );
    assert_eq!(
        renewals,
        successful_claims / 3,
        "renewals must follow every third successful claim"
    );
    assert_eq!(
        abandons,
        successful_claims / 2,
        "abandons must follow every second successful claim"
    );
    assert_eq!(
        result.extra_counters["durable_contention.lease_probe_busy"], workers as u64,
        "each worker must witness the controller fence rejecting a foreign owner"
    );

    let claim_wait = &result.metric_samples_ms["durable_contention.claim_wait_ms"];
    let service = &result.metric_samples_ms["durable_contention.service_ms"];
    assert_eq!(
        claim_wait.len(),
        service.len(),
        "claim-wait and service samples must describe the same completed units"
    );
    assert!(
        claim_wait.len() >= completed as usize,
        "latency samples must cover every completed batch: samples={}, completed_batches={completed}",
        claim_wait.len()
    );
    for (metric, samples) in [("claim_wait_ms", claim_wait), ("service_ms", service)] {
        assert!(
            samples
                .iter()
                .all(|sample| sample.is_finite() && *sample >= 0.0),
            "{metric} samples must be finite and non-negative"
        );
    }
    assert!(
        result
            .metric_samples_ms
            .contains_key("durable_contention.pool_wait_ms"),
        "missing durable_contention.pool_wait_ms"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_sqlite_checkpoint_curve_reports_paired_structural_samples() {
    let config = checkpoint_curve_config();
    let samples = 2;
    let result = Box::pin(run_once(
        RuntimePerfScenario::DurableCheckpointCurveSqlite,
        samples,
        4,
        &config,
        &high_traffic_config(),
    ))
    .await
    .expect("durable checkpoint curve should run");

    let points = checkpoint_curve_points(&config);
    assert_eq!(
        result.extra_counters["checkpoint_curve.point_count"],
        points.len() as u64
    );
    for point in &points {
        let prefix = point.prefix();
        for metric in [
            "manifest_count",
            "changed_body_count",
            "changed_body_bytes",
            "runtime_hash_count",
            "runtime_hash_bytes",
            "runtime_body_copy_count",
            "runtime_body_copy_bytes",
        ] {
            assert_eq!(
                result
                    .metric_samples
                    .get(&format!("{prefix}.{metric}"))
                    .map(Vec::len),
                Some(samples),
                "{prefix} must report a complete {metric} sample vector"
            );
        }
        for phase in ["capture", "serialize", "commit", "load"] {
            assert_eq!(
                result
                    .metric_samples_ms
                    .get(&format!("{prefix}.{phase}_ms"))
                    .map(Vec::len),
                Some(samples),
                "{prefix} must report a complete {phase} sample vector"
            );
        }
        for sample in 0..samples {
            let value =
                |metric: &str| result.metric_samples[&format!("{prefix}.{metric}")][sample] as u64;
            let changed_count = value("changed_body_count");
            let changed_bytes = value("changed_body_bytes");
            assert!(
                changed_count > 0,
                "{prefix} sample {sample} must change bodies"
            );
            assert!(
                changed_bytes > 0,
                "{prefix} sample {sample} must change bytes"
            );
            let hash_count = value("runtime_hash_count");
            let hash_bytes = value("runtime_hash_bytes");
            let copy_count = value("runtime_body_copy_count");
            let copy_bytes = value("runtime_body_copy_bytes");
            let manifest_count = value("manifest_count");
            let minimum_hash_count =
                changed_count * CHECKPOINT_HASH_PASSES_PER_CHANGED_BODY_FLOOR + manifest_count;
            assert!(
                hash_count >= minimum_hash_count,
                "{prefix} sample {sample} observed {hash_count} runtime hash passes for {changed_count} changed bodies and {manifest_count} loaded bodies; expected at least {minimum_hash_count}"
            );
            assert!(
                hash_bytes >= changed_bytes,
                "{prefix} sample {sample} hashed {hash_bytes} bytes for {changed_bytes} changed-body bytes"
            );
            assert!(
                copy_count >= changed_count,
                "{prefix} sample {sample} observed {copy_count} runtime body copies for {changed_count} changed bodies"
            );
            assert!(
                copy_bytes >= changed_bytes,
                "{prefix} sample {sample} copied {copy_bytes} bytes for {changed_bytes} changed-body bytes"
            );
        }
    }

    let component_points = points
        .iter()
        .filter(|point| point.axis == CheckpointCurveAxis::Components)
        .collect::<Vec<_>>();
    for pair in component_points.windows(2) {
        let left = &result.metric_samples[&format!("{}.manifest_count", pair[0].prefix())];
        let right = &result.metric_samples[&format!("{}.manifest_count", pair[1].prefix())];
        assert!(
            left.iter().zip(right).all(|(left, right)| left < right),
            "component curve manifest count must increase monotonically: left={left:?}, right={right:?}"
        );
        for metric in [
            "runtime_hash_count",
            "runtime_hash_bytes",
            "runtime_body_copy_count",
            "runtime_body_copy_bytes",
        ] {
            let left = &result.metric_samples[&format!("{}.{metric}", pair[0].prefix())];
            let right = &result.metric_samples[&format!("{}.{metric}", pair[1].prefix())];
            assert!(
                left.iter().zip(right).all(|(left, right)| left <= right),
                "component curve {metric} must be monotonic"
            );
        }
    }
    let byte_points = points
        .iter()
        .filter(|point| point.axis == CheckpointCurveAxis::Bytes)
        .collect::<Vec<_>>();
    for pair in byte_points.windows(2) {
        let left = &result.metric_samples[&format!("{}.changed_body_bytes", pair[0].prefix())];
        let right = &result.metric_samples[&format!("{}.changed_body_bytes", pair[1].prefix())];
        assert!(
            left.iter().zip(right).all(|(left, right)| left < right),
            "byte curve changed-body bytes must increase strictly: left={left:?}, right={right:?}"
        );
        for metric in [
            "runtime_hash_count",
            "runtime_hash_bytes",
            "runtime_body_copy_count",
            "runtime_body_copy_bytes",
        ] {
            let left = &result.metric_samples[&format!("{}.{metric}", pair[0].prefix())];
            let right = &result.metric_samples[&format!("{}.{metric}", pair[1].prefix())];
            assert!(
                left.iter().zip(right).all(|(left, right)| left <= right),
                "byte curve {metric} must be monotonic"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn high_traffic_sqlite_smoke_reports_load_structure() {
    let result = Box::pin(run_once(
        RuntimePerfScenario::HighTrafficLoadSqlite,
        1,
        4,
        &checkpoint_curve_config(),
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
        4,
        &checkpoint_curve_config(),
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
        4,
        &checkpoint_curve_config(),
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
        RuntimePerfScenario::DurableQueuedWorkContentionSqlite,
        RuntimePerfScenario::DurableQueuedWorkContentionPostgres,
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
