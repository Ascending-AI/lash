// User-defined pure synchronous functions (`fn`).
//
// The feature's whole safety argument is that a function body cannot perform
// an effect, so every effect stays at a stable top-level syntactic site and
// neither exactly-once call-site identity nor continuation snapshots gain a
// new shape. These tests pin both halves: the type contract a call site gets,
// and the ban that makes the contract safe.

fn link(source: &str) -> Result<lashlang::LinkedModule, ExecuteError> {
    let program = parse(source)?;
    Ok(lashlang::LinkedModule::link(
        program,
        test_host_environment(),
    )?)
}

// These helpers link explicitly rather than going through `execute`, whose
// fallback to the unlinked compile path would turn a link error into a later
// runtime error and hide exactly what these tests are about.
async fn link_error(source: &str) -> lashlang::LinkError {
    match link(source).expect_err("linking should fail") {
        ExecuteError::Link(error) => error,
        ExecuteError::Parse(error) => panic!("expected link error, got parse error: {error:?}"),
        ExecuteError::Runtime(error) => panic!("expected link error, got runtime error: {error:?}"),
    }
}

async fn finish_value(source: &str) -> Value {
    let linked = link(source).expect("linking should succeed");
    let compiled = lashlang::compile_linked(&linked);
    let host = TestHost::default();
    let mut state = State::new();
    finished(
        lashlang::execute(&compiled, &mut state, &host)
            .await
            .expect("execution should succeed"),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_is_callable_like_a_builtin() {
    let value = finish_value(
        r#"
        fn double(n: int) -> float {
          n * 2
        }

        finish double(21)
        "#,
    )
    .await;

    assert_eq!(value, Value::Number(42.0));
}

#[tokio::test(flavor = "current_thread")]
async fn one_function_serves_many_call_sites() {
    // The founding argument for the feature: shared logic is written once and
    // reached from several places, including from inside a loop.
    let value = finish_value(
        r#"
        fn label(name: str, count: int) -> str {
          format("{}={}", name, count)
        }

        parts = []
        for name in ["a", "b"] {
          parts = push(parts, label(name, 1))
        }
        parts = push(parts, label("c", 3))
        finish join(parts, ",")
        "#,
    )
    .await;

    assert_eq!(value, Value::String("a=1,b=1,c=3".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_may_call_itself() {
    let value = finish_value(
        r#"
        fn countdown(n: float) -> str {
          if n <= 0 { "done" } else { countdown(n - 1) }
        }

        finish countdown(5)
        "#,
    )
    .await;

    assert_eq!(value, Value::String("done".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn functions_may_call_each_other_in_either_direction() {
    // Declaration order is not a call-graph constraint: the callee is
    // materialized from the chunk at the call site, so mutual recursion and
    // forward references both link.
    let value = finish_value(
        r#"
        fn even(n: float) -> bool {
          if n == 0 { true } else { odd(n - 1) }
        }

        fn odd(n: float) -> bool {
          if n == 0 { false } else { even(n - 1) }
        }

        finish [even(4), odd(4)]
        "#,
    )
    .await;

    assert_eq!(
        value,
        Value::List(vec![Value::Bool(true), Value::Bool(false)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn arguments_are_isolated_from_the_caller() {
    // Lashlang is a value-semantics dialect: mutating a parameter inside a
    // function must not reach the caller's binding.
    let value = finish_value(
        r#"
        fn extend(items: list[int]) -> list[int] {
          items = push(items, 3)
          items
        }

        original = [1, 2]
        extended = extend(original)
        finish [original, extended]
        "#,
    )
    .await;

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into()),
                Value::List(
                    vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into()
                ),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_body_sees_only_its_parameters() {
    // Turn state is not ambient inside a function. A body that could read the
    // caller's variables would make one call's result depend on when it ran.
    let error = link_error(
        r#"
        outer = 1

        fn read_outer(n: int) -> float {
          n + outer
        }

        finish read_outer(1)
        "#,
    )
    .await;

    assert!(
        matches!(&error, lashlang::LinkError::UnknownName { name, .. } if name == "outer"),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_is_checked_against_the_declared_arity() {
    let error = link_error(
        r#"
        fn add(a: int, b: int) -> float { a + b }

        finish add(1)
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::FunctionArgumentCount {
                function,
                expected: 2,
                actual: 1,
                ..
            } if function == "add"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_is_checked_against_the_declared_parameter_types() {
    let error = link_error(
        r#"
        fn shout(text: str) -> str { upper(text) }

        finish shout(3)
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::IncompatibleFunctionArgument { function, param, .. }
                if function == "shout" && param == "text"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_body_is_checked_against_the_declared_return_type() {
    let error = link_error(
        r#"
        fn name(n: int) -> str { n }

        finish name(1)
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::IncompatibleFunctionReturn { function, .. } if function == "name"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_declared_return_type_flows_into_the_call_site() {
    // The point of a mandatory return type: the call's type is known, so a
    // downstream type error is caught at the use rather than at runtime.
    let error = link_error(
        r#"
        fn count(items: list[str]) -> int { len(items) }

        finish upper(count(["a"]))
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::IncompatibleBuiltinOperands { builtin, .. } if builtin == "upper"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_return_type_is_mandatory() {
    let error = parse(
        r#"
        fn double(n: int) { n * 2 }

        finish double(1)
        "#,
    )
    .expect_err("parse should fail");

    assert!(
        matches!(
            &error,
            lashlang::ParseError::Expected { expected, .. } if *expected == "`->` and a return type"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_name_is_not_a_value() {
    let error = link_error(
        r#"
        fn double(n: int) -> float { n * 2 }

        finish double
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::FunctionNameIsNotAValue { name, .. } if name == "double"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_name_cannot_be_bound_as_a_variable() {
    let error = link_error(
        r#"
        fn double(n: int) -> float { n * 2 }

        double = 3
        finish double(1)
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::FunctionNameIsNotAValue { name, .. } if name == "double"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_cannot_reuse_a_builtin_name() {
    let error = link_error(
        r#"
        fn len(items: list[int]) -> int { 0 }

        finish len([1])
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::FunctionShadowsBuiltin { name, .. } if name == "len"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn two_functions_cannot_share_a_name() {
    let error = link_error(
        r#"
        fn one(n: int) -> int { n }
        fn one(n: int) -> int { n }

        finish one(1)
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::DuplicateDeclaration { name, .. } if name == "one"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_cannot_repeat_a_parameter_name() {
    let error = link_error(
        r#"
        fn add(a: int, a: int) -> int { a }

        finish add(1, 2)
        "#,
    )
    .await;

    assert!(
        matches!(
            &error,
            lashlang::LinkError::DuplicateFunctionParam { name, .. } if name == "a"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_may_use_a_declared_type() {
    let value = finish_value(
        r#"
        type Point = { x: int, y: int }

        fn total(point: Point) -> float {
          point.x + point.y
        }

        finish total({ x: 1, y: 2 })
        "#,
    )
    .await;

    assert_eq!(value, Value::Number(3.0));
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_may_be_called_from_a_process_body() {
    // A process compiles to its own chunk, so the declared functions have to be
    // registered for that chunk too — otherwise the call would compile against
    // an empty function table.
    let linked = link(
        r#"
        fn shout(text: str) -> str { upper(text) }

        process greet() {
          finish shout("ada")
        }

        finish null
        "#,
    )
    .expect("linking should succeed");

    let compiled = lashlang::compile_linked_process(&linked, "greet")
        .expect("the process chunk should compile");
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        lashlang::execute(&compiled, &mut state, &host)
            .await
            .expect("the process body should run"),
    );

    assert_eq!(value, Value::String("ADA".to_string().into()));
}

// ── The effect ban ────────────────────────────────────────────────────────
//
// Each shape below is rejected with the same typed error naming the construct.
// The ban is what keeps effect identity and continuation shape untouched, so
// every effectful form gets its own case rather than one representative.

async fn forbidden_construct(source: &str) -> (String, String) {
    let error = link_error(source).await;
    match error {
        lashlang::LinkError::ForbiddenInFunction {
            function,
            construct,
            ..
        } => (function, construct.to_string()),
        other => panic!("expected a forbidden-construct error, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_operation_call_is_rejected_in_a_function() {
    let (function, construct) = forbidden_construct(
        r#"
        fn read(path: str) -> str {
          await files.read({ path: path })?
        }

        finish read("a.txt")
        "#,
    )
    .await;

    assert_eq!(function, "read");
    assert_eq!(construct, "await");
}

#[tokio::test(flavor = "current_thread")]
async fn a_bare_module_operation_call_is_rejected_in_a_function() {
    let (function, construct) = forbidden_construct(
        r#"
        fn read(path: str) -> str {
          result = files.read({ path: path })
          "done"
        }

        finish read("a.txt")
        "#,
    )
    .await;

    assert_eq!(function, "read");
    assert_eq!(construct, "a module operation call");
}

#[tokio::test(flavor = "current_thread")]
async fn starting_a_process_is_rejected_in_a_function() {
    let (function, construct) = forbidden_construct(
        r#"
        process work(n: int) { finish n }

        fn kick(n: int) -> int {
          handle = start work(n: n)
          n
        }

        finish kick(1)
        "#,
    )
    .await;

    assert_eq!(function, "kick");
    assert_eq!(construct, "start");
}

#[tokio::test(flavor = "current_thread")]
async fn print_is_rejected_in_a_function() {
    let (function, construct) = forbidden_construct(
        r#"
        fn trace(n: int) -> int {
          print(n)
          n
        }

        finish trace(1)
        "#,
    )
    .await;

    assert_eq!(function, "trace");
    assert_eq!(construct, "print");
}

#[tokio::test(flavor = "current_thread")]
async fn sleeping_is_rejected_in_a_function() {
    let (function, construct) = forbidden_construct(
        r#"
        fn pause(n: int) -> int {
          sleep for "1s"
          n
        }

        finish pause(1)
        "#,
    )
    .await;

    assert_eq!(function, "pause");
    assert_eq!(construct, "sleep for");
}

#[tokio::test(flavor = "current_thread")]
async fn process_lifecycle_forms_are_rejected_in_a_function() {
    for (source, expected) in [
        ("fn f(n: int) -> int { finish n }", "finish"),
        ("fn f(n: int) -> int { cancel n }", "cancel"),
        (
            "fn f(n: int) -> int { payload = wait_signal(\"go\") n }",
            "wait_signal",
        ),
        (
            "fn f(n: int) -> int { signal_run(n, \"go\", n) n }",
            "signal_run",
        ),
    ] {
        let program = format!("{source}\n\nfinish f(1)\n");
        let (function, construct) = forbidden_construct(&program).await;
        assert_eq!(function, "f");
        assert_eq!(construct, expected, "for source: {source}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn process_admin_keywords_do_not_even_parse_in_a_function() {
    // `yield`/`wake`/`fail` are already process-body-only at the syntax layer,
    // and a function body is not a process body, so they never reach the
    // linker from source.
    for (source, keyword) in [
        ("fn f(n: int) -> int { fail n }", "fail"),
        ("fn f(n: int) -> int { yield n }", "yield"),
        ("fn f(n: int) -> int { wake n }", "wake"),
    ] {
        let error = parse(&format!("{source}\n\nfinish f(1)\n")).expect_err("parsing should fail");
        assert!(
            matches!(
                &error,
                lashlang::ParseError::SessionProcessAdminOutsideBlock { keyword: found, .. }
                    if *found == keyword
            ),
            "unexpected error for {source}: {error:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_effect_ban_holds_for_an_ast_that_never_passed_the_parser() {
    // Programs also arrive as deserialized workflow graphs, which can carry a
    // function body the source grammar would have refused. The ban is enforced
    // in the linker precisely so that path is covered too.
    for (body, expected) in [
        (
            lashlang::Expr::Yield(Box::new(lashlang::Expr::Null)),
            "yield",
        ),
        (lashlang::Expr::Wake(Box::new(lashlang::Expr::Null)), "wake"),
        (lashlang::Expr::Fail(Box::new(lashlang::Expr::Null)), "fail"),
    ] {
        let mut program = lashlang::Program::block(vec![lashlang::Expr::Null]);
        program.declarations = vec![lashlang::Declaration::Function(lashlang::FunctionDecl {
            name: "f".into(),
            params: Vec::new(),
            return_ty: lashlang::TypeExpr::Any,
            body,
        })];
        let error = lashlang::LinkedModule::link(program, test_host_environment())
            .expect_err("linking should fail");
        assert!(
            matches!(
                &error,
                lashlang::LinkError::ForbiddenInFunction { function, construct, .. }
                    if function == "f" && *construct == expected
            ),
            "unexpected error: {error:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_label_is_rejected_in_a_function() {
    // A label names a step in the workflow graph; a pure body contributes no
    // steps, so the annotation would be silently inert.
    let (function, construct) = forbidden_construct(
        r#"
        fn work(n: int) -> float {
          @label(title: "Compute")
          doubled = n * 2
          doubled
        }

        finish work(1)
        "#,
    )
    .await;

    assert_eq!(function, "work");
    assert_eq!(construct, "@label");
}

#[tokio::test(flavor = "current_thread")]
async fn an_effect_nested_deep_in_a_function_is_still_rejected() {
    let (function, construct) = forbidden_construct(
        r#"
        fn scan(paths: list[str]) -> float {
          total = 0
          for path in paths {
            if len(path) > 0 {
              body = await files.read({ path: path })?
              total = total + len(body)
            }
          }
          total
        }

        finish scan(["a.txt"])
        "#,
    )
    .await;

    assert_eq!(function, "scan");
    assert_eq!(construct, "await");
}

#[tokio::test(flavor = "current_thread")]
async fn fn_is_still_usable_as_an_ordinary_identifier() {
    // `fn` is a contextual keyword, so existing programs that used it as a
    // variable name keep working.
    let value = finish_value(
        r#"
        fn = 3
        finish fn + 1
        "#,
    )
    .await;

    assert_eq!(value, Value::Number(4.0));
}

#[tokio::test(flavor = "current_thread")]
async fn a_process_name_is_rejected_in_a_function() {
    // The bare name of a declared process is an ordinary identifier in the
    // source, so the parsed body holds nothing forbidden; the linker is what
    // turns it into a process reference. Checking only the parsed body would
    // let this through, which is why the ban is also applied to the lowered
    // body.
    let (function, construct) = forbidden_construct(
        r#"
        process worker() {
          finish 1
        }

        fn peek() -> any {
          worker
        }

        finish peek()
        "#,
    )
    .await;

    assert_eq!(function, "peek");
    assert_eq!(construct, "a process reference");
}
