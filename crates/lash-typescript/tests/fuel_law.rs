//! FIG-3729's law: every loop back-edge and every call or hook path charges
//! the instruction budget.
//!
//! Each row is a guest program that can repeat only through the named path: a
//! loop whose back-edge is that construct, or a recursion whose only call is
//! that path's. Under a small instruction budget it must stop with the typed
//! `InstructionBudgetExceeded` error — not a hang and not a wall-clock
//! timeout. A row that never returns within [`WALL`] fails by name instead of
//! stalling the suite.
//!
//! The rows are the law's enumeration of the chargeable call and loop
//! surfaces: a construct that can loop or recurse a guest without charging is
//! a fuel leak, and a new one adds its row here.

use std::sync::mpsc;
use std::time::Duration;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, RuntimeError, State,
};

/// Small enough that a charged loop exhausts it in well under a second, large
/// enough that the program around the looping construct fits inside it.
const BUDGET: u64 = 2_000;

/// The per-row bound on "within a bounded wall time": a charged program at
/// this budget finishes in milliseconds, so seconds measure only a leak.
const WALL: Duration = Duration::from_secs(20);

struct BudgetHost;

impl ExecutionHost for BudgetHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "the fuel-law host answers no effect but finish",
            )),
        }
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::new(
            ExecutionBound::instructions(BUDGET),
            ExecutionBound::Bounded(lashlang::DEFAULT_HOST_MEMORY_LIMIT_BYTES),
        )
    }
}

fn run(source: &'static str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::testing::compile(source)
        .unwrap_or_else(|error| panic!("`{source}` compiles: {error}"));
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &BudgetHost))
}

