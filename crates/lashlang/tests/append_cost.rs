//! The per-append cost law for JavaScript arrays (FIG-3063).
//!
//! The heap scenarios in the benchmark corpus exist to pin one law: the work a
//! loop does on each step must not depend on how long the list it is building
//! has become. Appending used to break it. `push` and a terminal index write
//! both rebuilt the whole backing vector — clone it, edit the clone, clone the
//! object again inside the commit, and re-walk every member to price it — so a
//! loop that appended n items moved n²/2 members.
//!
//! This is a cost assertion, not a timing one. A counting global allocator
//! measures the bytes the VM asks the allocator for while a program runs; the
//! law is read off the bytes each append costs at two list lengths. Under the
//! rebuild the per-append figure grew with the list (4x the items, ~4x the
//! cost per item); an in-place append leaves it flat. Wall-clock time never
//! enters the assertion, so the test says the same thing on a loaded box.
//!
//! Both spellings are measured through the exact lowered forms the TypeScript
//! adapter emits: `xs.push(item)` becomes a `__typescript_stdlib("push", …)`
//! call, and `xs[xs.length] = item` becomes a terminal index assignment. What
//! those spellings mean is pinned next door in
//! `crates/lash-typescript/tests/array_append.rs`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
    compile, execute,
};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

#[expect(
    unsafe_code,
    reason = "the per-append cost law is measured with a counting global allocator, and GlobalAlloc is an unsafe trait"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let grown = unsafe { System.realloc(pointer, layout, new_size) };
        if !grown.is_null() && new_size > layout.size() {
            ALLOCATED_BYTES.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        }
        grown
    }
}

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unsupported append-cost ability")),
        }
    }
}

/// Runs `source` and returns `(finished value, bytes the run allocated)`.
///
/// Only execution is measured: the program is compiled first, so the parser and
/// the compiler's allocations stay out of the figure.
fn run_measured(source: &str) -> (Value, u64) {
    let compiled = compile(source).expect("cost probe should compile");
    let mut state = State::new();
    let before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let outcome = futures::executor::block_on(execute(&compiled, &mut state, &Host))
        .expect("cost probe should execute");
    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed) - before;
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("cost probe must finish");
    };
    (value, allocated)
}

/// The loop every probe shares, with `body` as its one statement. `finish`
/// reports a number rather than the list so the export at the end of the run
/// does not itself walk what was built.
fn probe_source(body: &str, iterations: usize) -> String {
    format!(
        "items = []
total = 0
for i in range(0, {iterations}) {{
{body}
}}
finish total + len(items)
"
    )
}

/// Bytes one append costs, with the loop, the range and the scaffolding the
/// append does not pay for subtracted out.
fn bytes_per_append(body: &str, iterations: usize) -> f64 {
    let (built, with_append) = run_measured(&probe_source(body, iterations));
    let (baseline_value, without_append) =
        run_measured(&probe_source("    total = total + i", iterations));
    assert_eq!(
        built,
        Value::Number(iterations as f64),
        "the probe must actually append {iterations} items"
    );
    assert_eq!(
        baseline_value,
        Value::Number(((iterations - 1) * iterations / 2) as f64),
        "the baseline probe must run the same loop without appending"
    );
    let appended = with_append.saturating_sub(without_append);
    appended as f64 / iterations as f64
}

/// The law: four times the list, the same cost per item.
///
/// The bound is deliberately loose — an amortised doubling vector allocates in
/// bursts, and the two runs do not land on the same point of the growth curve —
/// but it is far below what a rebuild produces. Before this fix, appending
/// 3,200 items cost roughly four times as much per item as appending 800.
fn assert_per_append_cost_is_flat(label: &str, body: &str) {
    let small = bytes_per_append(body, 800);
    let large = bytes_per_append(body, 3_200);
    assert!(
        large <= small * 2.0,
        "{label}: per-append cost must not grow with the list; \
         800 items cost {small:.1} bytes each, 3200 items cost {large:.1} bytes each"
    );
}

#[test]
fn push_costs_the_same_at_every_list_length() {
    assert_per_append_cost_is_flat(
        "push",
        "    appended = __typescript_stdlib(\"push\", items, i)",
    );
}

#[test]
fn terminal_index_assignment_costs_the_same_at_every_list_length() {
    assert_per_append_cost_is_flat("index append", "    items[items.length] = i");
}
