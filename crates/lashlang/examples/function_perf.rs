#![allow(dead_code)]

mod bench_support;

use bench_support::{BenchHost, FunctionScenario, function_benchmark_program};
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionMode, ExecutionOutcome,
    State, Value, Vm, VmRunOutcome, compile_ast, execute,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, old, new_size) };
        if !pointer.is_null() && new_size > old.size() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add((new_size - old.size()) as u64, Ordering::Relaxed);
        }
        pointer
    }
}

struct FrameHost;

impl ExecutionHost for FrameHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ProcessEvent(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected frame benchmark effect")),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Process
    }
}

fn main() {
    let mut args = env::args().skip(1);
    if matches!(args.next().as_deref(), Some("--list-scenarios")) {
        for scenario in FunctionScenario::ALL {
            println!("{scenario}");
        }
        return;
    }
    let mut args = env::args().skip(1);
    let scenario = args
        .next()
        .as_deref()
        .and_then(FunctionScenario::parse)
        .unwrap_or(FunctionScenario::NonCapturingCall);
    let iterations = args
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(500)
        .max(1);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let program = function_benchmark_program(scenario);
    let compiled = compile_ast(&program).expect("benchmark program nesting is within the cap");
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    let started = Instant::now();
    let mut maximum_frames = 0usize;
    let mut maximum_heap_allocations = 0u64;
    let mut maximum_live_logical_bytes = 0u64;

    if matches!(scenario, FunctionScenario::FrameHeavy) {
        for _ in 0..iterations {
            let mut state = State::new();
            let mut vm =
                Vm::from_state(&compiled, &mut state, &FrameHost).expect("frame benchmark VM");
            assert_eq!(
                runtime
                    .block_on(vm.run_process_until_effect())
                    .expect("frame benchmark execution"),
                VmRunOutcome::EffectCompleted
            );
            let continuation = vm.suspend().expect("frame benchmark continuation");
            maximum_frames = maximum_frames.max(continuation.frame_depth());
            maximum_heap_allocations =
                maximum_heap_allocations.max(continuation.heap.allocation_counter());
            maximum_live_logical_bytes =
                maximum_live_logical_bytes.max(continuation.heap.live_logical_bytes());
        }
    } else {
        for _ in 0..iterations {
            let mut state = State::new();
            let outcome = runtime
                .block_on(execute(&compiled, &mut state, &BenchHost))
                .expect("function benchmark execution");
            let ExecutionOutcome::Finished(value) = outcome else {
                panic!("function benchmark must finish")
            };
            std::hint::black_box(value);
        }
    }

    let elapsed = started.elapsed();
    println!("lashlang function perf");
    println!("mode: compiled_ast_execute");
    println!("scenario: {scenario}");
    println!("iterations: {iterations}");
    println!(
        "ns_per_iter: {:.1}",
        elapsed.as_nanos() as f64 / iterations as f64
    );
    println!(
        "allocations_per_iter: {:.3}",
        ALLOCATIONS.load(Ordering::Relaxed) as f64 / iterations as f64
    );
    println!(
        "allocated_bytes_per_iter: {:.1}",
        ALLOCATED_BYTES.load(Ordering::Relaxed) as f64 / iterations as f64
    );
    println!("max_frame_depth: {maximum_frames}");
    println!("heap_allocations: {maximum_heap_allocations}");
    println!("live_logical_bytes: {maximum_live_logical_bytes}");
    std::hint::black_box(Value::Null);
}
