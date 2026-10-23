use std::collections::BTreeSet;

use super::budgets::{
    assert_complete_runtime_budget, configured_phase_names, configured_scenario_names,
    phase_wall_clock_budget_ms,
};
use super::guards::required_phases;
use super::{RuntimePerfScenario, ScenarioHarnessKind};
use crate::runtime_perf::measurement::{
    CheckpointCurveAxis, CheckpointCurveConfig, HighTrafficConfig, RuntimePerfPhaseProbe,
    checkpoint_curve_points, phase_name, run_once, run_once_durable_checkpoint_curve,
    run_once_store_hardening_hot_paths,
};
use crate::runtime_perf::scenarios::ScenarioPhaseContract;
use lash_core::runtime::RuntimeTurnPhaseProbe;
use lash_core::store::QueuedWorkStore as _;
use lash_core::{SessionCatalogStore as _, SessionListFilter};
use lash_vocabulary::tokens::TokenWriter;

/// FIG-6620: changing phase observation or losing its identity refuses history.
#[test]
fn runtime_history_refuses_mixed_or_missing_phase_observers() {
    let identity = |clock_only| {
        let mut identity = super::runtime_experiment(serde_json::json!({}), clock_only, false)
            .for_scenario("standard");
        identity.host = "control-host".to_owned();
        let mut geometry: serde_json::Value =
            serde_json::from_str(&identity.configured_geometry).unwrap();
        geometry["worker"] = serde_json::json!("control-worker");
        identity.configured_geometry = geometry.to_string();
        identity
    };
    let observed = identity(false);
    let clock = identity(true);
    assert!(observed.compare(&observed).is_ok());
    assert!(observed.compare(&clock).is_err());
    let mut missing = observed.clone();
    missing.configured_geometry = "{}".to_owned();
    assert!(missing.compare(&missing).is_err());
    println!(
        "history-observer-identity.json={}",
        serde_json::json!({
            "runtime": {"observed": observed, "clock": clock,
                "mixed_refused": true, "missing_refused": true}
        })
    );
}

/// GRAPH-WORK: a reused canonical image materializes no envelope, while
/// every cache-key hash and every adoption hash remains measured work.
#[test]
fn prepared_graph_work_counts_materialization_and_lookup() {
    use lash_core_store::checkpoint_graph::*;
    let witness = lash_core::perf_witness::Collector::install().expect("graph witness");
    let mut graph = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(1 << 20)));
    let mut body = TokenWriter::new();
    body.text("FIG-6157 prepared graph work witness");
    let body = body.into_bytes();
    let root = graph
        .encode(NodeKind::TEXT_LEAF, TypedBody::logical(&body))
        .unwrap();
    let bytes = graph.nodes()[&root].bytes.len() as u64;
    graph
        .encode(NodeKind::TEXT_LEAF, TypedBody::logical(&body))
        .unwrap();
    let encoded_work = witness.snapshot();
    assert_eq!(encoded_work.graph_envelopes, 1);
    assert_eq!(encoded_work.graph_encoded_bytes, bytes);
    let encoding_lookups = 2;
    let logical_scan_lookups = 1;
    let materialization_hashes = 1;
    let encoding_hashes = encoding_lookups + logical_scan_lookups + materialization_hashes;
    assert_eq!(
        encoded_work.hash_passes, encoding_hashes,
        "encoding lookups, logical scan lookup, and envelope materialization"
    );
    let encoded_hash_bytes = (encoding_lookups + logical_scan_lookups) * body.len() as u64 + bytes;
    assert_eq!(encoded_work.hashed_bytes, encoded_hash_bytes);
    graph.adopt(graph.nodes()[&root].clone()).unwrap();
    let work = witness.snapshot();
    assert_eq!(
        work.hash_passes - encoded_work.hash_passes,
        1,
        "adoption hashes the supplied current envelope once"
    );
    assert_eq!(
        work.hash_passes,
        encoding_hashes + 1,
        "adoption still hashes the supplied envelope once"
    );
    assert_eq!(work.hashed_bytes, encoded_hash_bytes + bytes);
    assert_eq!(work.graph_adopt_attempts, 1);
    assert_eq!(work.graph_adopt_bytes, bytes);
    assert_eq!(work.graph_envelopes, 1);
    assert_eq!(work.graph_encoded_bytes, bytes);
}

