#[path = "../examples/bench_support/mod.rs"]
mod bench_support;

use std::collections::BTreeMap;

use bench_support::{
    BenchHost, FrameHost, FunctionScenario, Scenario, function_benchmark_program,
    linked_benchmark_program, projected_bindings, seeded_state_for,
};
use lashlang::{ExecutionEnvironment, ExecutionOutcome, State, Value, Vm, VmRunOutcome, execute};

#[tokio::test(flavor = "current_thread")]
async fn benchmark_scenarios_have_golden_outputs() {
    let host = BenchHost;
    let mut outputs = BTreeMap::new();

    for scenario in Scenario::ALL {
        let linked = linked_benchmark_program(*scenario);
        let compiled = lashlang::compile(
            &linked.artifact,
            lashlang::Entry::Main,
            Some(linked.spans()),
        )
        .expect("a module main entry compiles");
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

#[expect(
    clippy::expect_used,
    reason = "the benchmark-contract snapshot is a crate-owned JSON structure, which serializes, per the message"
)]
fn stable_json(value: Value) -> serde_json::Value {
    serde_json::to_value(value).expect("benchmark output should be JSON serializable")
}

#[tokio::test(flavor = "current_thread")]
async fn frame_heavy_function_benchmark_captures_recursive_frames_and_resumes() {
    let program = function_benchmark_program(FunctionScenario::FrameHeavy);
    let compiled = lashlang::testing::harness::compile_program(&program);
    let mut state = State::new();
    let mut vm = Vm::from_state(&compiled, &mut state, &FrameHost).expect("frame benchmark VM");

    assert_eq!(
        vm.run_process_until_effect()
            .await
            .expect("frame benchmark effect"),
        VmRunOutcome::EffectCompleted
    );
    let continuation = vm.suspend().expect("frame benchmark continuation");
    assert_eq!(continuation.frame_depth(), 513);
    assert!(continuation.heap.allocation_counter() > 0);
    assert!(continuation.heap.live_logical_bytes() > 0);

    let mut resumed =
        Vm::resume_from(continuation, &compiled, &FrameHost).expect("frame benchmark resumes");
    assert_eq!(
        resumed
            .run_process_until_effect()
            .await
            .expect("frame benchmark finishes"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(0.0); 8].into()
        )))
    );
}
