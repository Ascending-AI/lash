//! Call receivers (FIG-3700): a function or method's `this` is the receiver
//! of the call that runs it, and an arrow's `this` is its enclosing
//! function's. Every row below answered `undefined` (or was refused) before
//! receivers existed, and each answer is Node v25.2.1's.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionMode, ExecutionOutcome,
    RuntimeError, State, Value, Vm, VmContinuation, VmRunOutcome,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            AbilityOp::ResourceOperation(_) => Ok(AbilityResult::Value(Value::Number(7.0))),
            _ => Err(ExecutionHostError::new("unsupported receiver-test ability")),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Process
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::testing::compile(source)
        .unwrap_or_else(|error| panic!("TypeScript should compile: {source}: {error}"));
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn finished_json(source: &str) -> String {
    match execute(source).unwrap_or_else(|error| panic!("{source}: {error}")) {
        ExecutionOutcome::Finished(value) => {
            serde_json::to_string(&value).unwrap_or_else(|error| panic!("{source}: {error}"))
        }
        other => panic!("{source}: expected finish, got {other:?}"),
    }
}

#[test]
fn member_calls_bind_their_receiver() {
    let cases = [
        // Object-literal method and function-valued property.
        (
            "const o = { k: 1, f() { return this.k; } }; finish(o.f());",
            "1",
        ),
        (
            "const o = { k: 2, f: function () { return this.k; } }; finish(o.f());",
            "2",
        ),
        // A computed member and a parenthesized member keep the reference.
        (
            "const o = { k: 3, f() { return this.k; } }; const m = 'f'; finish(o[m]());",
            "3",
        ),
        (
            "const o = { k: 4, f() { return this.k; } }; finish((o.f)());",
            "4",
        ),
        // The receiver is the object the method is read from, not its owner.
        (
            "const o = { k: 5, f() { return this.k; } }; const p = { k: 6, g: o.f }; finish(p.g());",
            "6",
        ),
        (
            "const o = { a: { k: 7, f() { return this.k; } } }; finish(o.a.f());",
            "7",
        ),
        // A method mutates its receiver.
        (
            "const o = { n: 0, inc() { this.n++; return this.n; } }; o.inc(); finish(o.inc());",
            "2",
        ),
        // Optional chains keep the receiver.
        (
            "const o = { k: 8, f() { return this.k; } }; finish(o?.f());",
            "8",
        ),
        (
            "const o = { k: 9, f() { return this.k; } }; finish(o.f?.());",
            "9",
        ),
        (
            "const o = { k: 10, f() { return this.k; } }; const m = 'f'; finish(o?.[m]());",
            "10",
        ),
        // Parentheses keep the reference: `(a?.b)()` calls with `a`.
        (
            "const a = { b() { return this._b; }, _b: { c: 42 } }; finish([(a?.b)().c, (a.b)?.().c, a?.b?.().c, (a?.b)?.().c]);",
            "[42,42,42,42]",
        ),
        (
            "const a: any = null; let threw = false; try { (a?.b)(); } catch (e) { threw = true; } finish([threw, (a?.b)?.() === undefined]);",
            "[true,true]",
        ),
        // A spread argument list keeps the receiver.
        (
            "const o = { k: 11, f(a: number, b: number) { return this.k + a + b; } }; const xs = [1, 2]; finish(o.f(...xs));",
            "14",
        ),
        // An own `toString` is the object's method, called with the object.
        (
            "const o = { s: 'x', toString() { return this.s; } }; finish(o.toString());",
            "\"x\"",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished_json(source), expected, "{source}");
    }
}

#[test]
fn arrows_read_their_enclosing_functions_receiver() {
    assert_eq!(
        finished_json(
            "const o = { k: 3, f() { return [1, 2].map((x) => this.k + x); } }; finish(o.f());"
        ),
        "[4,5]"
    );
    assert_eq!(
        finished_json(
            "const o = { k: 3, f() { const g = () => () => this.k; return g()(); } }; finish(o.f());"
        ),
        "3"
    );
}

#[test]
fn plain_calls_bind_an_undefined_receiver() {
    let cases = [
        "function f() { return this; } finish(f() === undefined);",
        "const o = { f() { function g() { return this; } return g(); } }; finish(o.f() === undefined);",
        "const o = { f() { return this; } }; const g = o.f; finish(g() === undefined);",
    ];
    for source in cases {
        assert_eq!(finished_json(source), "true", "{source}");
    }
}

