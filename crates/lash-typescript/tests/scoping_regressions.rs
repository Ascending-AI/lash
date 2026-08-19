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
    let typo =
        lash_typescript::compile("finish(someTypo);").expect_err("unknown binding must reject");
    assert_eq!(typo.code, lash_typescript::DiagnosticCode::UnknownBinding);
    let arguments =
        lash_typescript::compile("function f(): number { return arguments.length; } finish(f());")
            .expect_err("arguments must direct authors to rest parameters");
    assert_eq!(
        arguments.code,
        lash_typescript::DiagnosticCode::ThisUnsupported
    );
    assert!(arguments.to_string().contains("...rest"), "{arguments}");
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
fn immutable_captures_cross_every_intermediate_function_frame() {
    let cases = [
        (
            "const outer = 5; const f = () => { const g = () => outer; return g(); }; finish(f());",
            5.0,
        ),
        (
            "const base = 10; const outer = () => { const inner = () => base; return inner; }; finish(outer()());",
            10.0,
        ),
        (
            "const a = 1; const f = () => { const g = () => { const h = () => a; return h(); }; return g(); }; finish(f());",
            1.0,
        ),
        (
            "const outer = (a: number) => { const x = 2; const middle = (b: number) => { const y = 4; const inner = () => a + x + b + y; return inner(); }; return middle(8); }; finish(outer(16));",
            30.0,
        ),
        (
            "const outer = (a: number) => { const x = 2; const middle = (b: number) => { const y = 4; const deep = (c: number) => { const z = 8; const inner = () => a + x + b + y + c + z; return inner(); }; return deep(16); }; return middle(32); }; finish(outer(64));",
            126.0,
        ),
        (
            "const base = 10; const outer = () => { const middle = () => { const also = base; const inner = () => base + also; return inner(); }; return middle(); }; finish(outer());",
            20.0,
        ),
    ];

    for (source, expected) in cases {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
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
fn nested_function_declarations_read_enclosing_bindings() {
    assert_eq!(
        finished(
            "const top = 9; function outerFn(): number { function innerFn(): number { return top; } return innerFn(); } finish(outerFn());"
        ),
        Value::Number(9.0)
    );
    // The enclosing binding is reachable regardless of source order, and
    // through more than one level of nesting.
    assert_eq!(
        finished(
            "function outerFn(): number { function innerFn(): number { return later; } return innerFn(); } const later = 4; finish(outerFn());"
        ),
        Value::Number(4.0)
    );
    assert_eq!(
        finished(
            "const seed = 2; function a(): number { function b(): number { function c(): number { return seed; } return c(); } return b(); } finish(a());"
        ),
        Value::Number(2.0)
    );
    // Self-recursion needs no peer and stays supported.
    assert_eq!(
        finished(
            "function fact(n: number): number { if (n <= 1) { return 1; } return fact(n - 1) * n; } finish(fact(5));"
        ),
        Value::Number(120.0)
    );
    // An acyclic chain of declarations is ordered, not rejected.
    assert_eq!(
        finished(
            "function head(n: number): number { return tail(n) + 1; } function tail(n: number): number { return n * 2; } finish(head(3));"
        ),
        Value::Number(7.0)
    );
}

#[test]
fn mutually_recursive_declarations_reject_with_their_cycle() {
    // v1 captures by value, so a declaration cycle has no emission order; the
    // frame-record alternative builds a heap cycle the durable encoding cannot
    // hold. The shape rejects statically and names the cycle.
    for (source, cycle) in [
        (
            "function isEven(n: number): boolean { if (n === 0) { return true; } return isOdd(n - 1); } function isOdd(n: number): boolean { if (n === 0) { return false; } return isEven(n - 1); } finish(isEven(4));",
            "isEven -> isOdd -> isEven",
        ),
        (
            "function a(n: number): number { if (n === 0) { return 0; } return b(n - 1); } function b(n: number): number { return c(n); } function c(n: number): number { return 1 + a(n); } finish(a(3));",
            "a -> b -> c -> a",
        ),
        (
            "function ping(n: number): number { return pong(n); } function pong(n: number): number { return ping(n); } function caller(): number { return ping(1); } finish(caller());",
            "ping -> pong -> ping",
        ),
    ] {
        let error = lash_typescript::compile(source)
            .expect_err("mutually recursive declarations must reject");
        assert_eq!(
            error.code,
            lash_typescript::DiagnosticCode::MutualRecursionUnsupported,
            "{source}"
        );
        assert!(
            error.to_string().contains(cycle),
            "expected cycle `{cycle}` in: {error}"
        );
    }
    // A cycle nested inside a function body has the same lowering problem and
    // takes the same rejection, with the names the author wrote.
    let error = lash_typescript::compile(
        "function shell(n: number): number { function up(k: number): number { if (k === 0) { return 0; } return down(k - 1) + 1; } function down(k: number): number { return up(k); } return up(n); } finish(shell(5));",
    )
    .expect_err("a nested cycle must reject too");
    assert_eq!(
        error.code,
        lash_typescript::DiagnosticCode::MutualRecursionUnsupported
    );
    assert!(error.to_string().contains("up -> down -> up"), "{error}");
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

#[test]
fn generated_binding_namespace_is_reserved() {
    // A source identifier spelled like a generated one would collide with the
    // lowerer's own namespace and silently take a block-local value, including
    // over a durable root global.
    for source in [
        "const __typescript_0_a = 'top'; { const a = 'inner'; } finish(__typescript_0_a);",
        "const f = () => { const __typescript_0_b = 'fn'; { const b = 'blk'; } return __typescript_0_b; }; finish(f());",
        "function __typescript_0_f(): number { return 1; } finish(__typescript_0_f());",
        "const g = (__typescript_0_p: number): number => __typescript_0_p; finish(g(1));",
        "try { throw 1; } catch (__typescript_0_e) { finish(1); }",
        "let __typescript_0_frame = 1; finish(__typescript_0_frame);",
    ] {
        assert_eq!(
            lash_typescript::compile(source)
                .expect_err("generated-namespace identifiers must reject")
                .code,
            lash_typescript::DiagnosticCode::ReservedIdentifier,
            "{source}"
        );
    }
    // Neighbouring spellings stay ordinary identifiers.
    assert_eq!(
        finished(
            "const __typescript = 3; const _typescript_0_a = 4; finish(__typescript + _typescript_0_a);"
        ),
        Value::Number(7.0)
    );
}

#[test]
fn named_function_expressions_bind_their_own_name() {
    // ECMA binds a function expression's name inside its own body, which is how
    // the classic self-recursive function expression works.
    assert_eq!(
        finished(
            "const g = function self(n: number): number { if (n <= 0) { return 0; } return self(n - 1); }; finish(g(4));"
        ),
        Value::Number(0.0)
    );
    assert_eq!(
        finished(
            "const fact = function inner(n: number): number { if (n <= 1) { return 1; } return n * inner(n - 1); }; finish(fact(5));"
        ),
        Value::Number(120.0)
    );
    // The name is scoped to the body and does not escape into the enclosing
    // scope, and it does not shadow an enclosing binding of the same name.
    assert_eq!(
        lash_typescript::compile(
            "const g = function self(n: number): number { return n; }; finish(self(1));"
        )
        .expect_err("the expression name is not visible outside its body")
        .code,
        lash_typescript::DiagnosticCode::UnknownBinding
    );
    assert_eq!(
        finished(
            "const outer = 3; const g = function outer2(n: number): number { return n + outer; }; finish(g(1));"
        ),
        Value::Number(4.0)
    );
    // The generated namespace is reserved on this path too.
    assert_eq!(
        lash_typescript::compile("const g = function __typescript_h(): number { return 1; };")
            .expect_err("a generated-namespace expression name must reject")
            .code,
        lash_typescript::DiagnosticCode::ReservedIdentifier
    );
}

#[test]
fn sibling_scopes_do_not_share_async_helper_facts() {
    // A dead sibling `f` that happened to be async must not make a later,
    // ordinary `f` look like an async helper and demand `await`.
    assert_eq!(
        finished("{ const f = async () => 1; } { const f = (): number => 2; finish(f()); }"),
        Value::Number(2.0)
    );
}

#[test]
fn sibling_scopes_do_not_share_exotic_iterable_facts() {
    // A dead sibling `m` that was a Map must not route a later array `m` down
    // the exotic-iterable path, where `forEach` stops being the array method.
    assert_eq!(
        finished(
            "{ const m = new Map(); } { const m = [1, 2, 3]; const out: number[] = []; m.forEach((value: number) => { out.push(value * 2); }); finish(out.length); }"
        ),
        Value::Number(3.0)
    );
}

#[test]
fn dead_process_handle_names_do_not_change_await_lowering() {
    // A process handle whose scope has closed must not lend its name to an
    // unrelated later binding, turning `await` on a settled value into a
    // process await.
    let source = r#"
        const worker = defineProcess({ name: "worker", signals: {}, run: async () => 1 });
        { const handle = start(worker); }
        { const handle = 5; finish(await handle); }
    "#;
    assert_eq!(
        lash_typescript::parse(source)
            .expect_err("awaiting a settled value must reject")
            .code,
        lash_typescript::DiagnosticCode::AwaitUnsupported
    );
}

#[test]
fn start_resolves_its_target_through_the_scope_stack() {
    // `start` resolves its argument like every other read: a parameter that
    // shadows the process binding is not the process.
    let shadowed = r#"
        const worker = defineProcess({ name: "worker", signals: {}, run: async () => 1 });
        const f = (worker: number) => start(worker);
        finish(f(1));
    "#;
    assert_eq!(
        lash_typescript::parse(shadowed)
            .expect_err("a shadowing parameter is not a process definition")
            .code,
        lash_typescript::DiagnosticCode::ProcessTargetStaticRequired
    );
    // The unshadowed target still resolves.
    let visible = r#"
        const worker = defineProcess({ name: "worker", signals: {}, run: async () => 1 });
        const handle = start(worker);
        finish(1);
    "#;
    lash_typescript::parse(visible).expect("a top-level process binding starts");
}
