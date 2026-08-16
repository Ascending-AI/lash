use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionOutcome, RuntimeError, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected construct-test ability")),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program =
        lash_typescript::compile(source).unwrap_or_else(|error| panic!("{source}: {error}"));
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn finished(source: &str) -> Value {
    match execute(source).unwrap_or_else(|error| panic!("{source}: {error}")) {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[test]
fn destructuring_defaults_rest_and_assignment_are_exact() {
    assert_eq!(
        finished(
            "const {a, nested: [b = 4], ...rest} = {a:1,nested:[undefined],z:3}; let x=0; let y=0; [x,y] = [7,8]; finish(`${a}|${b}|${rest.z}|${x}|${y}`);"
        ),
        Value::String("1|4|3|7|8".into())
    );
}

#[test]
fn optional_chains_short_circuit_the_complete_tail() {
    assert_eq!(
        finished("let n=0; const a=null; const x=a?.b[n++].c; finish(`${x}|${n}`);"),
        Value::String("undefined|0".into())
    );
    assert_eq!(
        finished("const f=undefined; finish(f?.());"),
        Value::Undefined
    );
}

#[test]
fn compound_and_update_references_are_evaluated_once() {
    assert_eq!(
        finished(
            "let i=0; const a=[2]; const old=a[i++]++; a[0] **= 3; a[0] ||= 99; finish(`${old}|${a[0]}|${i}`);"
        ),
        Value::String("2|27|1".into())
    );
}

#[test]
fn literal_and_call_spread_preserve_order() {
    assert_eq!(
        finished(
            "function f(a,b,c){return `${a}${b}${c}`;} const a=[...'ab','c']; const o={x:1,...{y:2},['z']:3}; finish(`${f(...a)}|${o.x}${o.y}${o.z}`);"
        ),
        Value::String("abc|123".into())
    );
}

#[test]
fn control_flow_and_property_queries_lower_without_new_vm_ops() {
    assert_eq!(
        finished(
            "let out=''; switch(2){case 1:out+='a';break;default:out+='d';case 2:out+='b';case 3:out+='c';} let i=0; do{i++;}while(i<2); for(const k in {b:1,1:2,a:3}){out+=k;} finish(`${out}|${i}|${'a' in {a:1}}`);"
        ),
        Value::String("bc1ba|2|true".into())
    );
}

#[test]
fn bitwise_and_shift_operators_use_ecma_int32_rules() {
    assert_eq!(
        finished("finish(`${-1 >>> 1}|${2147483647 << 1}|${~0}|${5 & 3}|${5 | 2}|${5 ^ 1}`);"),
        Value::String("2147483647|-2|-1|1|7|4".into())
    );
}

#[test]
fn error_constructors_and_instanceof_use_heap_kinds() {
    assert_eq!(
        finished(
            "const e=new TypeError('bad',{cause:7}); finish(`${e.name}|${e.message}|${e.cause}|${e instanceof TypeError}|${e instanceof Error}`);"
        ),
        Value::String("TypeError|bad|7|true|true".into())
    );
    assert_eq!(
        finished(
            "const a=new SyntaxError(); const b=new ReferenceError(); const c=new URIError(); const d=new EvalError(); const e=new AggregateError([]); finish(`${a.name},${b.name},${c.name},${d.name},${e.name}`);"
        ),
        Value::String("SyntaxError,ReferenceError,URIError,EvalError,AggregateError".into())
    );
    let error = lash_typescript::compile("finish({} instanceof Promise);")
        .expect_err("Promise remains unavailable as an instanceof RHS");
    assert_eq!(
        error.code,
        lash_typescript::DiagnosticCode::InstanceOfUnsupported
    );
    assert!(error.message.contains("Unsupported:"));
    let stack = lash_typescript::compile("finish(new Error('x').stack);")
        .expect_err("nondeterministic stack must reject");
    assert!(stack.message.contains("stack"));
}

#[test]
fn date_utc_surface_is_complete_and_iso_only() {
    assert_eq!(
        finished(
            "const d=new Date(Date.UTC(2000,1,29,23,58,57,456)); finish(`${d.getUTCFullYear()}|${d.getUTCMonth()}|${d.getUTCDate()}|${d.getUTCDay()}|${d.getUTCHours()}|${d.getUTCMinutes()}|${d.getUTCSeconds()}|${d.getUTCMilliseconds()}|${d.getTime()}|${d.valueOf()}|${d.toISOString()}|${d.toJSON()}`);"
        ),
        Value::String(
            "2000|1|29|2|23|58|57|456|951868737456|951868737456|2000-02-29T23:58:57.456Z|2000-02-29T23:58:57.456Z".into()
        )
    );
    assert_eq!(
        finished(
            "finish(`${new Date(8640000000000000).toISOString()}|${new Date(-8640000000000000).toISOString()}|${new Date(NaN).toJSON()}|${Number.isNaN(Date.parse('2020-13-01'))}`);"
        ),
        Value::String("+275760-09-13T00:00:00.000Z|-271821-04-20T00:00:00.000Z|null|true".into())
    );
    let error = execute("finish(Date.parse('March 1, 2020'));")
        .expect_err("implementation-defined parse fallback must reject");
    assert!(
        error.to_string().contains("TS_DATE_PARSE_NON_ISO"),
        "{error}"
    );
}

#[test]
fn date_string_coercion_rejects_inside_containers_and_error_messages() {
    for source in [
        "finish('' + [new Date(0)]);",
        "finish(new Error(new Date(0)).message);",
        "finish(new Error([new Date(0)]).message);",
    ] {
        let error = execute(source).expect_err("Date string coercion remains a loud deviation");
        assert!(
            error
                .to_string()
                .contains("TS_DATE_STRING_COERCION_PENDING")
                && error.to_string().contains("toISOString"),
            "{source}: {error}"
        );
    }

    assert_eq!(
        finished("const a=new Date(1); const b=new Date(4); finish(`${b-a}|${a<b}`);"),
        Value::String("3|true".into())
    );
    for source in [
        "finish(new Date(0) + '');",
        "finish(`${new Date(0)}`);",
        "finish(String(new Date(0)));",
    ] {
        let error = execute(source).expect_err("Date string coercion remains a loud deviation");
        assert!(
            error
                .to_string()
                .contains("TS_DATE_STRING_COERCION_PENDING")
                && error.to_string().contains("toISOString"),
            "{source}: {error}"
        );
    }
}

#[test]
fn enums_match_tsc_runtime_objects_and_const_members_inline() {
    assert_eq!(
        finished(
            "let n=0; enum Numeric { A, B=4, C, D=(n+=2) } enum Text { A='x', B='x'+'y' } enum Referenced { A=Text.A } const enum Inline { A, B=4, C='z', D=Referenced.A, E=4294967296|0 } function scoped(){const enum Inline { A=9 } return Inline.A;} finish(`${Numeric.A}|${Numeric[0]}|${Numeric.B}|${Numeric[5]}|${Numeric.D}|${Numeric[2]}|${Text.A}|${Text.B}|${Object.keys(Text).join(',')}|${Object.keys(Referenced).join(',')}|${Inline.A}|${Inline.C}|${Inline.D}|${Inline.E}|${scoped()}|${n}`);"
        ),
        Value::String("0|A|4|C|2|D|x|xy|A,B|A|0|z|x|0|9|2".into())
    );
}

#[test]
fn map_and_set_surface_preserves_same_value_zero_identity_and_order() {
    assert_eq!(
        finished(
            "const key={}; const other={}; const m=new Map([[NaN,'nan'],[-0,'zero'],[key,'id']]); m.set(+0,'updated'); const calls={text:''}; m.forEach((v,k)=>{calls.text=calls.text+(calls.text?',':'')+`${v}:${k===key}`;}); const removed=m.delete(other); const has=m.has(key); const size=m.size; const order=[...m].map(([k,v])=>v).join(','); m.clear(); const s=new Set([NaN,NaN,-0,+0,2]); const setOrder=[...s].join(','); const deleted=s.delete(NaN); const setSize=s.size; s.clear(); finish(`${m.get(key)}|${has}|${removed}|${size}|${order}|${calls.text}|${m.size}|${setOrder}|${deleted}|${setSize}|${s.size}`);"
        ),
        Value::String(
            "undefined|true|false|3|nan,updated,id|nan:false,updated:false,id:true|0|NaN,0,2|true|2|0".into()
        )
    );
}

#[test]
fn delete_preserves_alias_identity_and_rejects_array_holes() {
    assert_eq!(
        finished(
            "const o={a:1,b:2}; const alias=o; const ok=delete o.a; finish(`${ok}|${'a' in alias}|${alias.b}`);"
        ),
        Value::String("true|false|2".into())
    );
    let error = execute("const a=[1]; delete a[0]; finish(a);")
        .expect_err("dense arrays cannot represent delete-created holes");
    assert!(
        error
            .to_string()
            .contains("TS_DELETE_ARRAY_INDEX_UNSUPPORTED")
    );
}

#[test]
fn var_hoisting_conversions_iterators_and_global_state_forms_work() {
    assert_eq!(
        finished(
            "function f(){const before=x; if(true){var x=3;} return `${before}|${x}`;} const values=[0,1,2].filter(Boolean); const pairs=[...values.entries()]; globalThis.state ??= {}; globalThis.state.n=4; finish(`${f()}|${parseInt('10',2)}|${isNaN('x')}|${pairs[0][0]}:${pairs[0][1]}|${globalThis.state.n}|${'state' in globalThis}`);"
        ),
        Value::String("undefined|3|2|true|0:1|4|true".into())
    );
}

#[test]
fn parameter_defaults_rest_and_destructuring_run_in_parameter_order() {
    assert_eq!(
        finished(
            "function f(a=1,{b}={b:a+1},...rest){return `${a}|${b}|${rest.join(',')}`;} finish(f(undefined,undefined,3,4));"
        ),
        Value::String("1|2|3,4".into())
    );
}

#[test]
fn classic_for_creates_per_iteration_closure_values() {
    lash_typescript::compile(
        "function run(){let first=()=>-1; let second=()=>-1; for(let i=0;i<2;i++){if(i===0){first=()=>i;}else{second=()=>i;}} return `${first()}|${second()}`;} finish(run());",
    )
    .expect("per-iteration closure captures classify and lower");
}

#[test]
fn program_bounds_bypass_catch_and_finally_code() {
    let program = lash_typescript::compile(
        "try { while (true) {} } catch (error) { finish('caught'); } finally { finish('finally'); }",
    )
    .expect("bounded program compiles");
    let environment = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::instructions(100),
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
    ));
    let outcome =
        futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &environment));
    assert!(matches!(
        outcome,
        Err(RuntimeError::InstructionBudgetExceeded { .. })
    ));
}

