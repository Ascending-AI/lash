use super::*;
use lashlang::{AssignTarget, Expr, Program, TypeField};

pub(super) fn expect_string<'a>(
    args: &'a Record,
    key: &str,
) -> Result<&'a str, ExecutionHostError> {
    match args.get(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(ExecutionHostError::new(format!(
            "missing string arg: {key}"
        ))),
    }
}

// ------------------------------------------------------------------
//  Type values
// ------------------------------------------------------------------
//
// `Type { .. }` was a literal of the retired Lashlang surface and has no
// TypeScript spelling (ADR 0096), so these programs are built through the
// public AST instead of authored. The value model they pin — a type value is
// JSON-schema shaped, travels as a tool-call argument, drives `validate`, and
// survives a snapshot round trip — is an IR fact rather than a surface one, so
// it is re-pointed at the IR rather than deleted with the syntax.

fn type_literal(fields: Vec<(&str, TypeExpr, bool)>) -> Expr {
    Expr::TypeLiteral(Box::new(TypeExpr::Object(
        fields
            .into_iter()
            .map(|(name, ty, optional)| TypeField {
                name: name.into(),
                ty,
                optional,
            })
            .collect(),
    )))
}

fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

fn books_type() -> Expr {
    type_literal(vec![
        ("title", TypeExpr::Str, false),
        (
            "genre",
            TypeExpr::Enum(vec!["fiction".into(), "non-fiction".into()]),
            false,
        ),
        ("tags", TypeExpr::List(Box::new(TypeExpr::Str)), false),
        (
            "meta",
            TypeExpr::Object(vec![
                TypeField {
                    name: "pages".into(),
                    ty: TypeExpr::Int,
                    optional: false,
                },
                TypeField {
                    name: "published".into(),
                    ty: TypeExpr::Int,
                    optional: false,
                },
            ]),
            false,
        ),
        ("isbn", TypeExpr::Str, true),
    ])
}

