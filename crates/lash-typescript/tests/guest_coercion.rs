//! ToPrimitive runs an object's own `valueOf`/`toString` (FIG-3652).
//!
//! Each conversion the VM performs calls the hooks in ECMA-262's hint order
//! with the object as their receiver, through the one call path every guest
//! call takes. The instruction that needed a hook reruns once the hook has
//! answered, so every answer below is Node v25.2.1's, hook call counts
//! included.

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
            _ => Err(ExecutionHostError::new("unsupported coercion-test ability")),
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
fn operators_convert_through_the_hooks_in_hint_order() {
    let cases = [
        ("const o = { valueOf() { return 7; } }; finish(o + 1);", "8"),
        (
            "const o = { toString() { return 'x'; } }; finish('a' + o);",
            "\"ax\"",
        ),
        (
            "const o = { toString() { return 'T'; }, valueOf() { return 5; } }; finish([o + 1, `${o}`, String(o), o * 2]);",
            "[6,\"T\",\"T\",10]",
        ),
        (
            "const n = { v: 2, valueOf() { return this.v; } }; finish([n * 3, n > 1, n == 2, -n, +n, ~n, n << 1, n & 3]);",
            "[6,true,true,-2,2,-3,4,2]",
        ),
        // Left operand's hook first, both after both operands are evaluated.
        (
            "const log: string[] = []; const a = { valueOf() { log.push('a'); return 1; } }; const b = { valueOf() { log.push('b'); return 2; } }; const s = a + b; finish([s, log]);",
            "[3,[\"a\",\"b\"]]",
        ),
        // A hook answering an object hands the conversion to the next one.
        (
            "const o = { valueOf() { return {}; }, toString() { return 'second'; } }; finish(o + '');",
            "\"second\"",
        ),
        // With no hook answering, the built-in toString's type tag does.
        (
            "const o = { valueOf() { return {}; } }; finish(`${o}`);",
            "\"[object Object]\"",
        ),
        (
            "finish([`${{}}`, String({}), '' + new Map(), String(new Set())]);",
            "[\"[object Object]\",\"[object Object]\",\"[object Map]\",\"[object Set]\"]",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished_json(source), expected, "{source}");
    }
}

#[test]
fn property_keys_and_built_in_arguments_convert_through_the_hooks() {
    let cases = [
        (
            "const k = { toString() { return 'key'; } }; const r: any = {}; r[k] = 1; finish(Object.keys(r));",
            "[\"key\"]",
        ),
        (
            "const k = { toString() { return 'a'; } }; finish({ [k]: 1 });",
            "{\"a\":1}",
        ),
        (
            "const k = { toString() { return 'a'; } }; const r: any = { a: 1 }; delete r[k]; finish(Object.keys(r).length);",
            "0",
        ),
        (
            "const a = { valueOf() { return 1; } }; const b = { valueOf() { return 3; } }; finish('abcdef'.slice(a, b));",
            "\"bc\"",
        ),
        (
            "const s = { toString() { return 'cd'; } }; finish(['abcdef'.indexOf(s), 'abcd'.includes(s), 'x'.concat(s)]);",
            "[2,true,\"xcd\"]",
        ),
        (
            "const i = { valueOf() { return 1; } }; finish([[1, 2, 3].indexOf(2, i), [1, 2, 3].fill(0, i), (3.14159).toFixed(i)]);",
            "[1,[1,0,0],\"3.1\"]",
        ),
        (
            "finish(parseInt('ff', { valueOf() { return 16; } }));",
            "255",
        ),
        (
            "finish(new Error({ toString() { return 'msg'; } }).message);",
            "\"msg\"",
        ),
        (
            "finish([encodeURIComponent({ toString() { return 'a b'; } }), String.fromCharCode({ valueOf() { return 65; } }), Math.max({ valueOf() { return 4; } }, 2)]);",
            "[\"a%20b\",\"A\",4]",
        ),
        (
            "finish(JSON.parse({ toString() { return '[1,2]'; } }));",
            "[1,2]",
        ),
        (
            "finish([/x/.test({ toString() { return 'x'; } }), 'xAB'.match({ toString() { return 'AB'; } })[0]]);",
            "[true,\"AB\"]",
        ),
        (
            "finish(['a', { toString() { return 'x'; } }].join('-'));",
            "\"a-x\"",
        ),
        (
            "finish(Object.groupBy([1, '1', { toString() { return 1; } }], (v) => v)['1'].length);",
            "3",
        ),
        // An empty array answers before converting fromIndex.
        (
            "const f = { valueOf() { throw new Error('no'); } }; finish([[].indexOf(1, f), [].includes(1, f)]);",
            "[-1,false]",
        ),
        // ToString(searchValue) before ToString(replaceValue).
        (
            "const s = { toString() { throw 'search'; } }; const r = { toString() { throw 'replace'; } }; let e: any; try { 'x'.replace(s, r); } catch (x) { e = x; } finish(e);",
            "\"search\"",
        ),
        // `copyWithin` runs ToInteger on target, start and end in order, on
        // an empty receiver too: the conversions precede the copy.
        (
            "const log: string[] = []; [1, 2].copyWithin({ valueOf() { log.push('t'); return 0; } }, { valueOf() { log.push('s'); return 0; } }, { valueOf() { log.push('e'); return 2; } }); finish(log.join(''));",
            "\"tse\"",
        ),
        (
            "let e: any; try { [].copyWithin(0, 0, { valueOf() { throw 'boom'; } }); } catch (x) { e = x; } finish(e);",
            "\"boom\"",
        ),
        (
            "finish([1, 2, 3, 4].copyWithin({ valueOf() { return 1; } }, { valueOf() { return 2; } }, { valueOf() { return 3; } }));",
            "[1,3,3,4]",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished_json(source), expected, "{source}");
    }
}

