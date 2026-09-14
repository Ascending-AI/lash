#[path = "../examples/bench_support/mod.rs"]
mod bench_support;

use std::collections::BTreeMap;

use bench_support::{
    BenchHost, Scenario, benchmark_program, linked_benchmark_program, projected_bindings,
    seeded_state_for,
};
use lashlang::{ExecutionEnvironment, ExecutionOutcome, Value, compile_linked, execute};

#[tokio::test(flavor = "current_thread")]
async fn benchmark_scenarios_have_golden_outputs() {
    let host = BenchHost;
    let mut outputs = BTreeMap::new();

    for scenario in Scenario::ALL {
        let source = benchmark_program(*scenario);
        let linked = linked_benchmark_program(*scenario, source.as_str());
        let compiled = compile_linked(&linked);
        let mut state = seeded_state_for(*scenario);
        let projected = projected_bindings(*scenario);
        let env = ExecutionEnvironment::new(&host).with_projected_bindings(projected);
        let outcome = execute(&compiled, &mut state, &env)
            .await
            .unwrap_or_else(|err| panic!("{scenario} benchmark should execute: {err}"));
        let ExecutionOutcome::Finished(value) = outcome else {
            panic!("{scenario} benchmark must finish");
        };
        outputs.insert(scenario.to_string(), stable_json(value));
    }

    insta::assert_snapshot!(
        "lashlang_benchmark_scenario_outputs",
        serde_json::to_string_pretty(&outputs).expect("benchmark outputs should serialize")
    );
}

fn stable_json(value: Value) -> serde_json::Value {
    serde_json::to_value(value).expect("benchmark output should be JSON serializable")
}