#[tokio::test(flavor = "current_thread")]
async fn end_to_end_type_value_is_json_schema_shaped() {
    let program = Program::block(vec![Expr::Finish(Box::new(books_type()))]);
    let host = TestHost::default();
    let mut state = State::new();
    let outcome = lashlang::execute(
        &lashlang_compile_program(&program).expect("the program compiles"),
        &mut state,
        &host,
    )
    .await
    .expect("should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let schema = lashlang::unwrap_type_value(&value)
        .and_then(Value::as_record)
        .expect("wrapped type");
    assert_eq!(schema["type"], Value::String("object".into()));
    let required = match &schema["required"] {
        Value::List(items) => items,
        _ => panic!("required must be list"),
    };
    // isbn is optional → 4 required
    assert_eq!(required.len(), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn type_is_usable_as_a_tool_call_argument() {
    #[derive(Default)]
    struct CaptureHost {
        captured: std::sync::Mutex<Option<Value>>,
    }
    impl ExecutionHost for CaptureHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => {
                    *self.captured.lock_recover() = operation
                        .args
                        .first()
                        .and_then(Value::as_record)
                        .and_then(|record| record.get("output"))
                        .cloned();
                    Ok(AbilityResult::Value(Value::Null))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }
    let shape = type_literal(vec![
        ("name", TypeExpr::Str, false),
        (
            "labels",
            TypeExpr::List(Box::new(TypeExpr::Enum(vec!["a".into(), "b".into()]))),
            false,
        ),
    ]);
    let program = Program::block(vec![
        assign("Shape", shape),
        Expr::Await(Box::new(Expr::ReceiverCall {
            receiver: Box::new(Expr::ResourceRef(lashlang::ResourceRefExpr::unresolved(
                vec!["agents".into()],
            ))),
            operation: "spawn".into(),
            args: vec![Expr::Record(vec![
                ("task".into(), Expr::String("find X".into())),
                ("output".into(), Expr::Variable("Shape".into())),
            ])],
        })),
        Expr::Finish(Box::new(Expr::Null)),
    ]);
    let host = CaptureHost::default();
    lashlang::execute(
        &lashlang_compile_program(&program).expect("the program compiles"),
        &mut State::new(),
        &host,
    )
    .await
    .expect("should run");
    let captured = host.captured.lock_recover().clone().expect("captured arg");
    let inner = lashlang::unwrap_type_value(&captured).expect("wrapped type");
    let schema = inner.as_record().expect("schema record");
    assert_eq!(schema["type"], Value::String("object".into()));
    let props = schema["properties"].as_record().unwrap();
    let labels = props["labels"].as_record().unwrap();
    assert_eq!(labels["type"], Value::String("array".into()));
    let items = labels["items"].as_record().unwrap();
    let enum_values = match &items["enum"] {
        Value::List(items) => items,
        _ => panic!("enum should be list"),
    };
    assert_eq!(enum_values.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn validate_reuses_type_literals_for_intermediate_checks() {
    let host = TestHost::default();
    let package_type = || {
        type_literal(vec![
            ("name", TypeExpr::Str, false),
            ("version", TypeExpr::Str, false),
            ("labels", TypeExpr::List(Box::new(TypeExpr::Str)), false),
        ])
    };
    let raw = Expr::Record(vec![
        ("name".into(), Expr::String("lashlang".into())),
        ("version".into(), Expr::String("0.2.61".into())),
        (
            "labels".into(),
            Expr::List(vec![
                Expr::String("agent".into()),
                Expr::String("runtime".into()),
            ]),
        ),
    ]);
    let program = Program::block(vec![
        assign("raw", raw),
        assign(
            "package",
            Expr::BuiltinCall {
                name: "validate".into(),
                args: vec![Expr::Variable("raw".into()), package_type()],
            },
        ),
        Expr::Finish(Box::new(Expr::Variable("package".into()))),
    ]);
    let mut state = State::new();
    let value = finished(
        lashlang::execute(
            &lashlang_compile_program(&program).expect("the program compiles"),
            &mut state,
            &host,
        )
        .await
        .expect("validate should succeed"),
    );
    let package = value.as_record().expect("package record");
    assert_eq!(
        package["name"],
        Value::String("lashlang".to_string().into())
    );

    let bad = Expr::Record(vec![
        ("name".into(), Expr::String("lashlang".into())),
        (
            "labels".into(),
            Expr::List(vec![Expr::String("agent".into()), Expr::Number(42.0)]),
        ),
    ]);
    let failing = Program::block(vec![Expr::Finish(Box::new(Expr::BuiltinCall {
        name: "validate".into(),
        args: vec![
            bad,
            type_literal(vec![
                ("name", TypeExpr::Str, false),
                ("labels", TypeExpr::List(Box::new(TypeExpr::Str)), false),
            ]),
        ],
    }))]);
    let mut state = State::new();
    let err = lashlang::execute(
        &lashlang_compile_program(&failing).expect("the program compiles"),
        &mut state,
        &host,
    )
    .await
    .expect_err("validate should fail");
    let RuntimeError::ValidationFailed { reason } = err else {
        panic!("expected validation runtime error");
    };
    assert!(
        reason.contains("$.labels[1]: expected string, got number"),
        "{reason}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn undefined_ref_in_type_produces_runtime_error() {
    let program = Program::block(vec![Expr::Finish(Box::new(Expr::TypeLiteral(Box::new(
        TypeExpr::Object(vec![TypeField {
            name: "inner".into(),
            ty: TypeExpr::Ref("Missing".into()),
            optional: false,
        }]),
    ))))]);
    let host = TestHost::default();
    let mut state = State::new();
    let err = lashlang::execute(
        &lashlang_compile_program(&program).expect("the program compiles"),
        &mut state,
        &host,
    )
    .await
    .expect_err("Missing is undefined");
    assert!(matches!(err, RuntimeError::UndefinedVariable { .. }));
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_round_trip_preserves_type_values() {
    let program = Program::block(vec![
        assign(
            "Books",
            type_literal(vec![
                ("title", TypeExpr::Str, false),
                ("count", TypeExpr::Int, false),
            ]),
        ),
        Expr::Finish(Box::new(Expr::Variable("Books".into()))),
    ]);
    let host = TestHost::default();
    let mut state = State::new();
    let outcome = lashlang::execute(
        &lashlang_compile_program(&program).expect("the program compiles"),
        &mut state,
        &host,
    )
    .await
    .expect("should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let snapshot = state.snapshot();
    let serialized = snapshot.to_canonical_bytes().expect("serialize");
    let restored = lashlang::Snapshot::from_canonical_bytes(&serialized).expect("deserialize");
    let restored_state = State::from_snapshot(restored);
    // Re-execute a program that references Books — the ref should still resolve.
    let program2 = Program::block(vec![Expr::Finish(Box::new(Expr::Variable("Books".into())))]);
    let mut state2 = restored_state;
    let outcome2 = lashlang::execute(
        &lashlang_compile_program(&program2).expect("the program compiles"),
        &mut state2,
        &host,
    )
    .await
    .expect("run");
    let ExecutionOutcome::Finished(v2) = outcome2 else {
        panic!("expected finish");
    };
    assert_eq!(value, v2);
}

/// Compiles an IR program as the main entry of the raw module artifact it
/// forms, through the one public compile entry.
fn lashlang_compile_program(
    program: &lashlang::Program,
) -> Result<lashlang::CompiledProgram, Box<dyn std::error::Error>> {
    let artifact = lashlang::ModuleArtifact::from_program(program.clone())?;
    Ok(lashlang::compile(
        &artifact,
        lashlang::Entry::Main,
        Some(&program.spans),
    )?)
}