#[test]
fn each_conversion_calls_its_hook_once() {
    assert_eq!(
        finished_json(
            "const c = { n: 0 }; const o = { valueOf() { c.n = c.n + 1; return 1; } }; const r = 'abcdef'.slice(o, o); finish([r, c.n]);"
        ),
        "[\"\",2]"
    );
    assert_eq!(
        finished_json(
            "const c = { n: 0 }; const o = { valueOf() { c.n = c.n + 1; return c.n; } }; finish([o + o, c.n]);"
        ),
        "[3,2]"
    );
}

#[test]
fn a_hook_that_throws_ends_the_conversion_and_a_later_run_starts_afresh() {
    assert_eq!(
        finished_json(
            "const o = { valueOf() { throw 'boom'; } }; let e: any; try { 'abc'.slice(o); } catch (x) { e = x; } finish(e);"
        ),
        "\"boom\""
    );
    // The same instruction runs again after a caught throw: its hook runs
    // again rather than replaying the abandoned run's answers.
    assert_eq!(
        finished_json(
            "const seen: number[] = []; for (let i = 0; i < 3; i++) { try { 'x'.slice({ valueOf() { throw i; } }); } catch (e) { seen.push(e as number); } } finish(seen);"
        ),
        "[0,1,2]"
    );
    // Neither hook answers a primitive: TypeError.
    assert_eq!(
        finished_json(
            "const o = { valueOf() { return {}; }, toString() { return {}; } }; let r: any; try { o + 1; } catch (e) { r = e instanceof TypeError; } finish(r);"
        ),
        "true"
    );
}

#[test]
fn a_hook_may_convert_another_object() {
    assert_eq!(
        finished_json(
            "const inner = { valueOf() { return 2; } }; const outer = { valueOf() { return inner * 10; } }; finish(outer + 1);"
        ),
        "21"
    );
}

#[test]
fn a_hook_cannot_perform_an_effect() {
    let error = execute("const o = { valueOf() { finish(1); return 1; } }; finish(o + 1);")
        .expect_err("an effect inside a hook refuses");
    assert!(
        matches!(error, RuntimeError::EffectInBuiltinCallback),
        "{error:?}"
    );
}

#[test]
fn a_hook_that_recurses_hits_the_frame_limit() {
    let error =
        execute("const o = { valueOf(): number { return (this as any) + 1; } }; finish(o + 1);")
            .expect_err("an unbounded conversion must stop");
    assert!(
        matches!(error, RuntimeError::FrameDepthExceeded { .. }),
        "{error:?}"
    );
}

#[test]
fn converting_a_function_refuses_by_name() {
    for source in [
        "finish('' + (() => 1));",
        "finish(`${function () { return 1; }}`);",
        "const f: any = () => 1; finish(f * 2);",
        "const f: any = () => 1; finish({ [f]: 1 });",
    ] {
        let error = execute(source).expect_err(source);
        assert!(
            error.to_string().contains("TS_FUNCTION_STRING_COERCION"),
            "{source}: {error}"
        );
    }
}

/// Conversions before and after a park give the resident run's answer: no
/// conversion spans a park, and the resumed run replays nothing.
#[tokio::test(flavor = "current_thread")]
async fn conversions_around_a_park_match_the_resident_run() {
    let source = r#"
        const o = { v: 3, valueOf() { return this.v; }, toString() { return 'o' + this.v; } };
        const before = [o + 1, `${o}`, 'abcdef'.slice(o)];
        const got = await tools.ping({});
        o.v = got;
        finish(JSON.stringify([before, o + 1, `${o}`, 'abcdefghij'.slice(o)]));
    "#;
    let program = lash_typescript::testing::compile(source).expect("compiles");
    let resident = match lashlang::execute(&program, &mut State::new(), &Host)
        .await
        .expect("resident run")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    };
    assert_eq!(
        resident,
        Value::String("[[4,\"o3\",\"def\"],8,\"o7\",\"hij\"]".into())
    );
    let host = Host;
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let continuation = vm.suspend().expect("capturable");
    drop(vm);
    let bytes = serde_json::to_vec(&continuation).expect("serialize");
    let restored: VmContinuation = serde_json::from_slice(&bytes).expect("deserialize");
    let host = Host;
    let mut resumed = Vm::resume_from(restored, &program, &host).expect("resume");
    assert_eq!(
        resumed.run_process_until_effect().await.expect("finish"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(resident))
    );
}
