//! The TypeScript dialect measured beside the AST corpus (FIG-3063).
//!
//! `crates/lashlang/examples/bench_support/sections/program.rs` builds the
//! benchmark corpus straight from the AST, because what it measures is the IR
//! and the VM and lowering it through a dialect would change the measurement
//! rather than its spelling. That stays true. What was also true until FIG-3063
//! is that the dialect *could not* have carried the corpus: TypeScript had no
//! O(1) list append, so `heap_list_iteration` — a loop that appends 2,000 rows
//! and then walks them — measured 214x its allocation budget, and 27 of the 29
//! scenarios exceeded theirs.
//!
//! This file is the standing proof that this is no longer so. It authors the
//! `heap_list_iteration` scenario in TypeScript and runs it in this process
//! beside the AST program the corpus measures, so the two figures share an
//! allocator and a build profile and the comparison is like-for-like. The AST
//! program is additionally held to the corpus's own checked-in budget — the
//! one in `scripts/perf_guard_budgets.json`, read here, never edited — which
//! anchors the comparison to a number the perf guard already enforces. The
//! three append spellings are measured beside it, so a regression says which
//! spelling regressed.
//!
//! Bytes, never a clock: a counting global allocator records what the VM asks
//! the allocator for, and every figure below is allocated bytes per iteration.
//! Nothing here is a timing assertion, so a loaded box does not move it.

#![expect(clippy::expect_used, clippy::unwrap_used, reason = "FIG-2784 pass 2")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../examples/bench_support/mod.rs"]
mod bench_support;

use bench_support::{
    BenchHost, Scenario, linked_benchmark_program, projected_bindings, seeded_state_for,
};
use lashlang::{
    CompiledProgram, ExecutionEnvironment, ExecutionOutcome, State, compile_linked, execute,
};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

#[expect(
    unsafe_code,
    reason = "the dialect measurement installs a counting global allocator, and GlobalAlloc is an unsafe trait"
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

/// The corpus's own budget file, embedded the way `lash-perf` embeds it
/// (`crates/lash-perf/src/runtime_perf/report/budgets.rs`). Reading it here is
/// what makes "meets the existing budget" checkable without restating a number
/// that would then drift: this file is never edited by this test.
const PERF_GUARD_BUDGETS: &str = include_str!("../../../scripts/perf_guard_budgets.json");

/// `allocated_bytes_per_iter_max` for one corpus scenario and perf mode.
fn budgeted_bytes_per_iter(scenario: &str, mode: &str) -> f64 {
    let budgets: serde_json::Value = serde_json::from_str(PERF_GUARD_BUDGETS)
        .expect("the perf guard budgets must be valid JSON");
    budgets["lashlang"]["perf"][scenario][mode]["allocated_bytes_per_iter_max"]
        .as_f64()
        .unwrap_or_else(|| {
            panic!("scripts/perf_guard_budgets.json must budget {scenario}/{mode} allocated bytes")
        })
}

/// Runs `compiled` `iterations` times from a fresh state and returns allocated
/// bytes per iteration.
///
/// This is the `compiled_execute` shape the corpus budgets: compile once, then
/// pay only for execution. One unmeasured warm-up run absorbs the one-time
/// allocations a first execution makes.
async fn bytes_per_iteration(
    compiled: &CompiledProgram,
    mut fresh_state: impl FnMut() -> State,
    iterations: usize,
) -> f64 {
    let host = BenchHost;
    let projected = projected_bindings(Scenario::HeapListIteration);
    let run = async |state: &mut State| {
        let env = ExecutionEnvironment::new(&host).with_projected_bindings(projected.clone());
        match execute(compiled, state, &env).await {
            Ok(ExecutionOutcome::Finished(value)) => value,
            other => panic!("dialect measurement must finish: {other:?}"),
        }
    };

    let mut warm = fresh_state();
    std::hint::black_box(run(&mut warm).await);

    let before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    for _ in 0..iterations {
        let mut state = fresh_state();
        std::hint::black_box(run(&mut state).await);
    }
    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed) - before;
    allocated as f64 / iterations as f64
}

/// The corpus scenario, built from the AST exactly as the benchmark builds it.
fn ast_heap_list_iteration() -> CompiledProgram {
    compile_linked(&linked_benchmark_program(Scenario::HeapListIteration))
}

/// The same scenario authored in TypeScript: append 2,000 rows, then walk them.
const TYPESCRIPT_HEAP_LIST_ITERATION: &str = "const rows: number[] = [];
for (let n = 0; n < 2000; n++) {
  rows.push(n);
}
let total = 0;
let seen = 0;
for (const row of rows) {
  total += row;
  seen += 1;
}
finish({ total, seen });
";

/// 2,000 `push` appends and nothing else.
const TYPESCRIPT_PUSH: &str = "const rows: number[] = [];
for (let n = 0; n < 2000; n++) {
  rows.push(n);
}
finish(rows.length);
";

/// The same 2,000 appends spelled as a write one past the end.
const TYPESCRIPT_INDEX_ASSIGN: &str = "const rows: number[] = [];
for (let n = 0; n < 2000; n++) {
  rows[rows.length] = n;
}
finish(rows.length);
";

