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
//! measures, per thread, the bytes the VM asks the allocator for while a
//! program runs, so a case running beside this one cannot move the figure; the
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
use std::cell::Cell;

use lashlang::{
    AbilityOp, AbilityResult, AssignPathStep, AssignTarget, BinaryOp, ExecutionHost,
    ExecutionHostError, ExecutionOutcome, Expr, Program, Snapshot, State, Value, compile_ast,
    execute,
};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

thread_local! {
    /// Allocated bytes, charged to the thread that asked for them.
    ///
    /// Per thread rather than per process for the reason FIG-3221 records in
    /// `dialect_cost.rs`: the cases here run concurrently under plain
    /// `cargo test`, and a process-global counter folds a sibling case's
    /// allocations into whatever window happens to be open. Each case measures
    /// on its own libtest thread and blocks on its own future there, so a
    /// thread's total over a window is exactly the run that window measured.
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

/// Charges `bytes` to the calling thread.
///
/// The key is `const`-initialised and holds a `Copy` type, so it registers no
/// destructor and this call allocates nothing — it cannot re-enter the
/// allocator.
fn charge_to_this_thread(bytes: usize) {
    let _ =
        ALLOCATED_BYTES.try_with(|counter| counter.set(counter.get().saturating_add(bytes as u64)));
}

/// What the calling thread has allocated so far.
fn allocated_bytes_on_this_thread() -> u64 {
    ALLOCATED_BYTES.try_with(Cell::get).unwrap_or(0)
}

struct CountingAllocator;

#[expect(
    unsafe_code,
    reason = "the per-append cost law is measured with a counting global allocator, and GlobalAlloc is an unsafe trait"
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

/// Only execution is measured: the program is compiled first, so the compiler's
/// allocations stay out of the figure. The probes are built from the IR rather
/// than authored: what they pin is the cost of the two lowered append forms,
/// and stating those forms is the only way to be sure the measurement is of
/// them (ADR 0096).
#[expect(
    clippy::expect_used,
    reason = "the measured append probe compiles and executes its fixture cell, per each message"
)]
fn run_measured(program: &Program) -> (Value, u64) {
    let compiled = compile_ast(program).expect("cost probe should compile");
    let mut state = State::new();
    let before = allocated_bytes_on_this_thread();
    let outcome = futures::executor::block_on(execute(&compiled, &mut state, &Host))
        .expect("cost probe should execute");
    let allocated = allocated_bytes_on_this_thread() - before;
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("cost probe must finish");
    };
    (value, allocated)
}

fn var(name: &str) -> Expr {
    Expr::Variable(name.into())
}

fn builtin(name: &str, args: Vec<Expr>) -> Expr {
    Expr::BuiltinCall {
        name: name.into(),
        args,
    }
}

fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

/// `appended = __typescript_stdlib("push", items, <member>)` — what `xs.push(m)`
/// lowers to.
fn push_append(member: Expr) -> Expr {
    assign(
        "appended",
        builtin(
            "__typescript_stdlib",
            vec![Expr::String("push".into()), var("items"), member],
        ),
    )
}

/// `items[items.length] = <member>` — what a terminal index write lowers to.
fn index_append(member: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget {
            root: "items".into(),
            steps: vec![AssignPathStep::Index(Expr::Field {
                target: Box::new(var("items")),
                field: "length".into(),
            })],
        },
        expr: Box::new(member),
    }
}

/// The loop every probe shares, with `body` as its one statement. `finish`
/// reports a number rather than the list so the export at the end of the run
/// does not itself walk what was built.
///
/// ```text
/// items = []
/// total = 0
/// for i in range(0, <iterations>) { <body> }
/// finish total + len(items)
/// ```
fn probe_program(body: Expr, iterations: usize) -> Program {
    Program::block(vec![
        assign("items", Expr::List(Vec::new())),
        assign("total", Expr::Number(0.0)),
        Expr::For {
            binding: "i".into(),
            iterable: Box::new(builtin(
                "range",
                vec![Expr::Number(0.0), Expr::Number(iterations as f64)],
            )),
            body: Box::new(Expr::Block(vec![body])),
        },
        Expr::Finish(Box::new(Expr::Binary {
            op: BinaryOp::Add,
            left: Box::new(var("total")),
            right: Box::new(builtin("len", vec![var("items")])),
        })),
    ])
}

/// Bytes one append costs, with the loop, the range and the scaffolding the
/// append does not pay for subtracted out.
fn bytes_per_append(body: fn() -> Expr, iterations: usize) -> f64 {
    let (built, with_append) = run_measured(&probe_program(body(), iterations));
    let baseline = assign(
        "total",
        Expr::Binary {
            op: BinaryOp::Add,
            left: Box::new(var("total")),
            right: Box::new(var("i")),
        },
    );
    let (baseline_value, without_append) = run_measured(&probe_program(baseline, iterations));
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
fn assert_per_append_cost_is_flat(label: &str, body: fn() -> Expr) {
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
    assert_per_append_cost_is_flat("push", || push_append(var("i")));
}

#[test]
fn terminal_index_assignment_costs_the_same_at_every_list_length() {
    assert_per_append_cost_is_flat("index append", || index_append(var("i")));
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
    // items = []
    // for i in range(0, 64) {
    //     items[items.length] = "member-" + to_string(i)
    //     appended = __typescript_stdlib("push", items, [i, "nested"])
    //     also = __typescript_stdlib("push", items, i)
    // }
    // finish items.length
    let program = Program::block(vec![
        assign("items", Expr::List(Vec::new())),
        Expr::For {
            binding: "i".into(),
            iterable: Box::new(builtin(
                "range",
                vec![Expr::Number(0.0), Expr::Number(64.0)],
            )),
            body: Box::new(Expr::Block(vec![
                index_append(Expr::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(Expr::String("member-".into())),
                    right: Box::new(builtin("to_string", vec![var("i")])),
                }),
                push_append(Expr::List(vec![var("i"), Expr::String("nested".into())])),
                assign(
                    "also",
                    builtin(
                        "__typescript_stdlib",
                        vec![Expr::String("push".into()), var("items"), var("i")],
                    ),
                ),
            ])),
        },
        Expr::Finish(Box::new(Expr::Field {
            target: Box::new(var("items")),
            field: "length".into(),
        })),
    ]);
    let compiled = compile_ast(&program).expect("byte-accounting probe should compile");
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
    // finish items.length
    let compiled = compile_ast(&Program::block(vec![Expr::Finish(Box::new(Expr::Field {
        target: Box::new(var("items")),
        field: "length".into(),
    }))]))
    .expect("restored probe should compile");
    let outcome = futures::executor::block_on(execute(&compiled, &mut restored, &Host))
        .expect("restored probe should execute");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(192.0)));
}
