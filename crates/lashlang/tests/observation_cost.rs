//! The per-step cost law for unobserved execution sites (FIG-3730).
//!
//! A compiled program carries an execution site on each loop step, branch
//! and call, so that a host that traces execution learns what ran. The VM
//! used to build every observation, cloning the site's strings, and hand it
//! to the host whether or not the host looked. A host that does not observe
//! execution now declares so (`ExecutionHost::observes_lashlang_execution`),
//! and the VM builds nothing for it.
//!
//! This is a cost assertion, not a timing one. A counting global allocator
//! measures, per thread, the bytes the VM asks the allocator for while a loop
//! runs, at two iteration counts. A loop of branches and arithmetic allocates
//! nothing per iteration on an unobserved host, so the two figures match. On
//! an observing host the same loop allocates on every step, which is what the
//! law was measured against before the gate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    LashlangAbilities, LashlangExecutionObservation, LashlangHostCatalog, LashlangHostEnvironment,
    State, Value,
};

#[path = "support/execute.rs"]
#[allow(dead_code, reason = "the law runs cells, not the parse helpers")]
mod execute_support;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

thread_local! {
    /// Allocated bytes, charged to the thread that asked for them, for the
    /// reason FIG-3221 records in `dialect_cost.rs`.
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

/// Charges `bytes` to the calling thread. The key is `const`-initialised and
/// holds a `Copy` type, so this call allocates nothing.
fn charge_to_this_thread(bytes: usize) {
    let _ =
        ALLOCATED_BYTES.try_with(|counter| counter.set(counter.get().saturating_add(bytes as u64)));
}

fn allocated_bytes_on_this_thread() -> u64 {
    ALLOCATED_BYTES.try_with(Cell::get).unwrap_or(0)
}

struct CountingAllocator;

#[expect(
    unsafe_code,
    reason = "the per-step cost law is measured with a counting global allocator, and GlobalAlloc is an unsafe trait"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            charge_to_this_thread(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let grown = unsafe { System.realloc(pointer, layout, new_size) };
        if !grown.is_null() && new_size > layout.size() {
            charge_to_this_thread(new_size - layout.size());
        }
        grown
    }
}

/// A host that runs cells and does not trace them: the default answer of
/// `observes_lashlang_execution`.
struct UnobservedHost;

impl ExecutionHost for UnobservedHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unsupported observation-cost ability",
            )),
        }
    }

    fn observe_lashlang_execution(&self, _observation: LashlangExecutionObservation) {
        panic!("the VM delivered an observation to a host that does not observe execution");
    }
}

/// A loop whose every iteration takes a loop step, a branch and a few ECMA
/// operators, over `iterations` iterations.
fn branching_loop(iterations: u32) -> String {
    format!(
        "let acc = 0; \
         for (let i = 0; i < {iterations}; i++) {{ \
           if (i % 2 === 0) {{ acc = acc + 1; }} else {{ acc = acc - 1; }} \
         }} \
         finish(acc);"
    )
}

#[expect(
    clippy::expect_used,
    reason = "the measured probe compiles and executes its fixture cell, per each message"
)]
fn allocated_while_running(source: &str) -> (Value, u64) {
    let environment =
        LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::all());
    let program = lash_typescript::parse_with_globals(source, &environment.globals)
        .expect("the probe parses");
    let linked = lashlang::LinkedModule::link(program, &environment).expect("the probe links");
    let compiled = lashlang::compile(
        &linked.artifact,
        lashlang::Entry::Main,
        Some(linked.spans()),
    )
    .expect("the probe compiles");
    let mut state = State::new();
    let before = allocated_bytes_on_this_thread();
    let outcome =
        futures::executor::block_on(lashlang::execute(&compiled, &mut state, &UnobservedHost))
            .expect("the probe runs");
    let allocated = allocated_bytes_on_this_thread() - before;
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("the probe must finish");
    };
    (value, allocated)
}

#[test]
fn unobserved_loop_steps_and_branches_allocate_nothing_per_iteration() {
    let (short_value, short) = allocated_while_running(&branching_loop(300));
    let (long_value, long) = allocated_while_running(&branching_loop(3_000));
    assert_eq!(short_value, Value::Number(0.0));
    assert_eq!(long_value, Value::Number(0.0));
    let per_iteration = long.saturating_sub(short) as f64 / 2_700.0;
    assert!(
        per_iteration < 1.0,
        "an unobserved loop allocated {per_iteration:.1} bytes per iteration ({short} bytes over 300 iterations, {long} over 3,000)"
    );
}
