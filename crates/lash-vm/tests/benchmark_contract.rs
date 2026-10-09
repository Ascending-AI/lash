#[path = "../examples/bench_support/functions.rs"]
mod bench_support;

use bench_support::{FrameHost, FunctionScenario, function_benchmark_program};
use lash_vm::{ExecutionOutcome, State, Value, Vm, VmRunOutcome};

#[tokio::test(flavor = "current_thread")]
async fn frame_heavy_function_benchmark_captures_recursive_frames_and_resumes() {
    let program = function_benchmark_program(FunctionScenario::FrameHeavy);
    let compiled = lash_vm::testing::harness::compile_program(&program);
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
