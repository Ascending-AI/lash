use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new(
                "unsupported scoping regression ability",
            )),
        }
    }
}

fn finished(source: &str) -> Value {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
        .expect("TypeScript should execute")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[test]
fn assign_only_mutable_captures_reject_by_name() {
    let cases = [
        "let n = 0; const f = () => { n = 5; }; f(); finish(n);",
        "let seen = 0; function f(): number { try { return 1; } finally { seen = 1; } } f(); finish(seen);",
    ];
    for source in cases {
        assert_eq!(
            lash_typescript::compile(source)
                .expect_err("mutable capture writes must reject")
                .code,
            lash_typescript::DiagnosticCode::MutableCaptureUnsupported,
            "{source}"
        );
    }
}

#[test]
fn catch_body_declarations_shadow_enclosing_function_slots() {
    assert_eq!(
        finished(
            "function f(x: number): number { try { throw 1; } catch (e) { const x = 99; } return x; } finish(f(1));"
        ),
        Value::Number(1.0)
    );
    assert_eq!(
        finished(
            "function f(x: number): number { try { throw 1; } catch (e) { let x = 99; x = x + 1; } return x; } finish(f(1));"
        ),
        Value::Number(1.0)
    );
}

#[test]
fn unresolved_reads_and_arguments_reject_before_execution() {
    for source in [
        "finish(someTypo);",
        "function f(): number { return arguments.length; } finish(f());",
    ] {
        assert_eq!(
            lash_typescript::compile(source)
                .expect_err("unknown binding must reject")
                .code,
            lash_typescript::DiagnosticCode::UnknownBinding,
            "{source}"
        );
    }
}

#[test]
fn implicit_global_assignment_rejects_before_execution() {
    assert_eq!(
        lash_typescript::compile("durableTypo = 5; finish(durableTypo);")
            .expect_err("module-goal assignment cannot create a global")
            .code,
        lash_typescript::DiagnosticCode::UnknownBinding
    );
}

#[test]
fn function_declarations_capture_initialized_top_level_bindings() {
    assert_eq!(
        finished("const k = 3; function f(): number { return k; } finish(f());"),
        Value::Number(3.0)
    );
    assert_eq!(
        finished(
            "const state = { value: 1 }; function bump(): void { state.value = 3; } bump(); finish(state.value);"
        ),
        Value::Number(3.0)
    );
}

#[test]
fn hoisted_function_bodies_can_capture_later_const_bindings() {
    assert_eq!(
        finished("function f() { return k; } const k = 3; finish(f());"),
        Value::Number(3.0)
    );
    assert_eq!(
        finished("const value = f(); function f() { return 4; } finish(value);"),
        Value::Number(4.0)
    );
    assert_eq!(
        finished("function f() { return g(); } function g() { return 5; } finish(f());"),
        Value::Number(5.0)
    );
}

#[test]
fn lexical_bindings_shadow_host_intrinsic_names() {
    assert_eq!(
        finished("const print = (value: number): number => value; finish(print(5));"),
        Value::Number(5.0)
    );
    assert_eq!(
        finished(
            "const console = { log: (value: number): number => value }; finish(console.log(7));"
        ),
        Value::Number(7.0)
    );
}