#[test]
fn async_helpers_and_promise_all_use_the_resumable_map_driver() {
    assert_eq!(
        finished(
            "async function plusOne(x){return x+1;} const values=await Promise.all([1,2].map(async x=>await plusOne(x))); finish(values.join(','));"
        ),
        Value::String("2,3".into())
    );
}

#[test]
fn async_map_all_settled_wraps_each_callback_settlement() {
    assert_eq!(
        finished(
            r#"
            async function classify(x) {
                if (x === 2) { throw 'boom'; }
                if (x === 3) { return; }
                if (x === 4) { function nested() { return 14; } return nested(); }
                return x + 10;
            }
            const values = await Promise.allSettled(
                [1, 2, 3, 4].map(async x => await classify(x))
            );
            finish(JSON.stringify(values));
            "#,
        ),
        Value::String(
            r#"[{"status":"fulfilled","value":11},{"status":"rejected","reason":"boom"},{"status":"fulfilled"},{"status":"fulfilled","value":14}]"#.into()
        )
    );
}

#[test]
fn nested_global_assignment_and_uri_codecs_use_registered_intrinsics() {
    assert_eq!(
        finished(
            r#"
            function setAnswer(value) { return globalThis.answer = value; }
            const assigned = setAnswer(7);
            let malformed;
            try { decodeURIComponent('%E0%A4%A'); }
            catch (error) { malformed = `${error.name}|${error.message}|${error instanceof URIError}`; }
            let surrogate;
            try { encodeURIComponent('\uD800'); }
            catch (error) { surrogate = `${error.name}|${error.message}|${error instanceof URIError}`; }
            finish([
                assigned,
                globalThis.answer,
                encodeURIComponent('a b/é😀'),
                decodeURIComponent('a%20b%2F%C3%A9%F0%9F%98%80'),
                encodeURI('https://x.test/a b?x=é#z'),
                decodeURI('https://x.test/a%20b?x=%3F%23%2F'),
                malformed,
                surrogate,
            ]);
            "#,
        ),
        Value::List(
            vec![
                Value::Number(7.0),
                Value::Number(7.0),
                Value::String("a%20b%2F%C3%A9%F0%9F%98%80".into()),
                Value::String("a b/é😀".into()),
                Value::String("https://x.test/a%20b?x=%C3%A9#z".into()),
                Value::String("https://x.test/a b?x=%3F%23%2F".into()),
                Value::String("URIError|URI malformed|true".into()),
                Value::String("URIError|URI malformed|true".into()),
            ]
            .into(),
        )
    );
}

#[test]
fn every_wpa_private_intrinsic_shape_links_through_the_production_wrapper() {
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    for source in [
        "const e = new Error('x'); finish(e instanceof Error);",
        "globalThis.present = 1; finish('present' in globalThis);",
        "globalThis.removed = 1; finish(delete globalThis.removed);",
        "function add(a,b){return a+b;} finish(add(...[1,2]));",
        "finish(await Promise.all([1,2].map(async x => x + 1)));",
        "function collect(a=1,...rest){return [a,...rest];} finish(collect(undefined,2));",
        "function set(){return globalThis.nested=3;} finish(set());",
        "finish([encodeURIComponent('a b'),decodeURIComponent('a%20b'),encodeURI('a b'),decodeURI('a%20b')]);",
    ] {
        lash_typescript::link(source, &environment).unwrap_or_else(|error| {
            panic!("source must link through the production wrapper: {source}: {error}")
        });
    }
}