/// The looping or recursive guest program for each charged path. Keep one row
/// per surface the instruction budget must cover; a construct that can drive
/// unbounded guest work needs a row here.
const CASES: &[(&str, &str)] = &[
    // Loop back-edges: classic for in its init/test/update forms, while and
    // do-while, for-of (live array and live collections), for-in, and a
    // labelled continue's cross-loop back-edge.
    ("classic-for", "for (;;) {}"),
    ("classic-for-init", "for (let i = 0;;) {}"),
    ("classic-for-update", "for (let i = 0;; i++) {}"),
    ("classic-for-var", "for (var i = 0;;) {}"),
    ("while", "while (true) {}"),
    ("do-while", "do {} while (true);"),
    (
        "for-of-live-array",
        "const a = [0]; for (const v of a) { a.push(v + 1); }",
    ),
    (
        "for-of-live-set",
        "const s = new Set([0]); for (const v of s) { s.add(v + 1); }",
    ),
    (
        "for-of-live-map",
        "const m = new Map([[0, 0]]); for (const e of m) { m.set(e[0] + 1, 0); }",
    ),
    ("for-in", "while (true) { for (const k in { a: 1 }) {} }"),
    // Call paths: a plain call, the recursive call the ticket names, member
    // calls, spread (dynamic) calls, and a built-in function value called
    // detached.
    ("call", "function f() { return 0; } while (true) { f(); }"),
    // The dialect rejects name self-recursion (TDZ / mutable-capture /
    // mutual-recursion gates), so the recursive row passes the callee as a
    // parameter — the repeated surface is still only the call instruction.
    ("call-recursion", "function f(g) { return g(g); } f(f);"),
    (
        "method-call",
        "const o = { m() { return 0; } }; while (true) { o.m(); }",
    ),
    (
        "call-dynamic",
        "const f = () => 0; const a = []; while (true) { f(...a); }",
    ),
    (
        "call-method-dynamic",
        "const o = { m() { return 0; } }; const a = []; while (true) { o.m(...a); }",
    ),
    (
        "builtin-method-value",
        "const t = ({}).toString; while (true) { t(); }",
    ),
    // Guest ToPrimitive hooks: a loop whose every iteration suspends for a
    // hook, and a hook chain whose only call is the hook itself — the
    // suspend-and-replay must not re-run hooks unboundedly.
    (
        "valueof-hook",
        "const o = { valueOf() { return 1; } }; while (true) { o + 1; }",
    ),
    (
        "tostring-hook",
        "const o = { toString() { return 'x'; } }; while (true) { `${o}`; }",
    ),
    (
        "valueof-hook-chain",
        "const o = { valueOf() { return this + 1; } }; o + 1;",
    ),
    // Callback drivers: each invocation charges over the body's own
    // instructions, so a callback that only re-enters its driver still
    // exhausts the budget.
    ("array-map", "[1].map(function g() { return [1].map(g); });"),
    (
        "array-foreach",
        "[1].forEach(function g() { [1].forEach(g); });",
    ),
    (
        "array-filter",
        "[1].filter(function g() { [1].filter(g); return true; });",
    ),
    (
        "array-reduce",
        "[1, 2].reduce(function g(a, b) { return [1, 2].reduce(g); });",
    ),
    (
        "array-find",
        "[1].find(function g() { [1].find(g); return false; });",
    ),
    (
        "array-flatmap",
        "[1].flatMap(function g() { [1].flatMap(g); return [1]; });",
    ),
    (
        "array-sort-comparator",
        "[2, 1].sort(function c(a, b) { [4, 3].sort(c); return a - b; });",
    ),
    (
        "array-from-mapper",
        "Array.from([1], function g() { Array.from([1], g); return 0; });",
    ),
    (
        "set-foreach-live",
        "const s = new Set([0]); s.forEach((v) => { s.add(v + 1); });",
    ),
    (
        "map-foreach-live",
        "const m = new Map([[0, 0]]); m.forEach((v, k) => { m.set(k + 1, 0); });",
    ),
    (
        "urlsearchparams-foreach",
        "new URLSearchParams('a=1').forEach(function g() { new URLSearchParams('a=1').forEach(g); });",
    ),
    (
        "json-parse-reviver",
        "JSON.parse('1', function r() { JSON.parse('1', r); return 1; });",
    ),
    (
        "json-stringify-replacer",
        "JSON.stringify({ a: 1 }, function r(k, v) { JSON.stringify({ a: 1 }, r); return v; });",
    ),
    // Regexp execution: each call charges its fuel grant to the budget.
    ("regexp-loop", "while (true) { /a/.test('a'); }"),
];

/// Charged paths that cannot run because the dialect refuses them at compile
/// time. The row keeps the construct in the law: the day the rejection lifts,
/// the construct needs a `CASES` row and its charge first.
const REJECTED_CASES: &[(&str, &str, &str)] = &[(
    "labelled-continue",
    "outer: for (;;) { continue outer; }",
    "TS_LABEL_UNSUPPORTED",
)];

#[test]
fn every_loop_back_edge_and_call_path_exhausts_the_budget() {
    let mut failures = Vec::new();
    for &(name, source) in CASES {
        // A row that leaks runs forever; handing it its own thread makes the
        // leak a named failure with every other row still reported.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run(source));
        });
        match rx.recv_timeout(WALL) {
            Ok(Err(RuntimeError::InstructionBudgetExceeded { limit })) if limit == BUDGET => {}
            Ok(outcome) => failures.push(format!("{name}: ended as {outcome:?}")),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                failures.push(format!("{name}: run panicked"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => failures.push(format!(
                "{name}: did not exhaust {BUDGET} instructions within {WALL:?}"
            )),
        }
    }
    for &(name, source, code) in REJECTED_CASES {
        match lash_typescript::testing::compile(source) {
            Err(diagnostic) if format!("{diagnostic}").contains(code) => {}
            Err(diagnostic) => {
                failures.push(format!("{name}: rejected with {diagnostic}, not {code}"))
            }
            Ok(_) => failures.push(format!("{name}: compiled uncharged")),
        }
    }
    assert!(
        failures.is_empty(),
        "paths that outran the instruction budget:\n{}",
        failures.join("\n")
    );
}
