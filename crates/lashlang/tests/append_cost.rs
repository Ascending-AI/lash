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
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, Snapshot, State,
    Value, compile, execute,
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

/// What makes the in-place append cheap is that it charges the appended member
/// incrementally instead of re-pricing the array, and the figure it charges is
/// the one the memory limit is decided against. So the charge has to stay equal
/// to what a fresh measurement of the object would produce: an append that
/// under-charges buys the program headroom the bound was supposed to refuse,
/// and one that over-charges refuses a program the bound admits.
///
/// The snapshot wire states exactly that equality and states it in release
/// builds too. `State::snapshot` writes the heap's running charge
/// (`crates/lashlang/src/runtime/state.rs`, `live_logical_bytes: heap.live_logical_bytes()`,
/// the sum of the per-entry charges), and `Heap::from_wire` re-measures every
/// decoded object with `HeapObject::logical_bytes()` and refuses the snapshot
/// with "heap live logical byte counter does not match its objects" if the two
/// disagree. In a debug build the heap's own `debug_assert_byte_accounting`
/// closes the other half — the running charge equals the sum of the entries —
/// so the appended list's entry is pinned to its object.
///
/// The members are deliberately mixed: a small number, a string, and a nested
/// list, through both spellings. A per-member charge that is wrong by a
/// constant cannot cancel out against a differently shaped member.
#[test]
fn an_append_charges_what_the_object_measures() {
    let source = "items = []
for i in range(0, 64) {
    items[items.length] = \"member-\" + to_string(i)
    appended = __typescript_stdlib(\"push\", items, [i, \"nested\"])
    also = __typescript_stdlib(\"push\", items, i)
}
finish items.length
";
    let compiled = compile(source).expect("byte-accounting probe should compile");
    let mut state = State::new();
    let outcome = futures::executor::block_on(execute(&compiled, &mut state, &Host))
        .expect("byte-accounting probe should execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::Number(192.0)),
        "the probe must append through both spellings"
    );

    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("state carrying the appended array should encode");
    // This is the assertion: the decoder re-measures every object and refuses
    // the snapshot unless the charge the appends accumulated matches.
    let snapshot = Snapshot::from_canonical_bytes(&bytes)
        .expect("the charge accumulated by the appends must equal the objects' measured size");
    let mut restored = State::from_snapshot(snapshot);

    // And the array the charge was accumulated for is still the array that was
    // built, so the equality was not bought by losing members.
    let compiled = compile("finish items.length").expect("restored probe should compile");
    let outcome = futures::executor::block_on(execute(&compiled, &mut restored, &Host))
        .expect("restored probe should execute");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(192.0)));
}
