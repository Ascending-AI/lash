use super::case_builders::*;
use super::*;

/// `finish <name>(<args>)`
fn finish_builtin(name: &str, args: Vec<Expr>) -> Program {
    finish_program(builders::builtin(name, args))
}

/// `<name>(<arg>)`
fn one(name: &str, arg: Expr) -> Expr {
    builders::builtin(name, vec![arg])
}

/// `<name>(<left>, <right>)`
fn two(name: &str, left: Expr, right: Expr) -> Expr {
    builders::builtin(name, vec![left, right])
}

/// `slice(<target>, <from>, <to>)`
fn slice(target: Expr, from: Expr, to: Expr) -> Expr {
    builders::builtin("slice", vec![target, from, to])
}

/// A list of number literals.
fn numbers(values: &[f64]) -> Expr {
    builders::list(values.iter().copied().map(builders::num).collect())
}

/// `Type { email: str | null }`
fn email_union_type() -> Expr {
    builders::type_literal(TypeExpr::Object(vec![builders::type_field(
        "email",
        TypeExpr::union(vec![TypeExpr::Str, TypeExpr::Null]),
        false,
    )]))
}

/// `finish validate({ email: <value> }, <schema>)`
fn finish_validate_email(value: Expr, schema: Expr) -> Program {
    finish_program(two(
        "validate",
        builders::record(vec![("email", value)]),
        schema,
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn arithmetic_and_compare_errors_are_covered() {
    assert_eq!(
        exec(finish_binary(
            builders::num(7.0),
            CoercingBinaryOp::Subtract,
            builders::num(2.0)
        ))
        .await
        .expect("subtract should succeed"),
        Value::Number(5.0)
    );
    assert_eq!(
        exec(finish_binary(
            builders::num(3.0),
            CoercingBinaryOp::Multiply,
            builders::num(4.0)
        ))
        .await
        .expect("multiply should succeed"),
        Value::Number(12.0)
    );
    assert_eq!(
        exec(finish_binary(
            builders::num(8.0),
            CoercingBinaryOp::Divide,
            builders::num(2.0)
        ))
        .await
        .expect("divide should succeed"),
        Value::Number(4.0)
    );
    // `finish 8 % 3`
    assert_eq!(
        exec(finish_binary(
            builders::num(8.0),
            CoercingBinaryOp::Remainder,
            builders::num(3.0)
        ))
        .await
        .expect("modulo should succeed"),
        Value::Number(2.0)
    );
    assert_eq!(
        exec(finish_binary(
            builders::num(1.0),
            CoercingBinaryOp::StrictNotEqual,
            builders::num(2.0)
        ))
        .await
        .expect("not equal should succeed"),
        Value::Bool(true)
    );
    assert_eq!(
        exec(finish_binary(
            builders::num(1.0),
            CoercingBinaryOp::LessEqual,
            builders::num(2.0)
        ))
        .await
        .expect("less-equal should succeed"),
        Value::Bool(true)
    );
    assert_eq!(
        exec(finish_binary(
            builders::num(2.0),
            CoercingBinaryOp::Greater,
            builders::num(1.0)
        ))
        .await
        .expect("greater should succeed"),
        Value::Bool(true)
    );
    assert_eq!(
        exec(finish_binary(
            builders::num(2.0),
            CoercingBinaryOp::GreaterEqual,
            builders::num(1.0)
        ))
        .await
        .expect("greater-equal should succeed"),
        Value::Bool(true)
    );

    let value = exec(finish_program(builders::concat(
        builders::list(vec![builders::num(1.0), builders::num(2.0)]),
        builders::list(vec![builders::num(3.0)]),
    )))
    .await
    .expect("list concat should succeed");
    assert_eq!(
        value,
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );

    let value = exec(finish_binary(
        builders::string("a"),
        CoercingBinaryOp::Add,
        builders::string("b"),
    ))
    .await
    .expect("string add should succeed");
    assert_eq!(value, Value::String("ab".to_string().into()));

    let value = exec(finish_binary(
        builders::string("a"),
        CoercingBinaryOp::Add,
        builders::num(1.0),
    ))
    .await
    .expect("string coercion should succeed");
    assert_eq!(value, Value::String("a1".to_string().into()));

    let value = exec(finish_binary(
        builders::num(1.0),
        CoercingBinaryOp::Add,
        builders::string("b"),
    ))
    .await
    .expect("string coercion should succeed");
    assert_eq!(value, Value::String("1b".to_string().into()));

    // `finish 1 + true`
    let value = exec(finish_binary(
        builders::num(1.0),
        CoercingBinaryOp::Add,
        builders::bool_lit(true),
    ))
    .await
    .expect("bool should coerce for addition");
    assert_eq!(value, Value::Number(2.0));

    // `finish null + 2`
    let value = exec(finish_binary(
        builders::null(),
        CoercingBinaryOp::Add,
        builders::num(2.0),
    ))
    .await
    .expect("null should coerce for addition");
    assert_eq!(value, Value::Number(2.0));

    let value = exec(finish_binary(
        builders::string("2"),
        CoercingBinaryOp::Multiply,
        builders::num(3.0),
    ))
    .await
    .expect("numeric strings should coerce");
    assert_eq!(value, Value::Number(6.0));

    let value = exec(finish_binary(
        builders::string("2"),
        CoercingBinaryOp::Less,
        builders::num(10.0),
    ))
    .await
    .expect("numeric strings should compare");
    assert_eq!(value, Value::Bool(true));

    // ECMA-262 `+` coerces rather than fails: a record's ToPrimitive runs its
    // default `toString`.
    let value = exec(finish_binary(
        builders::record(Vec::new()),
        CoercingBinaryOp::Add,
        builders::num(1.0),
    ))
    .await
    .expect("records coerce under ECMA-262 addition");
    assert_eq!(value, Value::String("[object Object]1".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_success_matrix_is_covered() {
    // `rec = { a: 1, b: 2 }` / `base = [1, 2]` / `finish { ... }` over every
    // builtin's success path; each field below is one call from that record.
    let value = exec(builders::program(vec![
        builders::assign(
            "rec",
            builders::record(vec![("a", builders::num(1.0)), ("b", builders::num(2.0))]),
        ),
        builders::assign(
            "base",
            builders::list(vec![builders::num(1.0), builders::num(2.0)]),
        ),
        builders::finish(builders::record(vec![
            ("len_s", one("len", builders::string("ab"))),
            ("len_l", one("len", numbers(&[1.0, 2.0, 3.0]))),
            ("len_r", one("len", builders::var("rec"))),
            ("len_n", one("len", builders::null())),
            ("empty_n", one("empty", builders::null())),
            ("empty_s", one("empty", builders::string(""))),
            ("empty_l", one("empty", builders::list(Vec::new()))),
            ("empty_r", one("empty", builders::record(Vec::new()))),
            ("keys_n", one("keys", builders::null())),
            ("values_n", one("values", builders::null())),
            ("keys", one("keys", builders::var("rec"))),
            ("values", one("values", builders::var("rec"))),
            (
                "contains_s",
                two("contains", builders::string("abc"), builders::string("b")),
            ),
            (
                "contains_num",
                two("contains", builders::string("123"), builders::num(2.0)),
            ),
            (
                "contains_l",
                two("contains", numbers(&[1.0, 2.0, 3.0]), builders::num(2.0)),
            ),
            (
                "contains_r",
                two(
                    "contains",
                    builders::record(vec![
                        ("foo", builders::num(1.0)),
                        ("bar", builders::num(2.0)),
                    ]),
                    builders::string("foo"),
                ),
            ),
            (
                "contains_n",
                two("contains", builders::null(), builders::num(2.0)),
            ),
            (
                "find_hit",
                two(
                    "find",
                    builders::string("alpha beta"),
                    builders::string("beta"),
                ),
            ),
            (
                "find_from",
                builders::builtin(
                    "find",
                    vec![
                        builders::string("banana"),
                        builders::string("na"),
                        builders::num(3.0),
                    ],
                ),
            ),
            (
                "find_missing",
                two("find", builders::string("alpha"), builders::string("z")),
            ),
            (
                "starts",
                two(
                    "starts_with",
                    builders::string("lash"),
                    builders::string("la"),
                ),
            ),
            (
                "starts_num",
                two("starts_with", builders::num(123.0), builders::num(12.0)),
            ),
            (
                "ends",
                two(
                    "ends_with",
                    builders::string("lash"),
                    builders::string("sh"),
                ),
            ),
            (
                "split",
                two("split", builders::num(101.0), builders::num(0.0)),
            ),
            (
                "join",
                two(
                    "join",
                    builders::list(vec![
                        builders::string("a"),
                        builders::num(2.0),
                        builders::bool_lit(true),
                    ]),
                    builders::string("-"),
                ),
            ),
            ("trim", one("trim", builders::num(101.0))),
            (
                "slice_s",
                slice(
                    builders::string("abcd"),
                    builders::num(1.0),
                    builders::num(3.0),
                ),
            ),
            (
                "slice_end_s",
                slice(
                    builders::string("abcd"),
                    builders::num(2.0),
                    builders::null(),
                ),
            ),
            (
                "slice_back_s",
                slice(
                    builders::string("abcd"),
                    builders::num(3.0),
                    builders::num(1.0),
                ),
            ),
            (
                "slice_from_start_s",
                slice(
                    builders::string("abcd"),
                    builders::null(),
                    builders::num(2.0),
                ),
            ),
            (
                "slice_l",
                slice(
                    numbers(&[1.0, 2.0, 3.0, 4.0]),
                    builders::num(1.0),
                    builders::num(3.0),
                ),
            ),
            (
                "slice_end_l",
                slice(
                    numbers(&[1.0, 2.0, 3.0, 4.0]),
                    builders::num(2.0),
                    builders::null(),
                ),
            ),
            (
                "slice_back_l",
                slice(
                    numbers(&[1.0, 2.0, 3.0, 4.0]),
                    builders::num(3.0),
                    builders::num(1.0),
                ),
            ),
            (
                "slice_from_start_l",
                slice(
                    numbers(&[1.0, 2.0, 3.0, 4.0]),
                    builders::null(),
                    builders::num(2.0),
                ),
            ),
            (
                "to_s",
                one(
                    "to_string",
                    builders::record(vec![("a", builders::num(1.0))]),
                ),
            ),
            ("to_i_n", one("to_int", builders::num(3.9))),
            ("to_i_s", one("to_int", builders::string("4"))),
            ("to_i_b", one("to_int", builders::bool_lit(true))),
            ("to_f_n", one("to_float", builders::num(1.0))),
            ("to_f_s", one("to_float", builders::string("2.5"))),
            ("to_f_nl", one("to_float", builders::null())),
            (
                "fmt",
                builders::builtin(
                    "format",
                    vec![
                        builders::string("x={},y={}"),
                        builders::num(1.0),
                        builders::bool_lit(true),
                    ],
                ),
            ),
            ("range_end", one("range", builders::num(3.0))),
            (
                "range_pair",
                two("range", builders::num(-2.0), builders::num(2.0)),
            ),
            (
                "range_step",
                builders::builtin(
                    "range",
                    vec![builders::num(0.0), builders::num(5.0), builders::num(2.0)],
                ),
            ),
            (
                "range_step_down",
                builders::builtin(
                    "range",
                    vec![builders::num(5.0), builders::num(0.0), builders::num(-2.0)],
                ),
            ),
            (
                "range_empty",
                two("range", builders::num(2.0), builders::num(2.0)),
            ),
            (
                "ceil_div",
                two("ceil_div", builders::num(10.0), builders::num(3.0)),
            ),
            (
                "floor_div",
                two("floor_div", builders::num(-10.0), builders::num(3.0)),
            ),
            (
                "pushed",
                two("push", builders::var("base"), builders::num(3.0)),
            ),
            ("base_after_push", builders::var("base")),
            (
                "valid",
                two(
                    "validate",
                    builders::record(vec![
                        ("name", builders::string("pkg")),
                        ("version", builders::string("1.0.0")),
                        (
                            "deps",
                            builders::list(vec![builders::record(vec![
                                ("name", builders::string("dep")),
                                ("optional", builders::bool_lit(true)),
                            ])]),
                        ),
                        ("extra", builders::string("preserved")),
                    ]),
                    builders::type_literal(TypeExpr::Object(vec![
                        builders::type_field("name", TypeExpr::Str, false),
                        builders::type_field("version", TypeExpr::Str, false),
                        builders::type_field(
                            "deps",
                            TypeExpr::List(Box::new(TypeExpr::Object(vec![
                                builders::type_field("name", TypeExpr::Str, false),
                                builders::type_field("optional", TypeExpr::Bool, true),
                            ]))),
                            false,
                        ),
                    ])),
                ),
            ),
        ])),
    ]))
    .await
    .expect("builtins should succeed");

    let record = value.as_record().expect("expected record");
    assert_eq!(record["len_s"], Value::Number(2.0));
    assert_eq!(record["len_n"], Value::Number(0.0));
    assert_eq!(record["contains_num"], Value::Bool(true));
    assert_eq!(record["contains_l"], Value::Bool(true));
    assert_eq!(record["contains_r"], Value::Bool(true));
    assert_eq!(record["contains_n"], Value::Bool(false));
    assert_eq!(record["find_hit"], Value::Number(6.0));
    assert_eq!(record["find_from"], Value::Number(4.0));
    assert_eq!(record["find_missing"], Value::Null);
    assert_eq!(record["keys_n"], Value::List(Vec::new().into()));
    assert_eq!(record["values_n"], Value::List(Vec::new().into()));
    assert_eq!(record["starts_num"], Value::Bool(true));
    assert_eq!(
        record["split"],
        Value::List(
            vec![
                Value::String("1".to_string().into()),
                Value::String("1".to_string().into())
            ]
            .into()
        )
    );
    assert_eq!(record["join"], Value::String("a-2-true".to_string().into()));
    assert_eq!(record["trim"], Value::String("101".to_string().into()));
    assert_eq!(record["slice_s"], Value::String("bc".to_string().into()));
    assert_eq!(
        record["slice_end_s"],
        Value::String("cd".to_string().into())
    );
    assert_eq!(record["slice_back_s"], Value::String(String::new().into()));
    assert_eq!(
        record["slice_from_start_s"],
        Value::String("ab".to_string().into())
    );
    assert_eq!(
        record["slice_end_l"],
        Value::List(vec![Value::Number(3.0), Value::Number(4.0)].into())
    );
    assert_eq!(record["slice_back_l"], Value::List(Vec::new().into()));
    assert_eq!(
        record["slice_from_start_l"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(record["to_i_n"], Value::Number(3.0));
    assert_eq!(record["to_i_b"], Value::Number(1.0));
    assert_eq!(record["to_f_s"], Value::Number(2.5));
    assert_eq!(record["to_f_nl"], Value::Number(0.0));
    assert_eq!(
        record["fmt"],
        Value::String("x=1,y=true".to_string().into())
    );
    assert_eq!(
        record["range_end"],
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(
        record["range_pair"],
        Value::List(
            vec![
                Value::Number(-2.0),
                Value::Number(-1.0),
                Value::Number(0.0),
                Value::Number(1.0)
            ]
            .into()
        )
    );
    assert_eq!(record["range_empty"], Value::List(Vec::new().into()));
    assert_eq!(
        record["range_step"],
        Value::List(vec![Value::Number(0.0), Value::Number(2.0), Value::Number(4.0)].into())
    );
    assert_eq!(
        record["range_step_down"],
        Value::List(vec![Value::Number(5.0), Value::Number(3.0), Value::Number(1.0)].into())
    );
    assert_eq!(record["ceil_div"], Value::Number(4.0));
    assert_eq!(record["floor_div"], Value::Number(-4.0));
    assert_eq!(
        record["pushed"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );
    assert_eq!(
        record["base_after_push"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
    let valid = record["valid"].as_record().expect("validated record");
    assert_eq!(valid["name"], Value::String("pkg".to_string().into()));
    assert_eq!(
        valid["extra"],
        Value::String("preserved".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn grep_text_returns_documented_line_records() {
    // `matches = grep_text("alpha\nbeta match\r\ngamma match\n", "match")`
    // / `finish matches`
    let value = exec(builders::program(vec![
        builders::assign(
            "matches",
            builders::builtin(
                "grep_text",
                vec![
                    builders::string("alpha\nbeta match\r\ngamma match\n"),
                    builders::string("match"),
                ],
            ),
        ),
        builders::finish(builders::var("matches")),
    ]))
    .await
    .expect("grep_text should succeed");

    let Value::List(matches) = value else {
        panic!("expected list");
    };
    assert_eq!(matches.len(), 2);
    let first = matches[0].as_record().expect("first match record");
    assert_eq!(first["line"], Value::Number(2.0));
    assert_eq!(first["text"], Value::String("beta match".into()));
    assert_eq!(first["match"], Value::String("match".into()));
    assert_eq!(first["start"], Value::Number(5.0));
    assert_eq!(first["end"], Value::Number(10.0));
}

#[tokio::test(flavor = "current_thread")]
async fn find_uses_character_offsets() {
    // `finish { first: find("éclair café", "café"), from_end: find("abc", "", 3),`
    // `beyond_end: find("abc", "a", 4) }`
    let value = exec(finish_program(builders::record(vec![
        (
            "first",
            builders::builtin(
                "find",
                vec![builders::string("éclair café"), builders::string("café")],
            ),
        ),
        (
            "from_end",
            builders::builtin(
                "find",
                vec![
                    builders::string("abc"),
                    builders::string(""),
                    builders::num(3.0),
                ],
            ),
        ),
        (
            "beyond_end",
            builders::builtin(
                "find",
                vec![
                    builders::string("abc"),
                    builders::string("a"),
                    builders::num(4.0),
                ],
            ),
        ),
    ])))
    .await
    .expect("find should succeed");

    let record = value.as_record().expect("record");
    assert_eq!(record["first"], Value::Number(7.0));
    assert_eq!(record["from_end"], Value::Number(3.0));
    assert_eq!(record["beyond_end"], Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn validate_reports_precise_shape_errors() {
    let cases = [
        (
            finish_builtin(
                "validate",
                vec![
                    builders::record(vec![("name", builders::string("pkg"))]),
                    builders::type_literal(TypeExpr::Object(vec![
                        builders::type_field("name", TypeExpr::Str, false),
                        builders::type_field("version", TypeExpr::Str, false),
                    ])),
                ],
            ),
            "validation failed: $: missing required field `version`",
        ),
        (
            finish_builtin(
                "validate",
                vec![
                    builders::record(vec![(
                        "packages",
                        builders::list(vec![builders::record(vec![
                            ("name", builders::string("pkg")),
                            ("version", builders::num(1.0)),
                        ])]),
                    )]),
                    builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                        "packages",
                        TypeExpr::List(Box::new(TypeExpr::Object(vec![
                            builders::type_field("name", TypeExpr::Str, false),
                            builders::type_field("version", TypeExpr::Str, false),
                        ]))),
                        false,
                    )])),
                ],
            ),
            "validation failed: $.packages[0].version: expected string, got number",
        ),
        (
            finish_builtin(
                "validate",
                vec![
                    builders::record(vec![("status", builders::string("maybe"))]),
                    builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                        "status",
                        TypeExpr::Enum(vec!["ok".into(), "err".into()]),
                        false,
                    )])),
                ],
            ),
            "validation failed: $.status: expected one of [ok, err], got maybe",
        ),
        (
            finish_builtin(
                "validate",
                vec![
                    builders::record(vec![("count", builders::num(1.5))]),
                    builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                        "count",
                        TypeExpr::Int,
                        false,
                    )])),
                ],
            ),
            "validation failed: $.count: expected integer, got number",
        ),
    ];

    for (program, expected) in cases {
        let err = exec(program)
            .await
            .expect_err("validate should reject bad value");
        assert_eq!(
            err,
            RuntimeError::ValidationFailed {
                reason: expected
                    .strip_prefix("validation failed: ")
                    .expect("validation prefix")
                    .to_string()
            }
        );
    }

    // `finish validate({ name: "pkg" }, { type: "object" })`
    let err = exec(finish_builtin(
        "validate",
        vec![
            builders::record(vec![("name", builders::string("pkg"))]),
            builders::record(vec![("type", builders::string("object"))]),
        ],
    ))
    .await
    .expect_err("raw schema records should be rejected");
    assert_eq!(err, RuntimeError::ValidateTypeLiteralRequired);
}

#[tokio::test(flavor = "current_thread")]
async fn validate_union_accepts_any_variant() {
    // `str | null` must accept both a string and a null.
    let out = exec(finish_validate_email(
        builders::string("a@b"),
        email_union_type(),
    ))
    .await
    .expect("string-branch validate should succeed");
    assert_eq!(
        out,
        Value::Record(Arc::new({
            let mut rec = record_with_capacity(1);
            rec.insert("email".into(), Value::String("a@b".into()));
            rec
        }))
    );

    let out = exec(finish_validate_email(builders::null(), email_union_type()))
        .await
        .expect("null-branch validate should succeed");
    let Value::Record(rec) = &out else {
        panic!("expected record");
    };
    assert!(matches!(rec.get("email"), Some(Value::Null)));
}

#[tokio::test(flavor = "current_thread")]
async fn validate_static_and_dynamic_type_paths_share_error_text() {
    let static_err = exec(finish_validate_email(
        builders::num(42.0),
        email_union_type(),
    ))
    .await
    .expect_err("static Type literal should reject number");
    // `Schema = await tools.echo({ value: Type { email: str | null } })?`
    // / `finish validate({ email: 42 }, Schema)`
    let dynamic_err = exec(builders::program(vec![
        builders::assign("Schema", await_echo_unwrap(email_union_type())),
        builders::finish(two(
            "validate",
            builders::record(vec![("email", builders::num(42.0))]),
            builders::var("Schema"),
        )),
    ]))
    .await
    .expect_err("runtime Type value should reject number");

    assert_eq!(static_err, dynamic_err);
    assert_eq!(
        dynamic_err,
        RuntimeError::ValidationFailed {
            reason: "$.email: expected one of [string, null], got number".to_string()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn validate_object_type_accepts_image_descriptors() {
    // `finish validate(img, Type { type: str, id: str, label: str, size: int,`
    // `width: int | null, height: int | null })`
    let program = finish_program(two(
        "validate",
        builders::var("img"),
        builders::type_literal(TypeExpr::Object(vec![
            builders::type_field("type", TypeExpr::Str, false),
            builders::type_field("id", TypeExpr::Str, false),
            builders::type_field("label", TypeExpr::Str, false),
            builders::type_field("size", TypeExpr::Int, false),
            builders::type_field(
                "width",
                TypeExpr::union(vec![TypeExpr::Int, TypeExpr::Null]),
                false,
            ),
            builders::type_field(
                "height",
                TypeExpr::union(vec![TypeExpr::Int, TypeExpr::Null]),
                false,
            ),
        ])),
    ));
    let mut state = State::new();
    state
        .insert_global(
            "img",
            Value::Image(Box::new(ImageValue::new(
                "img-1",
                crate::MediaType::parse("image/png").unwrap(),
                "chart.png",
                1234,
                Some(640),
                None,
            ))),
        )
        .expect("seeding an image global stays within the heap bound");

    let outcome = execute_program(&program, &mut state, &Host)
        .await
        .expect("image descriptor validation should succeed");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::Image(Box::new(ImageValue::new(
            "img-1",
            crate::MediaType::parse("image/png").unwrap(),
            "chart.png",
            1234,
            Some(640),
            None
        ))))
    );
}