/// GRAPH-WORK: every logical-scan lookup hashes its complete current
/// input, including cache hits shared across diagnostic node kinds.
#[test]
fn logical_scan_lookup_hashes_are_measured() {
    use lash_core_store::checkpoint_graph::NodeKind;
    use lash_core_store::checkpoint_graph::encode::LogicalScan;
    let witness = lash_core::perf_witness::Collector::install().expect("scan witness");
    let mut body = TokenWriter::new();
    body.text("FIG-7135 measured logical scan lookup");
    let body = body.into_bytes();
    LogicalScan::scan(&body, NodeKind::TEXT_LEAF).unwrap();
    LogicalScan::scan(&body.clone(), NodeKind(0)).unwrap();
    let work = witness.snapshot();
    assert_eq!(work.hash_passes, 2, "both scan lookup hashes are measured");
    assert_eq!(work.hashed_bytes, 2 * body.len() as u64);
}

fn high_traffic_config() -> HighTrafficConfig {
    HighTrafficConfig::parse(4, "plain=1,tool=1,queued=1,child=1", "2,4", 1.25)
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
        .list_queued_work(&lash_vocabulary::identity::SessionId::from(
            "pool-wait-witness",
        ))
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
    assert!(result.extra_counters["initial_pages"] > 0);
    assert!(result.extra_counters["initial_spool_bytes"] > 0);
    assert_eq!(result.metric_samples["changed_bindings"], [1.0, 1.0]);
    for metric in ["pages_sealed", "spool_bytes"] {
        let samples = &result.metric_samples[metric];
        assert_eq!(samples.len(), 2);
        let seed = result.extra_counters[if metric == "pages_sealed" {
            "initial_pages"
        } else {
            "initial_spool_bytes"
        }] as f64;
        assert!(
            samples.iter().all(|value| *value > 0.0 && *value < seed),
            "one changed binding's {metric} must cost less than the retained population: {samples:?} vs {seed}"
        );
    }
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

/// FIG-7197: a checkpoint writer must leave other ready sessions available
/// to their serving node, including the session that runs its edit tasks.
#[tokio::test]
async fn checkpoint_publisher_claims_only_its_session() {
    use lash_core::durable_port::{ActorKey, ActorState, CommitLabel, FormatSet, MailTx};
    let backend = crate::runtime_perf::harness::durable_backend(std::sync::Arc::new(
        crate::runtime_perf::harness::sqlite_memory_stores()
            .await
            .unwrap(),
    ))
    .unwrap();
    let session = lash_vocabulary::identity::SessionId::from("checkpoint-publisher-law");
    let actor = ActorKey::session(session.as_str()).unwrap();
    let sibling = ActorKey::session("checkpoint-edit-law").unwrap();
    let mut created = MailTx::new();
    created.create_actor(sibling.clone(), FormatSet::unstarted_session());
    created.create_actor(actor.clone(), FormatSet::unstarted_session());
    backend
        .durable()
        .commit_mail(created, CommitLabel::new("perf.fixture"))
        .await
        .unwrap();
    let publisher = lash_protocol_rlm::CodeModeCheckpointPerfPublisher::open(&backend, &session)
        .await
        .unwrap();
    assert_eq!(
        backend
            .durable()
            .actor(&sibling)
            .await
            .unwrap()
            .unwrap()
            .state,
        ActorState::Ready,
        "the edit session remains available to its serving node"
    );
    drop(publisher);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !backend.durable().live_nodes().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a dropped publisher hands off its lease cleanup");
    assert_eq!(
        backend
            .durable()
            .actor(&actor)
            .await
            .unwrap()
            .unwrap()
            .state,
        ActorState::Ready,
        "the dropped writer releases its own session too"
    );
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
            "reachable_node_count",
            "staged_node_count",
            "staged_node_bytes",
            "changed_bindings",
            "pages_sealed",
            "spool_bytes",
            "graph_adopt_count",
            "graph_adopt_bytes",
            "graph_encode_count",
            "graph_encode_bytes",
            "runtime_hash_count",
            "runtime_hash_bytes",
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
            assert!(value("runtime_hash_count") > 0 && value("runtime_hash_bytes") > 0);
            assert!(value("graph_encode_count") > 0 && value("graph_encode_bytes") > 0);
            assert!(
                value("staged_node_count") > 0 && value("staged_node_bytes") > 0,
                "{prefix} sample {sample} must stage the reassigned binding"
            );
            assert_eq!(value("changed_bindings"), 1);
            assert!(
                value("pages_sealed") > 0 && value("spool_bytes") > 0,
                "{prefix} sample {sample} must measure heap work for its one changed binding"
            );
        }
    }

    let component_points = points
        .iter()
        .filter(|point| point.axis == CheckpointCurveAxis::Components)
        .collect::<Vec<_>>();
    for pair in component_points.windows(2) {
        let left = &result.metric_samples[&format!("{}.reachable_node_count", pair[0].prefix())];
        let right = &result.metric_samples[&format!("{}.reachable_node_count", pair[1].prefix())];
        assert!(
            left.iter().zip(right).all(|(left, right)| left < right),
            "component curve reachable nodes must increase monotonically: left={left:?}, right={right:?}"
        );
    }
    // HEAP-METER's fixed-change rule: sealed pages grow only with index
    // depth, not retained bindings. Bytes within packed pages can vary at
    // these small populations; the byte axis below measures payload growth.
    let first = component_points.first().unwrap();
    let last = component_points.last().unwrap();
    let left = &result.metric_samples[&format!("{}.pages_sealed", first.prefix())];
    let right = &result.metric_samples[&format!("{}.pages_sealed", last.prefix())];
    for (left, right) in left.iter().zip(right) {
        let exponent =
            (right / left).ln() / (last.component_count as f64 / first.component_count as f64).ln();
        assert!(
            exponent <= 0.25,
            "one changed binding's sealed pages must stay flat in retained bindings: {left} -> {right} (exponent {exponent})"
        );
    }
    let byte_points = points
        .iter()
        .filter(|point| point.axis == CheckpointCurveAxis::Bytes)
        .collect::<Vec<_>>();
    for pair in byte_points.windows(2) {
        let left = &result.metric_samples[&format!("{}.spool_bytes", pair[0].prefix())];
        let right = &result.metric_samples[&format!("{}.spool_bytes", pair[1].prefix())];
        assert!(
            left.iter().zip(right).all(|(left, right)| left < right),
            "byte curve spooled bytes must increase strictly: left={left:?}, right={right:?}"
        );
    }
    let component_center = component_points[1];
    let byte_center = byte_points[1];
    for metric in ["changed_bindings", "pages_sealed", "spool_bytes"] {
        assert_eq!(
            result.metric_samples[&format!("{}.{metric}", component_center.prefix())],
            result.metric_samples[&format!("{}.{metric}", byte_center.prefix())],
            "paired center points must measure the same heap work for {metric}"
        );
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

#[test]
fn turn_scenarios_require_the_typed_commit_phase_metrics() {
    for scenario in [
        RuntimePerfScenario::DeepTurnComposition,
        RuntimePerfScenario::RlmAsyncToolCompletion,
        RuntimePerfScenario::RlmLargePrint,
        RuntimePerfScenario::RlmObliqueStackMix,
        RuntimePerfScenario::RlmStreamedPairedLashVm,
        RuntimePerfScenario::RlmProcessHandles,
        RuntimePerfScenario::RlmGlobals,
        RuntimePerfScenario::Standard,
        RuntimePerfScenario::DurableStandardToolTurnSqlite,
        RuntimePerfScenario::DurableRlmCheckpointTurnSqlite,
        RuntimePerfScenario::DurableAgentChildTurnSqlite,
        RuntimePerfScenario::SqliteStoreReopen,
    ] {
        let phases = required_phases(scenario);
        for expected in ["prepared_turn", "committed_turn", "post_commit_delivery"] {
            assert!(
                phases.contains(&expected),
                "{} is missing required phase {expected}",
                scenario.name()
            );
        }
        // Turns on both SQLite lanes commit in engine phase transactions.
        // Local commit admission belongs to the session-command path.
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
}

/// PERF-INVENTORY: served turns require the phases their current producer emits;
/// retired process, trigger and start-gate machinery has no probe requirement.
#[test]
fn served_phase_inventory_tracks_current_producers() {
    for scenario in RuntimePerfScenario::KNOWN {
        if !scenario.has_guard_budget() {
            continue;
        }
        let phases = configured_phase_names(scenario).collect::<BTreeSet<_>>();
        for phase in &phases {
            assert!(
                !phase.starts_with("codemode_process.")
                    && !matches!(
                        *phase,
                        "turn_cancel.start_gate"
                            | "process_list_stress.signal_append"
                            | "process_list_stress.wait_roundtrip"
                            | "store_hardening.identity.trigger_occurrence"
                    ),
                "{} still budgets retired producer {phase}",
                scenario.name()
            );
        }
        if matches!(
            scenario,
            RuntimePerfScenario::DeepTurnComposition | RuntimePerfScenario::RlmObliqueStackMix
        ) {
            assert!(
                !phases.contains("process.await_handle"),
                "{} uses durable terminal waits rather than the session await helper",
                scenario.name()
            );
        }
        let operation = match scenario {
            RuntimePerfScenario::TurnCancelRoundTrip => {
                Some("turn_cancel.request_to_token_to_seal")
            }
            RuntimePerfScenario::IngressAdmissionProjection => {
                Some("turn_input_ingress.enqueue_to_admission_to_projection")
            }
            _ => None,
        };
        if let Some(operation) = operation {
            assert!(
                required_phases(scenario).contains(&operation),
                "{} must require {operation}",
                scenario.name()
            );
        }
        if phases.contains("effect_loop") {
            assert!(
                required_phases(scenario).contains(&"context_transform"),
                "{} must require its context transform",
                scenario.name()
            );
            if scenario.uses_standard_compaction() {
                assert!(
                    required_phases(scenario)
                        .contains(&"plugin_hook.context_pressure.standard_compaction"),
                    "{} must require its standard compaction hook",
                    scenario.name()
                );
            }
        }
    }
}

/// Material-reference preparation streams its canonical bytes: allocations
/// remain small when the immutable payload grows, and the digest stays exact.
#[test]
fn material_reference_preparation_does_not_copy_the_complete_encoding() {
    use lash_core_store::tool_run::{
        MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole,
    };
    let payload = MaterialPayload::new(
        MaterialOwner::Run {
            opener: lash_core_store::effect_opener::EffectOpener::turn("perf", "material-stream"),
        },
        MaterialRole::AttemptOutput,
        "unicode é\nquote \" slash \\ ".repeat(16_384),
    );
    let expected = lash_protocol_types::core_support::blake3_domain_hash_hex(
        "lash-tool-material/v1",
        serde_json::to_vec(&payload).unwrap(),
    );
    let before = crate::GLOBAL_ALLOCATOR.stats();
    let reference = payload.reference(MaterialLocation::JournalLocal).unwrap();
    let after = crate::GLOBAL_ALLOCATOR.stats();
    let allocated = after.bytes_allocated.saturating_sub(before.bytes_allocated)
        + after
            .bytes_reallocated
            .saturating_sub(before.bytes_reallocated)
            .max(0) as usize;
    assert!(
        allocated < payload.text.len() / 4,
        "material hashing allocated {allocated} bytes for {} payload bytes",
        payload.text.len()
    );
    assert_eq!(reference.digest.as_str(), expected);
    payload.verify(&reference, &payload.owner).unwrap();
}
