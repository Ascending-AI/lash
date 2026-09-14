// Declared pure synchronous functions (`Declaration::Function`).
//
// The feature's whole safety argument is that a function body cannot perform
// an effect, so every effect stays at a stable top-level site and neither
// exactly-once call-site identity nor continuation snapshots gain a new shape.
// These tests pin both halves: the type contract a call site gets, and the ban
// that makes the contract safe.
//
// ADR 0096 retires the dialect that spelled these declarations `fn name(..) ->
// T { .. }`; TypeScript lowers its own `function` declarations to closures and
// never mints a `Declaration::Function`. The declaration and its linker
// contract remain IR facts — a deserialized workflow graph carries them — so
// every program here is built from the AST, which is also the only path the
// surviving code has. The file's one AST-built row has become all of them.

use super::*;
use crate::ast_support::{call, finish, number, string};
use lashlang::{
    BinaryOp, Declaration, Expr, FunctionDecl, FunctionParam, LinkError, Program, TypeExpr,
};

fn param(name: &str, ty: TypeExpr) -> FunctionParam {
    FunctionParam {
        name: name.into(),
        ty,
    }
}

fn function(
    name: &str,
    params: Vec<FunctionParam>,
    return_ty: TypeExpr,
    body: Expr,
) -> Declaration {
    Declaration::Function(FunctionDecl {
        name: name.into(),
        params,
        return_ty,
        body,
    })
}

fn module(declarations: Vec<Declaration>, main: Vec<Expr>) -> Program {
    let mut program = Program::block(main);
    program.declarations = declarations;
    program
}

fn var(name: &str) -> Expr {
    Expr::Variable(name.into())
}

