use std::collections::BTreeSet;

use super::budgets::{
    assert_complete_runtime_budget, configured_phase_names, configured_scenario_names,
    phase_wall_clock_budget_ms,
};
use super::guards::required_phases;
use super::{RuntimePerfScenario, ScenarioHarnessKind};
use crate::runtime_perf::measurement::{
    CHECKPOINT_HASH_PASSES_PER_CHANGED_BODY, CheckpointCurveAxis, CheckpointCurveConfig,
    HighTrafficConfig, RuntimePerfPhaseProbe, checkpoint_curve_points, phase_name, run_once,
    run_once_durable_checkpoint_curve, run_once_store_hardening_hot_paths,
};
use crate::runtime_perf::scenarios::ScenarioPhaseContract;
use lash_core::runtime::RuntimeTurnPhaseProbe;
use lash_core::store::QueuedWorkStore as _;
use lash_core::{SessionCatalogStore as _, SessionListFilter};

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

fn postgres_database_url() -> String {
    ["LASH_POSTGRES_DATABASE_URL", "DATABASE_URL"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|database_url| !database_url.trim().is_empty())
        })
        .expect("PostgreSQL tests require LASH_POSTGRES_DATABASE_URL or DATABASE_URL")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn postgres_pool_checkout_wait_is_recorded_for_runtime_store_reads() {
    let database =
        lash_postgres_store::testing::IsolatedDatabase::create(&postgres_database_url()).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("provision PostgreSQL store");
    let store = storage.store();
    let witness =
        lash_core::perf_witness::Collector::install().expect("install pool checkout witness");

    let batches = store
        .list_queued_work(&lash_core::SessionId::from("pool-wait-witness"))
        .await
        .expect("read queued work through the runtime store");
    assert!(batches.is_empty());
    let waits = witness.snapshot().pool_checkout_wait_nanos;
    assert_eq!(
        waits.len(),
        1,
        "one runtime store read checks out one connection"
    );
    assert!(waits[0] > 0, "checkout duration must be observed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn affected_postgres_scenarios_leave_base_database_clean() {
    let base_database =
        lash_postgres_store::testing::IsolatedDatabase::create(&postgres_database_url()).await;
    let base_storage = lash_postgres_store::testing::connect(base_database.url())
        .await
        .expect("provision clean PostgreSQL base database");
    let base_factory = base_storage.store();
    let before = base_factory
        .list_sessions(&SessionListFilter::default())
        .await
        .expect("list base sessions before perf runs")
        .len();

    Box::pin(run_once_durable_checkpoint_curve(
        RuntimePerfScenario::DurableCheckpointCurvePostgres,
        1,
        &checkpoint_curve_config(),
        Some(base_database.url()),
    ))
    .await
    .expect("durable checkpoint curve PostgreSQL run");
    run_once_store_hardening_hot_paths(1, base_database.url())
        .await
        .expect("store hardening PostgreSQL run");

    let after = base_factory
        .list_sessions(&SessionListFilter::default())
        .await
        .expect("list base sessions after perf runs")
        .len();
    assert_eq!(
        after, before,
        "affected perf runs must not accumulate sessions in the supplied base database: before={before}, after={after}"
    );
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

#[test]
fn durable_representative_turn_inventory_is_backend_complete_and_opt_in() {
    assert_eq!(
        RuntimePerfScenario::DURABLE_REPRESENTATIVE_TURNS.as_slice(),
        [
            RuntimePerfScenario::DurableStandardToolTurnSqlite,
            RuntimePerfScenario::DurableRlmCheckpointTurnSqlite,
            RuntimePerfScenario::DurableAgentChildTurnSqlite,
            RuntimePerfScenario::DurableCheckpointCurveSqlite,
            RuntimePerfScenario::DurableCheckpointCurvePostgres,
            RuntimePerfScenario::HighTrafficLoadSqlite,
            RuntimePerfScenario::HighTrafficKneeSqlite,
            RuntimePerfScenario::FrameResidencyCurveSqlite,
            RuntimePerfScenario::FrameResidencyCurvePostgres,
        ]
    );

    // The durable checkpoint curve is the one storage-only PostgreSQL leg: it
    // measures the store directly and never builds a runtime on it.
    for scenario in RuntimePerfScenario::DURABLE_REPRESENTATIVE_TURNS {
        assert_eq!(
            scenario.uses_postgres(),
            matches!(
                scenario,
                RuntimePerfScenario::DurableCheckpointCurvePostgres
                    | RuntimePerfScenario::FrameResidencyCurvePostgres
            ),
            "{}",
            scenario.name()
        );
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

/// The harness partition and the phase contracts state the same thing.
///
/// `run_once_inner` used to select three scenario groups with predicate
/// early-returns and then carry a six-variant `unreachable!()` arm naming the
/// same groups: one partition written twice, with nothing relating the copies.
/// Narrowing `is_high_traffic`, or retargeting a `phase_contract`, compiled
/// green and panicked the first time that scenario was selected. The dispatch
/// now names its own variants, so the compiler owns exhaustiveness -- and this
/// keeps the predicates, which still steer budgets and summarization, honest
/// about the same partition.
#[test]
fn harness_predicates_and_phase_contracts_describe_one_partition() {
    for scenario in RuntimePerfScenario::KNOWN {
        let contract = scenario.phase_contract();
        assert_eq!(
            scenario.is_checkpoint_curve(),
            contract == ScenarioPhaseContract::CheckpointCurve,
            "{}",
            scenario.name()
        );
        assert_eq!(
            scenario.is_high_traffic(),
            contract == ScenarioPhaseContract::HighTraffic,
            "{}",
            scenario.name()
        );
    }

    // Every scenario with its own harness is durable and uses the runtime
    // harness kind, so the generic turn path is what the remaining contract
    // means.
    for scenario in RuntimePerfScenario::KNOWN {
        if scenario.phase_contract() != ScenarioPhaseContract::StableDurableTurn {
            assert!(scenario.is_durable(), "{}", scenario.name());
        }
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
        assert_eq!(
            scenario.phase_contract(),
            ScenarioPhaseContract::CheckpointCurve
        );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_state_hot_paths_capture_one_changed_component_per_turn() {
    let result = Box::pin(run_once(
        RuntimePerfScenario::CheckpointStateHotPaths,
        2,
        &checkpoint_curve_config(),
        &high_traffic_config(),
    ))
    .await
    .expect("checkpoint state hot paths should run");
    eprintln!(
        "PERF_SCENARIO {}",
        serde_json::to_string(&result).expect("serialize measurement")
    );

    assert_eq!(result.turns.len(), 2);
    assert_eq!(result.extra_counters["execution_state_bindings"], 300);
    assert_eq!(result.extra_counters["execution_state_components"], 300);
    assert_eq!(result.extra_counters["incremental_changed_components"], 1);
    assert!(result.extra_counters["checkpoint_bytes"] > 0);
    assert!(result.extra_counters["hydrated_execution_state_bytes"] > 0);
    for turn in &result.turns {
        for phase in [
            "incremental_capture",
            "measure_budget",
            "component_commit",
            "component_load",
            "execution_restore",
        ] {
            assert!(
                turn.phase_profile
                    .contains_key(&format!("checkpoint_state.{phase}")),
                "turn {} must measure {phase}",
                turn.turn_index,
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_sqlite_checkpoint_curve_reports_paired_structural_samples() {
    let config = checkpoint_curve_config();
    let samples = 2;
    let result = Box::pin(run_once(
        RuntimePerfScenario::DurableCheckpointCurveSqlite,
        samples,
        &config,
        &high_traffic_config(),
    ))
    .await
    .expect("durable checkpoint curve should run");
    eprintln!(
        "PERF_SCENARIO {}",
        serde_json::to_string(&result).expect("serialize measurement")
    );

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
            let expected_hash_count =
                changed_count * CHECKPOINT_HASH_PASSES_PER_CHANGED_BODY + manifest_count;
            assert_eq!(
                hash_count, expected_hash_count,
                "{prefix} sample {sample} observed {hash_count} runtime hash passes for {changed_count} changed bodies and {manifest_count} loaded bodies; expected exactly {expected_hash_count}"
            );
            assert!(
                hash_bytes >= changed_bytes,
                "{prefix} sample {sample} hashed {hash_bytes} bytes for {changed_bytes} changed-body bytes"
            );
            assert_eq!(
                copy_count, 0,
                "{prefix} sample {sample} observed {copy_count} runtime body copies; bodies are shared, not copied"
            );
            assert_eq!(
                copy_bytes, 0,
                "{prefix} sample {sample} copied {copy_bytes} bytes; bodies are shared, not copied"
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
        for metric in ["runtime_body_copy_count", "runtime_body_copy_bytes"] {
            let left = &result.metric_samples[&format!("{}.{metric}", pair[0].prefix())];
            let right = &result.metric_samples[&format!("{}.{metric}", pair[1].prefix())];
            assert!(
                left.iter().zip(right).all(|(left, right)| left <= right),
                "component curve {metric} must be monotonic"
            );
        }
        let left = &result.metric_samples[&format!("{}.runtime_hash_count", pair[0].prefix())];
        let right = &result.metric_samples[&format!("{}.runtime_hash_count", pair[1].prefix())];
        let manifest_left = &result.metric_samples[&format!("{}.manifest_count", pair[0].prefix())];
        let manifest_right =
            &result.metric_samples[&format!("{}.manifest_count", pair[1].prefix())];
        assert!(
            left.iter()
                .zip(right)
                .zip(manifest_left.iter().zip(manifest_right))
                .all(|((left, right), (manifest_left, manifest_right))| {
                    left - manifest_left == right - manifest_right
                }),
            "component curve commit-side hash count must stay flat after accounting for loaded-body validation: left={left:?}, right={right:?}, manifest_left={manifest_left:?}, manifest_right={manifest_right:?}"
        );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn high_traffic_trigger_waits_for_terminal_delivery() {
    let config = HighTrafficConfig::parse(1, 0, "trigger=1", "1,2", 1.25)
        .expect("valid trigger-only config");
    let result = Box::pin(run_once(
        RuntimePerfScenario::HighTrafficLoadSqlite,
        1,
        &checkpoint_curve_config(),
        &config,
    ))
    .await
    .expect("trigger-only high-traffic operation should observe terminal delivery");
    eprintln!(
        "PERF_SCENARIO {}",
        serde_json::to_string(&result).expect("serialize measurement")
    );

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
        RuntimePerfScenario::RlmGlobals,
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
        // A turn commits in its own phase transaction, so the in-process
        // lane never takes local commit admission.
        assert!(
            !phases.contains(&"commit_admission.product_attempt"),
            "{} requires local commit admission on the durable lane",
            scenario.name()
        );
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
    // The durable SQLite lanes admit their commits locally.
    for scenario in [
        RuntimePerfScenario::DurableStandardToolTurnSqlite,
        RuntimePerfScenario::DurableRlmCheckpointTurnSqlite,
        RuntimePerfScenario::DurableAgentChildTurnSqlite,
        RuntimePerfScenario::SqliteStoreReopen,
    ] {
        assert!(
            required_phases(scenario).contains(&"commit_admission.product_attempt"),
            "{} is missing required phase commit_admission.product_attempt",
            scenario.name()
        );
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
        RuntimePerfScenario::ScopedEffects,
        RuntimePerfScenario::StoreReopen,
        RuntimePerfScenario::SqliteStoreReopen,
        RuntimePerfScenario::TurnCheckpoint,
        RuntimePerfScenario::LiveReplayPressure,
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

#[tokio::test]
async fn async_completion_smoke_witnesses_match_session_geometry() {
    for scenario in [
        RuntimePerfScenario::StandardAsyncToolCompletion,
        RuntimePerfScenario::RlmAsyncToolCompletion,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion,
    ] {
        crate::runtime_perf::smoke::execute(
            true,
            scenario,
            3,
            run_once(
                scenario,
                3,
                &checkpoint_curve_config(),
                &high_traffic_config(),
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("{} completion witness: {error:#}", scenario.name()));
    }
}
