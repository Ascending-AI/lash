//! Mutable captures (FIG-3707, retired ADR 0062 deviation register entry 5).
//!
//! ECMA-262 closes over a binding, not a copy of it: a closure reads the
//! binding's current value and an assignment inside a closure writes the one
//! binding every other closure and the enclosing frame read. The dialect
//! copies a capture where nothing can assign it after the copy, and keeps
//! every other captured binding in one binding cell the frame and each closure
//! share. A top-level binding is the session slot itself, which a closure
//! reaches live.
//!
//! Every row below was refused as `TS_MUTABLE_CAPTURE_UNSUPPORTED` until
//! FIG-3707; each answer is Node v25.2.1's, except where a row names the
//! registered deviation that makes the dialect's answer its own.

use std::collections::BTreeSet;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionMode, ExecutionOutcome,
    State, Value, Vm, VmContinuation, VmRunOutcome,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            AbilityOp::ResourceOperation(_) => Ok(AbilityResult::Value(Value::Number(7.0))),
            _ => Err(ExecutionHostError::new(
                "unexpected mutable-capture ability",
            )),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Process
    }
}

fn finished(source: &str) -> Value {
    let program = lash_typescript::testing::compile(source)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
        .unwrap_or_else(|error| panic!("`{source}` should execute: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("`{source}` should finish, got {other:?}"),
    }
}

fn string(value: &str) -> Value {
    Value::String(value.into())
}

/// A closure reads the binding, not the copy it would have taken when it was
/// created: every assignment that follows it is visible.
#[test]
fn a_closure_reads_the_current_value_of_a_binding_assigned_after_it() {
    let cases = [
        // FIG-3599's `mutable-capture-read` row.
        (
            "finish((() => { let x = 1; const f = () => x; x = 2; return f(); })());",
            Value::Number(2.0),
        ),
        // The same at the top level of a cell, where the binding is the
        // session slot the closure reads live.
        (
            "let x = 1; const f = () => x; x = 2; finish(f());",
            Value::Number(2.0),
        ),
        // Every assignment form counts.
        (
            "let x = 1; const f = () => x; x++; finish(f());",
            Value::Number(2.0),
        ),
        (
            "let x = 1; const f = () => x; [x] = [5]; finish(f());",
            Value::Number(5.0),
        ),
        (
            "let x = 1; const f = () => x; for (x of [7]) { } finish(f());",
            Value::Number(7.0),
        ),
        // The store follows the value, which may create the closure.
        (
            "let x = 1; const fs: any[] = []; x = fs.push(() => x) + 1; finish(fs[0]());",
            Value::Number(2.0),
        ),
        // A closure made by another closure shares the same binding.
        (
            "let x = 1; const outer = () => () => x; const g = outer(); x = 2; finish(g());",
            Value::Number(2.0),
        ),
        // A hoisted declaration is created at the head of its block, before
        // an assignment that precedes it in the source.
        (
            "let x = 1; x = 2; function f(): number { return x; } finish(f());",
            Value::Number(2.0),
        ),
        (
            "let x; function f() { return x; } x = 1; finish(f());",
            Value::Number(1.0),
        ),
        // A `var` exists before its declaration runs, so its initializer is an
        // assignment.
        (
            "function f() { return x; } var x = 1; finish(f());",
            Value::Number(1.0),
        ),
        // Inside a function the binding is a cell the closure shares.
        (
            "function run() { let x = 1; const f = () => x; x = 2; return f(); } finish(run());",
            Value::Number(2.0),
        ),
        (
            "function run() { const f = () => x; var x = 1; return f(); } finish(run());",
            Value::Number(1.0),
        ),
        (
            "function run(p: number) { const f = () => p; p = p * 10; return f(); } finish(run(4));",
            Value::Number(40.0),
        ),
        (
            "function run() { try { throw 1; } catch (e) { const f = () => e; e = 2; return f(); } } finish(run());",
            Value::Number(2.0),
        ),
        // A later iteration's assignment follows an earlier iteration's
        // closure.
        (
            "const fs: any[] = []; let i = 0; while (i < 3) { i = i + 1; fs.push(() => i); } finish(JSON.stringify(fs.map((f) => f())));",
            string("[3,3,3]"),
        ),
        // A loop's test runs on every iteration, so the body's assignment
        // follows the test's closure.
        (
            "const fs: any[] = []; let n = 0; while (fs.push(() => n) < 3) { n = n + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            string("[2,2,2]"),
        ),
        // A per-iteration binding assigned after the closure, in the same
        // iteration: each closure has its own iteration's binding.
        (
            "const fs: any[] = []; for (let i = 0; i < 3; i++) { fs.push(() => i); i = i + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            string("[1,3]"),
        ),
        // A `for (var i ...)` head is one binding, not a per-iteration one.
        (
            "const fs: any[] = []; for (var i = 0; i < 3; i++) { fs.push(() => i); } finish(JSON.stringify(fs.map((f) => f())));",
            string("[3,3,3]"),
        ),
        (
            "function run() { const fs: any[] = []; for (var i = 0; i < 3; i++) { fs.push(() => i); } return fs.map((f) => f()); } finish(JSON.stringify(run()));",
            string("[3,3,3]"),
        ),
        (
            "const fs: any[] = []; for (let v of [1, 2]) { fs.push(() => v); v = 9; } finish(JSON.stringify(fs.map((f) => f())));",
            string("[9,9]"),
        ),
        (
            "function run() { const fs: any[] = []; for (let v of [1, 2]) { fs.push(() => v); v = v * 10; } return fs.map((f) => f()); } finish(JSON.stringify(run()));",
            string("[10,20]"),
        ),
        // A classic `for` update runs in the next iteration's copy, so its
        // own assignment follows a closure it creates.
        (
            "const fs: any[] = []; for (let i = 0; i < 2; i = i + 1 + fs.push(() => i) - fs.length) { } finish(JSON.stringify(fs.map((f) => f())));",
            string("[1,2]"),
        ),
        // The test runs in the iteration's copy, which the body then assigns.
        (
            "const fs: any[] = []; for (let i = 0; fs.push(() => i) < 3; ) { i = i + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            string("[1,2,2]"),
        ),
        // A top-level `var` is a property of the global object, so a
        // `globalThis` write assigns it.
        (
            "var x = 1; const f = () => x; globalThis.x = 2; finish(f());",
            Value::Number(2.0),
        ),
        // The dialect gives a top-level `let` the same session slot as its
        // `globalThis` property (register entry 19,
        // `global-object-aliases-lexical-bindings`), so these writes assign
        // the binding too, at the root or from inside a function. Node keeps
        // the two apart and answers 1.
        (
            "let x = 1; const f = () => x; globalThis.x = 2; finish(f());",
            Value::Number(2.0),
        ),
        (
            "let x = 1; const f = () => x; function g() { globalThis.x = 2; } g(); finish(f());",
            Value::Number(2.0),
        ),
        // Deleting the session slot deletes the binding the closure reads,
        // under the same entry; the closure then reads the absent slot as a
        // `globalThis.x` read does. Node answers 1.
        (
            "let x = 1; const f = () => x; delete globalThis.x; finish(typeof f());",
            string("undefined"),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A closure assigns the binding it captured: the enclosing frame and every
/// other closure over it see the write.
#[test]
fn a_closure_assigns_the_binding_it_captured() {
    let cases = [
        (
            "let n = 0; const f = () => { n = 5; }; f(); finish(n);",
            Value::Number(5.0),
        ),
        (
            "let seen = 0; function f(): number { try { return 1; } finally { seen = 1; } } f(); finish(seen);",
            Value::Number(1.0),
        ),
        (
            "var n = 0; const f = () => { n = 5; }; f(); finish(n);",
            Value::Number(5.0),
        ),
        (
            "const f = (p: number) => { const set = () => { p = 5; }; set(); return p; }; finish(f(0));",
            Value::Number(5.0),
        ),
        // The shape model-written code reaches for first.
        (
            "let n = 0; [1, 2, 3].forEach((x) => { n += x; }); finish(n);",
            Value::Number(6.0),
        ),
        (
            "function count(items: number[]) { let n = 0; items.forEach(() => { n++; }); return n; } finish(count([4, 5, 6]));",
            Value::Number(3.0),
        ),
        // Two closures share one binding: a counter and its reader.
        (
            "function counter() { let n = 0; return { inc: () => { n = n + 1; return n; }, get: () => n }; } const c = counter(); c.inc(); c.inc(); finish(c.get());",
            Value::Number(2.0),
        ),
        // Each call of the enclosing function makes a binding of its own.
        (
            "function counter() { let n = 0; return () => { n = n + 1; return n; }; } const a = counter(); const b = counter(); a(); a(); b(); finish(JSON.stringify([a(), b()]));",
            string("[3,2]"),
        ),
        // A nested closure writes through the closure between it and the
        // binding's frame.
        (
            "function run() { let n = 0; const outer = () => { const inner = () => { n = n + 10; }; inner(); }; outer(); outer(); return n; } finish(run());",
            Value::Number(20.0),
        ),
        // A member write through a captured, reassigned binding writes the
        // object the binding holds now.
        (
            "function run() { let o = { v: 1 }; const f = () => { o.v = o.v + 1; }; o = { v: 10 }; f(); return o.v; } finish(run());",
            Value::Number(11.0),
        ),
        // Each iteration's binding is its own: closures assigning them do not
        // interfere.
        (
            "const fs: any[] = []; for (let i = 0; i < 3; i++) { fs.push(() => { i = i * 10; return i; }); } finish(JSON.stringify(fs.map((f) => f())));",
            string("[0,10,20]"),
        ),
        (
            "function run() { const out: any[] = []; for (const k of ['a', 'b']) { let hits = 0; const hit = () => { hits++; }; hit(); hit(); out.push(k + hits); } return out; } finish(JSON.stringify(run()));",
            string("[\"a2\",\"b2\"]"),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// What stays a plain copy, with Node's answers. None of these can see an
/// assignment after its closure's copy, so the copy is exact.
#[test]
fn captures_no_later_assignment_reaches_read_what_node_reads() {
    let cases = [
        // A `const`, and a `let` nothing reassigns.
        (
            "const k = 3; const f = () => k; finish(f());",
            Value::Number(3.0),
        ),
        (
            "let k = 3; const f = () => k; finish(f());",
            Value::Number(3.0),
        ),
        // Reassigned only before the closure exists.
        (
            "let x = 1; x = 2; const f = () => x; finish(f());",
            Value::Number(2.0),
        ),
        (
            "let x = 1; x++; const f = () => x; finish(f());",
            Value::Number(2.0),
        ),
        // A classic `for` binding is per-iteration, and its increment writes
        // the next iteration's copy.
        (
            "const fs: any[] = []; for (let i = 0; i < 3; i++) { fs.push(() => i); } finish(JSON.stringify(fs.map((f) => f())));",
            string("[0,1,2]"),
        ),
        (
            "const fs: any[] = []; for (const v of [1, 2, 3]) { fs.push(() => v * 10); } finish(JSON.stringify(fs.map((f) => f())));",
            string("[10,20,30]"),
        ),
        // A binding declared in a loop body is fresh each iteration, even one
        // assigned before its closure.
        (
            "const fs: any[] = []; let i = 0; while (i < 3) { const snapshot = i; fs.push(() => snapshot); i = i + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            string("[0,1,2]"),
        ),
        (
            "const fs: any[] = []; let i = 0; while (i < 3) { let v = i; v = v * 2; fs.push(() => v); i = i + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            string("[0,2,4]"),
        ),
        (
            "const fs: any[] = []; for (let i = 0; i < 3; i++) { let label = 'item'; label = label + i; fs.push(() => label); } finish(JSON.stringify(fs.map((f) => f())));",
            string("[\"item0\",\"item1\",\"item2\"]"),
        ),
        // A classic `for` head runs once, in the copy the first iteration's
        // is taken from: nothing the loop assigns reaches a closure the head
        // made.
        (
            "let g: any = () => -1; for (let i = 0, f = () => i; i < 3; i++) { g = f; } finish(g());",
            Value::Number(0.0),
        ),
        (
            "const fs: any[] = []; for (let i = 0, f = () => i; i < 1; i++) { i = i + 0; fs.push(f); } finish(JSON.stringify(fs.map((f) => f())));",
            string("[0]"),
        ),
        // Each test runs in its iteration's copy, after the update wrote it.
        (
            "const fs: any[] = []; for (let i = 0; fs.push(() => i) <= 3; i++) { } finish(JSON.stringify(fs.map((f) => f())));",
            string("[0,1,2,3]"),
        ),
        // An accumulator read once the loop that assigns it is done.
        (
            "let total = 0; for (const n of [1, 2, 3]) { total = total + n; } finish(JSON.stringify([3, 6].map((v) => v / total)));",
            string("[0.5,1]"),
        ),
        // Mutation *through* a captured reference is not an assignment to the
        // binding.
        (
            "const state = { n: 0 }; const bump = () => { state.n = state.n + 1; }; bump(); bump(); finish(state.n);",
            Value::Number(2.0),
        ),
        (
            "let o = { n: 1 }; const f = () => o.n; o.n = 5; finish(f());",
            Value::Number(5.0),
        ),
        (
            "function h(a: number) { return () => a; } finish(h(4)());",
            Value::Number(4.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A session global is the session slot itself: a closure reads it live,
/// whichever cell declared it. A closure never outlives its cell
/// (`closure-boundary`), and a bare assignment to an earlier cell's binding
/// is refused, so the one way a cell assigns one is `globalThis.name`, which
/// the dialect aliases to the binding.
#[test]
fn a_closure_reads_a_session_global_live() {
    let globals = BTreeSet::from(["count".to_string()]);
    let cell = |source: &str| lash_typescript::parse_with_globals(source, &globals);

    cell("const f = () => count; finish(f());").expect("a capture nothing reassigns links");
    cell("globalThis.count = 2; const f = () => count; finish(f());")
        .expect("an assignment before the closure links");
    cell("const f = () => count; globalThis.count = 2; finish(f());")
        .expect("an assignment after the closure links; the closure reads the slot live");
}

/// The binding cells a suspended program holds survive its durable
/// continuation: the program parks at each of its effects in turn, its
/// continuation round-trips through the wire, and the resumed run finishes
/// with the answer an uninterrupted (resident) run gives. A rerun from the
/// start with the same recorded effect results (a cold replay) answers the
/// same again.
async fn resident_restored_and_replayed(source: &str) -> Value {
    let program = lash_typescript::testing::compile(source)
        .unwrap_or_else(|error| panic!("TypeScript should compile: {source}: {error}"));
    let run = || async {
        match lashlang::execute(&program, &mut State::new(), &Host)
            .await
            .unwrap_or_else(|error| panic!("{source}: {error}"))
        {
            ExecutionOutcome::Finished(value) => value,
            other => panic!("{source}: expected finish, got {other:?}"),
        }
    };
    let resident = run().await;
    assert_eq!(run().await, resident, "{source}: the cold replay diverged");
    let mut park_at = 1;
    loop {
        let host = Host;
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
        let mut parked = false;
        for _ in 0..park_at {
            match vm
                .run_process_until_effect()
                .await
                .unwrap_or_else(|error| panic!("{source}: {error}"))
            {
                VmRunOutcome::EffectCompleted => parked = true,
                VmRunOutcome::Complete(_) => {
                    parked = false;
                    break;
                }
            }
        }
        if !parked {
            assert!(park_at > 1, "{source}: the program must park at least once");
            return resident;
        }
        let continuation = vm.suspend().expect("the parked program must be capturable");
        drop(vm);
        let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
        let restored: VmContinuation =
            serde_json::from_slice(&bytes).expect("continuation should deserialize");
        let host = Host;
        let mut resumed =
            Vm::resume_from(restored, &program, &host).expect("continuation should resume");
        let restored = loop {
            match resumed
                .run_process_until_effect()
                .await
                .unwrap_or_else(|error| panic!("{source}: resumed: {error}"))
            {
                VmRunOutcome::EffectCompleted => {}
                VmRunOutcome::Complete(ExecutionOutcome::Finished(value)) => break value,
                VmRunOutcome::Complete(other) => {
                    panic!("{source}: expected finish, got {other:?}")
                }
            }
        };
        assert_eq!(
            restored, resident,
            "{source}: restored after effect {park_at} diverged from the resident run"
        );
        park_at += 1;
    }
}

/// A closure writes an outer binding while the program parks mid-loop: the
/// frame's cell and the closure's are the same cell after the restore, and
/// the counters agree with the resident run.
#[tokio::test(flavor = "current_thread")]
async fn a_closure_written_binding_survives_a_park_mid_loop() {
    let value = resident_restored_and_replayed(
        r#"
        async function tally(items: number[]) {
          let total = 0;
          const add = (n: number) => { total = total + n; };
          for (const item of items) {
            add(item);
            const bonus = await tools.ping({});
            add(bonus);
          }
          return total;
        }
        let hits = 0;
        const hit = () => { hits++; };
        hit();
        const result = await tally([1, 2]);
        hit();
        finish(JSON.stringify([result, hits]));
        "#,
    )
    .await;
    assert_eq!(value, string("[17,2]"));
}

/// Per-iteration cells survive a park inside the loop that makes them: each
/// closure keeps its own iteration's cell across the restore.
#[tokio::test(flavor = "current_thread")]
async fn per_iteration_cells_survive_a_park_inside_their_loop() {
    let value = resident_restored_and_replayed(
        r#"
        const fs: any[] = [];
        for (let i = 0; i < 3; i++) {
          fs.push(() => i);
          await tools.ping({});
          i = i + 0;
        }
        function run() {
          const gs: any[] = [];
          let n = 0;
          for (const k of [1, 2]) {
            gs.push(() => k + n);
          }
          n = 100;
          return gs;
        }
        const gs = run();
        const v = await tools.ping({});
        finish(JSON.stringify([fs.map((f) => f()), gs.map((g) => g()), v]));
        "#,
    )
    .await;
    assert_eq!(value, string("[[0,1,2],[101,102],7]"));
}

/// A closure that escapes an array callback, or any built-in that collects
/// callback results, still shares the bindings it captures: the collected
/// result is the closure itself, not a copy with copies of its cells (review
/// of #2211). Each answered 0 or 1 when results were isolated; the answers
/// are Node's.
#[tokio::test(flavor = "current_thread")]
async fn a_closure_escaping_a_callback_keeps_its_shared_bindings() {
    let cases = [
        (
            "function run() { let total = 0; const adders = [1, 2].map((k) => () => { total += k; }); adders.forEach((f) => f()); return total; } finish(run());",
            Value::Number(3.0),
        ),
        (
            "let n = 0; const [inc] = [() => ++n].filter(() => true); inc(); inc(); finish(n);",
            Value::Number(2.0),
        ),
        (
            "function run() { let n = 0; const [inc] = [() => ++n].filter(() => true); inc(); inc(); return n; } finish(run());",
            Value::Number(2.0),
        ),
        (
            "function run() { let t = 0; const f = [1].reduce((acc: any, k) => () => { t += k; }, null); f(); f(); return t; } finish(run());",
            Value::Number(2.0),
        ),
        (
            "function run() { let t = 0; const f: any = [() => { t++; }].find(() => true); f(); f(); return t; } finish(run());",
            Value::Number(2.0),
        ),
        (
            "function run() { let t = 0; const fs = Array.from([1, 2], (k) => () => { t += k; }); fs.forEach((f) => f()); return t; } finish(run());",
            Value::Number(3.0),
        ),
        (
            "function run() { let t = 0; const fs = [1, 2].flatMap((k) => [() => { t += k; }]); fs.forEach((f) => f()); return t; } finish(run());",
            Value::Number(3.0),
        ),
        // The object a callback returns is that object (ECMA identity).
        (
            "const o = {}; finish([1].map(() => o)[0] === o);",
            Value::Bool(true),
        ),
        (
            "const o = { n: 1 }; [1].map(() => o)[0].n = 5; finish(o.n);",
            Value::Number(5.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
    let value = resident_restored_and_replayed(
        r#"
        async function run() {
          let n = 0;
          const fs = await Promise.all([1, 2].map(async (k) => () => { n += k; }));
          fs.forEach((f) => f());
          await tools.ping({});
          fs.forEach((f) => f());
          return n;
        }
        finish(await run());
        "#,
    )
    .await;
    assert_eq!(value, Value::Number(6.0));
}

/// A `var` that redeclares a parameter is the parameter's binding, and its
/// initializer is an assignment to it (review of #2211: Node 6 and 5; the
/// redeclaration minted a fresh cell, or was never judged a write).
#[test]
fn a_var_redeclaring_a_parameter_is_the_same_binding() {
    let cases = [
        (
            "function h(a: number) { const g = () => a; var a = 5; a++; return g(); } finish(h(1));",
            Value::Number(6.0),
        ),
        (
            "function h(a: number) { const g = () => a; var a = 5; return g(); } finish(h(1));",
            Value::Number(5.0),
        ),
        (
            "function h(a: number) { var a; return a; } finish(h(4));",
            Value::Number(4.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// With parameter expressions, the body's `var` of a parameter's name is a
/// separate binding that starts with the parameter's value, so a closure a
/// default made keeps reading the parameter (ECMA-262
/// FunctionDeclarationInstantiation step 28; review of #2211: Node 1, lash 2).
#[test]
fn a_parameter_default_keeps_reading_the_parameter_the_body_var_shadows() {
    let cases = [
        (
            "function f(a: number, g = () => a) { var a; a = 2; return g(); } finish(f(1));",
            Value::Number(1.0),
        ),
        (
            "function f(a: number, g = () => a) { var a; return a; } finish(f(7));",
            Value::Number(7.0),
        ),
        (
            "function f(a: number, g = () => a) { var a = 3; return [a, g()]; } finish(JSON.stringify(f(1)));",
            string("[3,1]"),
        ),
        // Without parameter expressions the `var` is the parameter itself.
        (
            "function f(a: number) { const g = () => a; var a; a = 2; return g(); } finish(f(1));",
            Value::Number(2.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// `s = s + rhs` on a captured, assigned binding writes through its cell —
/// FIG-3733's fused `JavaScriptAddAssign` only fires on a plain slot, so a
/// cell-bound name lowers to `cell_set(s, cell_get(s) + rhs)` — and a closure
/// made before the append reads the grown text, as Node does.
#[test]
fn a_cell_bound_string_appends_through_its_cell() {
    let cases = [
        (
            "function run() { let s = 'a'; const f = () => s; s = s + 'b'; return f(); } finish(run());",
            string("ab"),
        ),
        (
            "function run() { let s = 'a'; const f = () => s; s += 'b'; s += 'c'; return f() + '|' + s; } finish(run());",
            string("abc|abc"),
        ),
        // The FIG-3733 concat loop, written through the cell.
        (
            "function run() { let s = ''; const f = () => s; for (const c of ['a', 'b', 'c']) { s = s + c; } return f(); } finish(run());",
            string("abc"),
        ),
        // A closure does the appending itself; the frame and a second
        // closure read what it wrote.
        (
            "function run() { let s = 'a'; const app = (x: string) => { s = s + x; }; const get = () => s; app('b'); app('c'); return get() + '|' + s; } finish(run());",
            string("abc|abc"),
        ),
        // At the top level the binding is the session slot itself, which the
        // fused opcode writes in place and the closure reads live.
        (
            "let s = 'a'; const f = () => s; s = s + 'b'; finish(f());",
            string("ab"),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A closure held by a `let` it captures is a heap cycle through its cell: it
/// runs, and a durable pause refuses it with a hint naming the shape and the
/// rewrite (register entry 4).
#[tokio::test(flavor = "current_thread")]
async fn a_closure_held_by_the_binding_it_captures_names_the_cycle_at_a_pause() {
    let source = r#"
        async function run() {
          let f: any = null;
          f = () => f;
          const v = await tools.ping({});
          return typeof f() + v;
        }
        finish(await run());
    "#;
    assert_eq!(finished(source), string("function7"));
    let program = lash_typescript::testing::compile(source).expect("compiles");
    let host = Host;
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let error = vm.suspend().expect_err("a cycle cannot be recorded");
    let message = error.to_string();
    assert!(
        message.contains("is held by a `let`/`var` binding it captures"),
        "{message}"
    );
}
