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
    for source in [
        "finish({} instanceof Promise);",
        "finish({} instanceof URL);",
        "finish({} instanceof URLSearchParams);",
    ] {
        let error = lash_typescript::compile(source).expect_err("unavailable kind must reject");
        assert_eq!(
            error.code,
            lash_typescript::DiagnosticCode::InstanceOfUnsupported
        );
        assert!(error.message.contains("Unsupported:"));
    }
    let stack = lash_typescript::compile("finish(new Error('x').stack);")
        .expect_err("nondeterministic stack must reject");
    assert!(stack.message.contains("stack"));
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