/// 2,000 `.length` reads and no appends at all.
///
/// This is the difference in *spelling* between the two appends above:
/// `rows[rows.length] = n` reads the field the `push` call does not. Measuring
/// it here is what lets the index-assign assertion charge the append for the
/// append and nothing else.
const TYPESCRIPT_LENGTH_READS: &str = "const rows: number[] = [0];
let total = 0;
for (let n = 0; n < 2000; n++) {
  total += rows.length;
}
finish(total);
";

/// A tenth of the rows, because `concat` copies the accumulator every step.
const TYPESCRIPT_CONCAT: &str = "let rows: number[] = [];
for (let n = 0; n < 200; n++) {
  rows = rows.concat([n]);
}
finish(rows.length);
";

fn typescript(source: &str) -> CompiledProgram {
    lash_typescript::compile(source).expect("dialect program should compile")
}

async fn typescript_bytes_per_iteration(source: &str, iterations: usize) -> f64 {
    bytes_per_iteration(&typescript(source), State::new, iterations).await
}

/// How far the dialect may sit above the AST program measured beside it.
///
/// The two programs are the same scenario, not the same instruction stream:
/// the dialect carries `for (const row of rows)` iteration and ECMA-shaped
/// arithmetic the hand-built AST does not. The measured ratio is 1.3x; the
/// headroom here is for that shape difference, not for a regression, and it is
/// three orders of magnitude below the 214x this scenario cost before FIG-3063.
const DIALECT_HEADROOM_OVER_AST: f64 = 1.5;

/// The TypeScript-authored `heap_list_iteration` measured beside the AST
/// program the corpus benchmarks.
///
/// Both figures are taken in this process with the same allocator, so the
/// comparison is like-for-like and does not depend on the build profile the
/// checked-in budgets were recorded under. The corpus scenario is also held to
/// its own checked-in budget, which anchors the ratio to something the perf
/// guard enforces rather than to a free-floating number.
#[tokio::test(flavor = "current_thread")]
async fn typescript_heap_list_iteration_is_measured_beside_the_ast_corpus() {
    let budget = budgeted_bytes_per_iter("heap_list_iteration", "compiled_execute");
    let ast = bytes_per_iteration(
        &ast_heap_list_iteration(),
        || seeded_state_for(Scenario::HeapListIteration),
        4,
    )
    .await;
    let dialect = typescript_bytes_per_iteration(TYPESCRIPT_HEAP_LIST_ITERATION, 4).await;

    assert!(
        ast <= budget,
        "the AST corpus scenario must still be inside its own checked-in budget: \
         measured {ast:.0} bytes/iter against {budget:.0}"
    );
    assert!(
        dialect <= ast * DIALECT_HEADROOM_OVER_AST,
        "the TypeScript-authored heap_list_iteration must stay within \
         {DIALECT_HEADROOM_OVER_AST}x of the AST program measured beside it: \
         dialect {dialect:.0} bytes/iter against AST {ast:.0} (corpus budget {budget:.0})"
    );
}

/// The three append spellings, measured beside the corpus so a regression names
/// the spelling that regressed.
///
/// `push` and the write one past the end are the O(1) appends FIG-3063
/// delivered, and they must now cost the same: the only difference the
/// measurement may show is the `.length` read the second spelling contains,
/// which is measured on its own and subtracted. `concat` is a copy in
/// JavaScript and stays one here, so it is held *above* a floor — fusing it
/// into the accumulator would change what the program means, not just what it
/// costs.
#[tokio::test(flavor = "current_thread")]
async fn each_append_spelling_is_measured_beside_the_corpus() {
    let ast = bytes_per_iteration(
        &ast_heap_list_iteration(),
        || seeded_state_for(Scenario::HeapListIteration),
        4,
    )
    .await;
    let push = typescript_bytes_per_iteration(TYPESCRIPT_PUSH, 4).await;
    let indexed = typescript_bytes_per_iteration(TYPESCRIPT_INDEX_ASSIGN, 4).await;
    let length_reads = typescript_bytes_per_iteration(TYPESCRIPT_LENGTH_READS, 4).await;
    let concatenated = typescript_bytes_per_iteration(TYPESCRIPT_CONCAT, 4).await;

    assert!(
        push <= ast * DIALECT_HEADROOM_OVER_AST,
        "2000 `push` appends must stay within {DIALECT_HEADROOM_OVER_AST}x of the AST corpus \
         scenario: {push:.0} bytes/iter against {ast:.0}"
    );
    // 1.1 covers measurement noise in the subtraction, not a second append:
    // an index assignment that cloned the backing vector again would land
    // hundreds of times over this line, not ten percent over it.
    assert!(
        indexed <= (push + length_reads) * 1.1,
        "a write one past the end must cost a `push` plus the `.length` read it spells: \
         indexed {indexed:.0} bytes/iter against push {push:.0} plus \
         {length_reads:.0} for the same number of `.length` reads"
    );
    assert!(
        concatenated > push,
        "`concat` must stay a copy: 200 concatenations measured {concatenated:.0} bytes/iter, \
         which is no more than the {push:.0} that 2000 in-place appends cost"
    );
}