#[test]
fn builtin_callbacks_receive_their_this_arg() {
    let cases = [
        (
            "const t = { k: 10 }; finish([1, 2].map(function (x) { return this.k + x; }, t));",
            "[11,12]",
        ),
        (
            "const t = { k: 1 }; finish([1, 2].filter(function (x) { return x > this.k; }, t));",
            "[2]",
        ),
        (
            "const t = { k: 2 }; finish([1, 2].find(function (x) { return x === this.k; }, t));",
            "2",
        ),
        (
            "const t = { k: 2 }; finish([1, 2].some(function (x) { return x === this.k; }, t));",
            "true",
        ),
        (
            "const t = { k: 10 }; const out: number[] = []; [1].forEach(function (x) { out.push(this.k + x); }, t); finish(out);",
            "[11]",
        ),
        (
            "const t = { k: 10 }; const out: number[] = []; new Map([[1, 2]]).forEach(function (v, k) { out.push(this.k + v + k); }, t); finish(out);",
            "[13]",
        ),
        (
            "const t = { k: 10 }; const out: number[] = []; new Set([1]).forEach(function (v) { out.push(this.k + v); }, t); finish(out);",
            "[11]",
        ),
        (
            "const t = { k: 10 }; finish(Array.from([1, 2], function (x) { return this.k + x; }, t));",
            "[11,12]",
        ),
        // Without a thisArg the callback's receiver is undefined.
        (
            "finish([1].map(function () { return this === undefined; }));",
            "[true]",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished_json(source), expected, "{source}");
    }
}

#[test]
fn json_hooks_receive_their_holder() {
    assert_eq!(
        finished_json(
            "finish(JSON.stringify({ a: 1, b: { v: 'z', toJSON(key: string) { return this.v + key; } } }));"
        ),
        "\"{\\\"a\\\":1,\\\"b\\\":\\\"zb\\\"}\""
    );
    assert_eq!(
        finished_json(
            "finish(JSON.stringify({ a: 1 }, function (key, value) { return key === 'a' ? this.a + 1 : value; }));"
        ),
        "\"{\\\"a\\\":2}\""
    );
}

#[test]
fn a_method_the_receiver_lacks_fails_the_call() {
    for source in [
        "const s = 'a,b'; finish(s.notAMethod(','));",
        "const o = { a: 1 }; const p: any = o; finish(p.missing());",
    ] {
        let error = execute(source).expect_err(source);
        assert!(
            matches!(error, RuntimeError::NonFunctionCall { .. }),
            "{source}: {error}"
        );
    }
}

#[test]
fn top_level_this_is_refused_even_through_an_arrow() {
    for source in ["finish(this);", "const f = () => this; finish(f());"] {
        let error = lash_typescript::testing::compile(source).expect_err(source);
        assert_eq!(
            error.code,
            lash_typescript::DiagnosticCode::ThisUnsupported,
            "{source}"
        );
    }
}

