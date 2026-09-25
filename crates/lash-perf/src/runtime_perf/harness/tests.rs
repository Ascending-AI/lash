use super::super::prompt::benchmark_prompt;
use super::super::scenarios::ScenarioWiring;
use super::*;
use tokio_util::sync::CancellationToken;

async fn benchmark_plugin_ids(scenario: RuntimePerfScenario) -> Vec<&'static str> {
    let effect_host = restate_backend()
        .await
        .expect("Restate test backend")
        .lash_backend()
        .effect_host();
    let settlement_control = scenario
        .settlement_children()
        .map(|_| Arc::new(BenchmarkSettlementControl::new()));
    let tool_catalog_observer = scenario
        .wiring()
        .tool_catalog_observer
        .then(|| Arc::new(BenchmarkToolCatalogObserver::default()));
    benchmark_plugin_factories(
        scenario,
        &effect_host,
        settlement_control.as_ref(),
        tool_catalog_observer.as_ref(),
    )
    .iter()
    .map(|factory| factory.id())
    .collect()
}

#[tokio::test]
async fn scenario_wiring_drives_the_benchmark_plugin_list_in_order() {
    const TOOLS: &str = "runtime_perf_tools";
    let expected: Vec<(RuntimePerfScenario, Vec<&'static str>)> = vec![
        (RuntimePerfScenario::RlmLlmQuery, vec![TOOLS, "llm_tools"]),
        (
            RuntimePerfScenario::RlmSubagentSpawn,
            vec![TOOLS, "subagents"],
        ),
        (
            RuntimePerfScenario::RlmObliqueStackMix,
            vec![TOOLS, "subagents", "runtime_perf_oblique_tools"],
        ),
        (
            RuntimePerfScenario::RlmLargeToolCatalog,
            vec![TOOLS, "runtime_perf_large_tool_catalog"],
        ),
        (
            RuntimePerfScenario::ToolDiscoverySearch,
            vec![TOOLS, "runtime_perf_large_tool_catalog"],
        ),
        (
            RuntimePerfScenario::RlmToolCatalogCold,
            vec![
                TOOLS,
                "runtime_perf_large_tool_catalog",
                "runtime_perf_tool_catalog_observer",
            ],
        ),
        (
            RuntimePerfScenario::RlmToolCatalogWarm,
            vec![
                TOOLS,
                "runtime_perf_large_tool_catalog",
                "runtime_perf_tool_catalog_observer",
            ],
        ),
        (
            RuntimePerfScenario::RlmTriggerMailPipeline,
            vec![TOOLS, "runtime_perf_workbench_trigger"],
        ),
        (
            RuntimePerfScenario::DeepTurnComposition,
            vec![TOOLS, "subagents", "runtime_perf_workbench_trigger"],
        ),
        (
            RuntimePerfScenario::AsyncProcessSettlement2Children,
            vec![TOOLS, "runtime_perf_workbench_trigger"],
        ),
        (
            RuntimePerfScenario::AsyncProcessSettlement8Children,
            vec![TOOLS, "runtime_perf_workbench_trigger"],
        ),
        (
            RuntimePerfScenario::DurableAgentChildTurnSqlite,
            vec![TOOLS, "subagents"],
        ),
        (
            RuntimePerfScenario::HighTrafficLoadSqlite,
            vec![TOOLS, "subagents", "runtime_perf_workbench_trigger"],
        ),
        (
            RuntimePerfScenario::HighTrafficKneeSqlite,
            vec![TOOLS, "subagents", "runtime_perf_workbench_trigger"],
        ),
    ];
    let pinned: std::collections::HashSet<RuntimePerfScenario> =
        expected.iter().map(|(scenario, _)| *scenario).collect();
    for (scenario, ids) in expected {
        assert_eq!(
            benchmark_plugin_ids(scenario).await,
            ids,
            "benchmark plugin order changed for {}",
            scenario.name()
        );
    }
    for metadata in RuntimePerfScenario::METADATA {
        if pinned.contains(&metadata.scenario) {
            continue;
        }
        assert_eq!(
            benchmark_plugin_ids(metadata.scenario).await,
            vec![TOOLS],
            "{} unexpectedly installs benchmark plugins",
            metadata.name
        );
    }
}

#[test]
fn rlm_globals_carve_out_lives_only_in_the_rlm_arm() {
    // The queued-work carve-out used to be written in both execution-mode
    // arms; in the Standard arm `RlmGlobals` cannot appear because the metadata
    // table declares it RLM, so both predicates were unconditionally constant.
    // The wiring column now carries the fact once.
    let wiring = RuntimePerfScenario::RlmGlobals.wiring();
    assert_eq!(
        RuntimePerfScenario::RlmGlobals.execution_mode(),
        ExecutionMode::Rlm
    );
    assert!(!wiring.queued_work);
    assert_eq!(
        RuntimePerfScenario::RlmGlobals.wiring(),
        ScenarioWiring {
            queued_work: false,
            ..ScenarioWiring::DEFAULT
        }
    );
}

#[tokio::test]
async fn rlm_globals_keeps_fixed_session_projection_across_real_turns() {
    super::super::smoke::execute(
        true,
        RuntimePerfScenario::RlmGlobals,
        1,
        Box::pin(async {
            let mut runtime = build_runtime(RuntimePerfScenario::RlmGlobals, None).await?;
            seed_runtime_state(&mut runtime, RuntimePerfScenario::RlmGlobals).await?;
            let turn = runtime
                .run_turn(
                    lash::TurnInput::text(benchmark_prompt(RuntimePerfScenario::RlmGlobals, 1)),
                    CancellationToken::new(),
                )
                .await?;
            validate_runtime_perf_turn(RuntimePerfScenario::RlmGlobals, 1, &turn)
        }),
    )
    .await
    .expect("RLM globals benchmark should reuse one fixed session projection");
}

/// A dropped benchmark runtime frees its in-process lane (FIG-3723): the
/// server double, its store set and the core's process worker it serves
/// segments with. Every measured run builds one, so a lane that outlived its
/// run would grow the benchmark's memory by a whole runtime per run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_runtime_frees_its_in_process_lane() {
    for scenario in [
        RuntimePerfScenario::Standard,
        RuntimePerfScenario::RlmProcessHandles,
    ] {
        let mut runtime = build_runtime(scenario, None)
            .await
            .expect("build the benchmark runtime");
        let TurnEntry::RestateHandler(restate) = &runtime.turn_entry else {
            panic!(
                "{}: the in-process lane runs on the server double",
                scenario.name()
            );
        };
        let watch = restate.server().drop_watch();
        seed_runtime_state(&mut runtime, scenario)
            .await
            .expect("seed the benchmark session");
        for turn in 1..=2 {
            let report = runtime
                .run_turn(
                    lash::TurnInput::text(benchmark_prompt(scenario, turn)),
                    CancellationToken::new(),
                )
                .await
                .expect("run a benchmark turn");
            validate_runtime_perf_turn(scenario, turn, &report).expect("a valid benchmark turn");
        }
        drop(runtime);
        assert!(
            watch.freed_within(std::time::Duration::from_secs(5)).await,
            "{}: the dropped runtime's server double is freed; {} task(s) left",
            scenario.name(),
            watch.live_tasks()
        );
    }
}