fn binary(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: lashlang::AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

fn if_else(condition: Expr, then_block: Expr, else_block: Expr) -> Expr {
    Expr::If {
        condition: Box::new(condition),
        then_block: Box::new(then_block),
        else_block: Box::new(else_block),
    }
}

fn link(program: Program) -> Result<lashlang::LinkedModule, LinkError> {
    lashlang::LinkedModule::link(program, test_host_environment())
}

fn link_error(program: Program) -> LinkError {
    link(program).expect_err("linking should fail")
}

async fn finish_value(program: Program) -> Value {
    let linked = link(program).expect("linking should succeed");
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
    let value = finish_value(module(
        vec![function(
            "double",
            vec![param("n", TypeExpr::Int)],
            TypeExpr::Float,
            binary(var("n"), BinaryOp::Multiply, number(2.0)),
        )],
        vec![finish(call("double", vec![number(21.0)]))],
    ))
    .await;

    assert_eq!(value, Value::Number(42.0));
}

#[tokio::test(flavor = "current_thread")]
async fn one_function_serves_many_call_sites() {
    // The founding argument for the feature: shared logic is written once and
    // reached from several places, including from inside a loop.
    let value = finish_value(module(
        vec![function(
            "label",
            vec![param("name", TypeExpr::Str), param("count", TypeExpr::Int)],
            TypeExpr::Str,
            call("format", vec![string("{}={}"), var("name"), var("count")]),
        )],
        vec![
            assign("parts", Expr::List(Vec::new())),
            Expr::For {
                binding: "name".into(),
                iterable: Box::new(Expr::List(vec![string("a"), string("b")])),
                body: Box::new(Expr::Block(vec![assign(
                    "parts",
                    call(
                        "push",
                        vec![var("parts"), call("label", vec![var("name"), number(1.0)])],
                    ),
                )])),
            },
            assign(
                "parts",
                call(
                    "push",
                    vec![var("parts"), call("label", vec![string("c"), number(3.0)])],
                ),
            ),
            finish(call("join", vec![var("parts"), string(",")])),
        ],
    ))
    .await;

    assert_eq!(value, Value::String("a=1,b=1,c=3".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_may_call_itself() {
    let value = finish_value(module(
        vec![function(
            "countdown",
            vec![param("n", TypeExpr::Float)],
            TypeExpr::Str,
            if_else(
                binary(var("n"), BinaryOp::LessEqual, number(0.0)),
                string("done"),
                call(
                    "countdown",
                    vec![binary(var("n"), BinaryOp::Subtract, number(1.0))],
                ),
            ),
        )],
        vec![finish(call("countdown", vec![number(5.0)]))],
    ))
    .await;

    assert_eq!(value, Value::String("done".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn functions_may_call_each_other_in_either_direction() {
    // Declaration order is not a call-graph constraint: the callee is
    // materialized from the chunk at the call site, so mutual recursion and
    // forward references both link.
    let parity = |name: &str, other: &str, at_zero: bool| {
        function(
            name,
            vec![param("n", TypeExpr::Float)],
            TypeExpr::Bool,
            if_else(
                binary(var("n"), BinaryOp::Equal, number(0.0)),
                Expr::Bool(at_zero),
                call(
                    other,
                    vec![binary(var("n"), BinaryOp::Subtract, number(1.0))],
                ),
            ),
        )
    };
    let value = finish_value(module(
        vec![parity("even", "odd", true), parity("odd", "even", false)],
        vec![finish(Expr::List(vec![
            call("even", vec![number(4.0)]),
            call("odd", vec![number(4.0)]),
        ]))],
    ))
    .await;

    assert_eq!(
        value,
        Value::List(vec![Value::Bool(true), Value::Bool(false)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn arguments_alias_the_caller_binding() {
    // Arguments are passed by reference (ADR 0096): mutating the object a
    // parameter names is visible at the caller's binding, exactly as it is in
    // TypeScript.
    let value = finish_value(module(
        vec![function(
            "extend",
            vec![param("items", TypeExpr::List(Box::new(TypeExpr::Int)))],
            TypeExpr::List(Box::new(TypeExpr::Int)),
            Expr::Block(vec![
                assign("items", call("push", vec![var("items"), number(3.0)])),
                var("items"),
            ]),
        )],
        vec![
            assign("original", Expr::List(vec![number(1.0), number(2.0)])),
            assign("extended", call("extend", vec![var("original")])),
            finish(Expr::List(vec![var("original"), var("extended")])),
        ],
    ))
    .await;

    let expected =
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into());
    assert_eq!(value, Value::List(vec![expected.clone(), expected].into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_body_sees_only_its_parameters() {
    // Turn state is not ambient inside a function. A body that could read the
    // caller's variables would make one call's result depend on when it ran.
    let error = link_error(module(
        vec![function(
            "read_outer",
            vec![param("n", TypeExpr::Int)],
            TypeExpr::Float,
            binary(var("n"), BinaryOp::Add, var("outer")),
        )],
        vec![
            assign("outer", number(1.0)),
            finish(call("read_outer", vec![number(1.0)])),
        ],
    ));

    assert!(
        matches!(&error, LinkError::UnknownName { name, .. } if name == "outer"),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_is_checked_against_the_declared_arity() {
    let error = link_error(module(
        vec![function(
            "add",
            vec![param("a", TypeExpr::Int), param("b", TypeExpr::Int)],
            TypeExpr::Float,
            binary(var("a"), BinaryOp::Add, var("b")),
        )],
        vec![finish(call("add", vec![number(1.0)]))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::FunctionArgumentCount {
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
    let error = link_error(module(
        vec![function(
            "shout",
            vec![param("text", TypeExpr::Str)],
            TypeExpr::Str,
            call("upper", vec![var("text")]),
        )],
        vec![finish(call("shout", vec![number(3.0)]))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::IncompatibleFunctionArgument { function, param, .. }
                if function == "shout" && param == "text"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_body_is_checked_against_the_declared_return_type() {
    let error = link_error(module(
        vec![function(
            "name",
            vec![param("n", TypeExpr::Int)],
            TypeExpr::Str,
            var("n"),
        )],
        vec![finish(call("name", vec![number(1.0)]))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::IncompatibleFunctionReturn { function, .. } if function == "name"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_declared_return_type_flows_into_the_call_site() {
    // The point of a mandatory return type: the call's type is known, so a
    // downstream type error is caught at the use rather than at runtime.
    let error = link_error(module(
        vec![function(
            "count",
            vec![param("items", TypeExpr::List(Box::new(TypeExpr::Str)))],
            TypeExpr::Int,
            call("len", vec![var("items")]),
        )],
        vec![finish(call(
            "upper",
            vec![call("count", vec![Expr::List(vec![string("a")])])],
        ))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::IncompatibleBuiltinOperands { builtin, .. } if builtin == "upper"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_name_is_not_a_value() {
    let error = link_error(module(
        vec![function(
            "double",
            vec![param("n", TypeExpr::Int)],
            TypeExpr::Float,
            binary(var("n"), BinaryOp::Multiply, number(2.0)),
        )],
        vec![finish(var("double"))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::FunctionNameIsNotAValue { name, .. } if name == "double"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_name_cannot_be_bound_as_a_variable() {
    let error = link_error(module(
        vec![function(
            "double",
            vec![param("n", TypeExpr::Int)],
            TypeExpr::Float,
            binary(var("n"), BinaryOp::Multiply, number(2.0)),
        )],
        vec![
            assign("double", number(3.0)),
            finish(call("double", vec![number(1.0)])),
        ],
    ));

    assert!(
        matches!(
            &error,
            LinkError::FunctionNameIsNotAValue { name, .. } if name == "double"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_cannot_reuse_a_builtin_name() {
    let error = link_error(module(
        vec![function(
            "len",
            vec![param("items", TypeExpr::List(Box::new(TypeExpr::Int)))],
            TypeExpr::Int,
            number(0.0),
        )],
        vec![finish(call("len", vec![Expr::List(vec![number(1.0)])]))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::FunctionShadowsBuiltin { name, .. } if name == "len"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn two_functions_cannot_share_a_name() {
    let one = || {
        function(
            "one",
            vec![param("n", TypeExpr::Int)],
            TypeExpr::Int,
            var("n"),
        )
    };
    let error = link_error(module(
        vec![one(), one()],
        vec![finish(call("one", vec![number(1.0)]))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::DuplicateDeclaration { name, .. } if name == "one"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_cannot_repeat_a_parameter_name() {
    let error = link_error(module(
        vec![function(
            "add",
            vec![param("a", TypeExpr::Int), param("a", TypeExpr::Int)],
            TypeExpr::Int,
            var("a"),
        )],
        vec![finish(call("add", vec![number(1.0), number(2.0)]))],
    ));

    assert!(
        matches!(
            &error,
            LinkError::DuplicateFunctionParam { name, .. } if name == "a"
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_may_use_a_declared_type() {
    let value = finish_value(module(
        vec![
            Declaration::Type(lashlang::TypeDecl {
                name: "Point".into(),
                ty: TypeExpr::Object(vec![
                    lashlang::TypeField {
                        name: "x".into(),
                        ty: TypeExpr::Int,
                        optional: false,
                    },
                    lashlang::TypeField {
                        name: "y".into(),
                        ty: TypeExpr::Int,
                        optional: false,
                    },
                ]),
            }),
            function(
                "total",
                vec![param("point", TypeExpr::Ref("Point".into()))],
                TypeExpr::Float,
                binary(
                    Expr::Field {
                        target: Box::new(var("point")),
                        field: "x".into(),
                    },
                    BinaryOp::Add,
                    Expr::Field {
                        target: Box::new(var("point")),
                        field: "y".into(),
                    },
                ),
            ),
        ],
        vec![finish(call(
            "total",
            vec![Expr::Record(vec![
                ("x".into(), number(1.0)),
                ("y".into(), number(2.0)),
            ])],
        ))],
    ))
    .await;

    assert_eq!(value, Value::Number(3.0));
}

#[tokio::test(flavor = "current_thread")]
async fn a_function_may_be_called_from_a_process_body() {
    // A process compiles to its own chunk, so the declared functions have to be
    // registered for that chunk too — otherwise the call would compile against
    // an empty function table.
    let linked = link(module(
        vec![
            function(
                "shout",
                vec![param("text", TypeExpr::Str)],
                TypeExpr::Str,
                call("upper", vec![var("text")]),
            ),
            Declaration::Process(lashlang::ProcessDecl {
                name: "greet".into(),
                params: Vec::new(),
                signals: Vec::new(),
                return_ty: None,
                label: None,
                body: Expr::Block(vec![finish(call("shout", vec![string("ada")]))]),
            }),
        ],
        vec![finish(Expr::Null)],
    ))
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

fn forbidden_construct(declarations: Vec<Declaration>, body: Expr) -> (String, String) {
    let mut declarations = declarations;
    declarations.push(function("f", Vec::new(), TypeExpr::Any, body));
    let error = link_error(module(declarations, vec![finish(call("f", Vec::new()))]));
    match error {
        LinkError::ForbiddenInFunction {
            function,
            construct,
            ..
        } => (function, construct.to_string()),
        other => panic!("expected a forbidden-construct error, got {other:?}"),
    }
}

fn files_read(path: Expr) -> Expr {
    Expr::ReceiverCall {
        receiver: Box::new(Expr::ResourceRef(lashlang::ResourceRefExpr::unresolved(
            vec!["files".into()],
        ))),
        operation: "read".into(),
        args: vec![Expr::Record(vec![("path".into(), path)])],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn every_effectful_construct_is_rejected_in_a_function() {
    for (body, expected) in [
        (
            Expr::ResultUnwrap(Box::new(Expr::Await(Box::new(files_read(string("a.txt")))))),
            "await",
        ),
        (files_read(string("a.txt")), "a module operation call"),
        (Expr::Print(Box::new(number(1.0))), "print"),
        (Expr::SleepFor(Box::new(string("1s"))), "sleep for"),
        (Expr::SleepUntil(Box::new(string("1s"))), "sleep until"),
        (finish(number(1.0)), "finish"),
        (Expr::Cancel(Box::new(number(1.0))), "cancel"),
        (Expr::WaitSignal { name: "go".into() }, "wait_signal"),
        (
            Expr::SignalRun {
                run: Box::new(number(1.0)),
                name: "go".into(),
                payload: Box::new(number(1.0)),
            },
            "signal_run",
        ),
        (Expr::Yield(Box::new(Expr::Null)), "yield"),
        (Expr::Wake(Box::new(Expr::Null)), "wake"),
        (Expr::Fail(Box::new(Expr::Null)), "fail"),
        (
            // A label names a step in the workflow graph; a pure body
            // contributes no steps, so the annotation would be silently inert.
            Expr::LabelAnnotated {
                label: lashlang::LabelMetadata {
                    title: "Compute".into(),
                    description: None,
                },
                expr: Box::new(number(2.0)),
            },
            "@label",
        ),
    ] {
        let (function, construct) = forbidden_construct(Vec::new(), body);
        assert_eq!(function, "f");
        assert_eq!(construct, expected);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn starting_a_process_is_rejected_in_a_function() {
    let worker = Declaration::Process(lashlang::ProcessDecl {
        name: "work".into(),
        params: vec![lashlang::ProcessParam {
            name: "n".into(),
            ty: TypeExpr::Int,
        }],
        signals: Vec::new(),
        return_ty: None,
        label: None,
        body: Expr::Block(vec![finish(var("n"))]),
    });
    let (function, construct) = forbidden_construct(
        vec![worker],
        Expr::StartProcess(lashlang::ProcessStartExpr {
            process: "work".into(),
            args: vec![("n".into(), number(1.0))],
        }),
    );

    assert_eq!(function, "f");
    assert_eq!(construct, "start");
}

#[tokio::test(flavor = "current_thread")]
async fn a_process_name_is_rejected_in_a_function() {
    // The bare name of a declared process is an ordinary identifier in the
    // body, so the unlowered body holds nothing forbidden; the linker is what
    // turns it into a process reference. Checking only the raw body would let
    // this through, which is why the ban is also applied to the lowered body.
    let worker = Declaration::Process(lashlang::ProcessDecl {
        name: "worker".into(),
        params: Vec::new(),
        signals: Vec::new(),
        return_ty: None,
        label: None,
        body: Expr::Block(vec![finish(number(1.0))]),
    });
    let (function, construct) = forbidden_construct(vec![worker], var("worker"));

    assert_eq!(function, "f");
    assert_eq!(construct, "a process reference");
}

#[tokio::test(flavor = "current_thread")]
async fn an_effect_nested_deep_in_a_function_is_still_rejected() {
    let (function, construct) = forbidden_construct(
        Vec::new(),
        Expr::Block(vec![
            assign("total", number(0.0)),
            Expr::For {
                binding: "path".into(),
                iterable: Box::new(Expr::List(vec![string("a.txt")])),
                body: Box::new(Expr::Block(vec![if_else(
                    binary(
                        call("len", vec![var("path")]),
                        BinaryOp::Greater,
                        number(0.0),
                    ),
                    Expr::Block(vec![assign(
                        "body",
                        Expr::ResultUnwrap(Box::new(Expr::Await(Box::new(files_read(var(
                            "path",
                        )))))),
                    )]),
                    Expr::Block(Vec::new()),
                )])),
            },
            var("total"),
        ]),
    );

    assert_eq!(function, "f");
    assert_eq!(construct, "await");
}