/// Parks the program at its first effect, round-trips the continuation through
/// its wire form, and resumes it: the finish value must equal the resident run.
async fn resident_and_restored(source: &str) -> (Value, Value) {
    let program = lash_typescript::testing::compile(source)
        .unwrap_or_else(|error| panic!("TypeScript should compile: {source}: {error}"));
    let resident = match lashlang::execute(&program, &mut State::new(), &Host)
        .await
        .unwrap_or_else(|error| panic!("{source}: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("{source}: expected finish, got {other:?}"),
    };
    let host = Host;
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let continuation = vm.suspend().expect("the parked turn must be capturable");
    drop(vm);
    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should deserialize");
    let host = Host;
    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    let restored = match resumed
        .run_process_until_effect()
        .await
        .expect("the resumed turn should finish")
    {
        VmRunOutcome::Complete(ExecutionOutcome::Finished(value)) => value,
        other => panic!("{source}: expected finish, got {other:?}"),
    };
    (resident, restored)
}

/// A method parks on a tool call with its receiver in its frame; the
/// restored frame still writes through `this` to the one object the caller
/// holds.
#[tokio::test(flavor = "current_thread")]
async fn a_method_parked_on_a_tool_call_keeps_its_receiver() {
    let (resident, restored) = resident_and_restored(
        r#"
        const o = { n: 1, async bump() { this.n = this.n + 1; const v = await tools.ping({}); this.n = this.n + v; return this.n; } };
        const r = o.bump();
        finish(JSON.stringify([r, o.n]));
        "#,
    )
    .await;
    assert_eq!(resident, Value::String("[9,9]".into()));
    assert_eq!(restored, resident);
}

/// Writes through `this` before and after a park land on the object the
/// caller holds, resident and restored alike.
#[tokio::test(flavor = "current_thread")]
async fn writes_through_this_survive_a_park_between_calls() {
    let (resident, restored) = resident_and_restored(
        r#"
        const o = { n: 0, inc() { this.n = this.n + 1; return this; } };
        const same = o.inc() === o;
        const v = await tools.ping({});
        o.inc();
        finish(JSON.stringify([same, o.n, v]));
        "#,
    )
    .await;
    assert_eq!(resident, Value::String("[true,2,7]".into()));
    assert_eq!(restored, resident);
}

/// A plain object's own method named like a built-in is its method, and the
/// built-in lowering's generated code never runs on it (review of #2187).
#[test]
fn a_plain_objects_own_method_wins_over_a_built_in_name() {
    let cases = [
        (
            "const o = { k: 2, map(f: any) { return f(this.k); }, filter() { return 'own'; }, some() { return 'own'; } }; finish([o.map((x: number) => x + 1), o.filter((x: any) => x), o.some((x: any) => x)]);",
            "[3,\"own\",\"own\"]",
        ),
        (
            "finish({ k: 1, sort(c: any) { return this.k; } }.sort((a: number, b: number) => a - b));",
            "1",
        ),
        (
            "const o = { k: 1, toSorted(c: any) { return this.k; } }; finish(o.toSorted((a: number, b: number) => a - b));",
            "1",
        ),
        (
            "const o = { v: 5, reduce(f: any, init: number) { return f(init, this.v); } }; finish(o.reduce((a: number, b: number) => a + b, 10));",
            "15",
        ),
        (
            "finish({ hasOwnProperty(k: string) { return 'own'; } }.hasOwnProperty('a'));",
            "\"own\"",
        ),
        (
            "finish({ replace(a: string, fn: any) { return 'own:' + fn(); }, indexOf() { return 0; } }.replace('x', () => 'y'));",
            "\"own:y\"",
        ),
        (
            "const r = { split(a: string, b: any) { return [a, b === undefined]; } }; finish(r.split(','));",
            "[\",\",true]",
        ),
        (
            "const r = { test(s: string) { return 'own:' + s; } }; finish(r.test('q'));",
            "\"own:q\"",
        ),
        (
            "const o = { k: 3, map(f: any) { return f(this.k); } }; finish(o.map(async (x: number) => x));",
            "3",
        ),
        // The call keeps its own arguments: no generated wrapper, no dropped
        // thisArg.
        (
            "const f = () => 1; const p = { forEach(cb: any, t: any) { return [typeof t, cb === f]; } }; finish(p.forEach(f, 7));",
            "[\"number\",true]",
        ),
        // A plain object without the member answers only Object.prototype's.
        (
            "const o: any = { a: 1 }; finish([o.hasOwnProperty('a'), o.hasOwnProperty('b')]);",
            "[true,false]",
        ),
        // Built-in receivers are unchanged.
        (
            "finish([[3, 1, 2].toSorted((a, b) => a - b), 'abc'.replace('b', () => 'X'), 'a,b'.split(','), [1].hasOwnProperty(0)]);",
            "[[1,2,3],\"aXc\",[\"a\",\"b\"],true]",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished_json(source), expected, "{source}");
    }
}

/// Calling a plain object's member that is not a function, or one it lacks,
/// throws the TypeError ECMA-262's Call does.
#[test]
fn a_plain_objects_missing_or_non_callable_built_in_name_is_a_type_error() {
    for source in [
        "const o: any = { get: 5 }; let r = 'no'; try { o.get('a'); } catch (e) { r = e instanceof TypeError ? 'TypeError' : 'other'; } finish(r);",
        "const o: any = { a: 1 }; let r = 'no'; try { o.slice(1); } catch (e) { r = e instanceof TypeError ? 'TypeError' : 'other'; } finish(r);",
        "const o: any = { a: 1 }; let r = 'no'; try { o.replace('x', () => 'y'); } catch (e) { r = e instanceof TypeError ? 'TypeError' : 'other'; } finish(r);",
        "const o: any = { a: 1 }; let r = 'no'; try { o.match('x'); } catch (e) { r = e instanceof TypeError ? 'TypeError' : 'other'; } finish(r);",
        "const o: any = { a: 1 }; let r = 'no'; try { o.map((x: any) => x); } catch (e) { r = e instanceof TypeError ? 'TypeError' : 'other'; } finish(r);",
    ] {
        assert_eq!(finished_json(source), "\"TypeError\"", "{source}");
    }
}
