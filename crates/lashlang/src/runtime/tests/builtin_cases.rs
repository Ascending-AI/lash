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
    // `finish 7 - 2`
    assert_eq!(
        exec(finish_binary(
            builders::num(7.0),
            BinaryOp::Subtract,
            builders::num(2.0)
        ))
        .await
        .expect("subtract should succeed"),
        Value::Number(5.0)
    );
    // `finish 3 * 4`
    assert_eq!(
        exec(finish_binary(
            builders::num(3.0),
            BinaryOp::Multiply,
            builders::num(4.0)
        ))
        .await
        .expect("multiply should succeed"),
        Value::Number(12.0)
    );
    // `finish 8 / 2`
    assert_eq!(
        exec(finish_binary(
            builders::num(8.0),
            BinaryOp::Divide,
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
            BinaryOp::Modulo,
            builders::num(3.0)
        ))
        .await
        .expect("modulo should succeed"),
        Value::Number(2.0)
    );
    // `finish 1 != 2`
    assert_eq!(
        exec(finish_binary(
            builders::num(1.0),
            BinaryOp::NotEqual,
            builders::num(2.0)
        ))
        .await
        .expect("not equal should succeed"),
        Value::Bool(true)
    );
    // `finish 1 <= 2`
    assert_eq!(
        exec(finish_binary(
            builders::num(1.0),
            BinaryOp::LessEqual,
            builders::num(2.0)
        ))
        .await
        .expect("less-equal should succeed"),
        Value::Bool(true)
    );
    // `finish 2 > 1`
    assert_eq!(
        exec(finish_binary(
            builders::num(2.0),
            BinaryOp::Greater,
            builders::num(1.0)
        ))
        .await
        .expect("greater should succeed"),
        Value::Bool(true)
    );
    // `finish 2 >= 1`
    assert_eq!(
        exec(finish_binary(
            builders::num(2.0),
            BinaryOp::GreaterEqual,
            builders::num(1.0)
        ))
        .await
        .expect("greater-equal should succeed"),
        Value::Bool(true)
    );

    // `finish [1,2] + [3]`
    let value = exec(finish_binary(
        builders::list(vec![builders::num(1.0), builders::num(2.0)]),
        BinaryOp::Add,
        builders::list(vec![builders::num(3.0)]),
    ))
    .await
    .expect("list concat should succeed");
    assert_eq!(
        value,
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );

    // `finish "a" + "b"`
    let value = exec(finish_binary(
        builders::string("a"),
        BinaryOp::Add,
        builders::string("b"),
    ))
    .await
    .expect("string add should succeed");
    assert_eq!(value, Value::String("ab".to_string().into()));

    // `finish "a" + 1`
    let value = exec(finish_binary(
        builders::string("a"),
        BinaryOp::Add,
        builders::num(1.0),
    ))
    .await
    .expect("string coercion should succeed");
    assert_eq!(value, Value::String("a1".to_string().into()));

    // `finish 1 + "b"`
    let value = exec(finish_binary(
        builders::num(1.0),
        BinaryOp::Add,
        builders::string("b"),
    ))
    .await
    .expect("string coercion should succeed");
    assert_eq!(value, Value::String("1b".to_string().into()));

    // `finish 1 + true`
    let value = exec(finish_binary(
        builders::num(1.0),
        BinaryOp::Add,
        builders::bool_lit(true),
    ))
    .await
    .expect("bool should coerce for addition");
    assert_eq!(value, Value::Number(2.0));

    // `finish null + 2`
    let value = exec(finish_binary(
        builders::null(),
        BinaryOp::Add,
        builders::num(2.0),
    ))
    .await
    .expect("null should coerce for addition");
    assert_eq!(value, Value::Number(2.0));

    // `finish "2" * 3`
    let value = exec(finish_binary(
        builders::string("2"),
        BinaryOp::Multiply,
        builders::num(3.0),
    ))
    .await
    .expect("numeric strings should coerce");
    assert_eq!(value, Value::Number(6.0));

    // `finish "2" < 10`
    let value = exec(finish_binary(
        builders::string("2"),
        BinaryOp::Less,
        builders::num(10.0),
    ))
    .await
    .expect("numeric strings should compare");
    assert_eq!(value, Value::Bool(true));

    // `finish {} + 1`
    let err = exec(finish_binary(
        builders::record(Vec::new()),
        BinaryOp::Add,
        builders::num(1.0),
    ))
    .await
    .expect_err("records should still fail arithmetic");
    assert!(matches!(err, RuntimeError::ExpectedNumberType { .. }));
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
async fn builtin_error_matrix_is_covered() {
    let cases = [
        (
            "finish len(true)",
            finish_builtin("len", vec![builders::bool_lit(true)]),
        ),
        (
            "finish empty(true)",
            finish_builtin("empty", vec![builders::bool_lit(true)]),
        ),
        (
            "finish keys([])",
            finish_builtin("keys", vec![builders::list(Vec::new())]),
        ),
        (
            "finish values([])",
            finish_builtin("values", vec![builders::list(Vec::new())]),
        ),
        (
            "finish contains(1, 2)",
            finish_builtin("contains", vec![builders::num(1.0), builders::num(2.0)]),
        ),
        (
            "finish find(\"a\")",
            finish_builtin("find", vec![builders::string("a")]),
        ),
        (
            "finish find(\"a\", \"a\", -1)",
            finish_builtin(
                "find",
                vec![
                    builders::string("a"),
                    builders::string("a"),
                    builders::num(-1.0),
                ],
            ),
        ),
        (
            "finish grep_text({}, \"a\")",
            finish_builtin(
                "grep_text",
                vec![builders::record(Vec::new()), builders::string("a")],
            ),
        ),
        (
            "finish grep_text(\"a\", \"\")",
            finish_builtin(
                "grep_text",
                vec![builders::string("a"), builders::string("")],
            ),
        ),
        (
            "finish starts_with({}, \"a\")",
            finish_builtin(
                "starts_with",
                vec![builders::record(Vec::new()), builders::string("a")],
            ),
        ),
        (
            "finish ends_with({}, \"a\")",
            finish_builtin(
                "ends_with",
                vec![builders::record(Vec::new()), builders::string("a")],
            ),
        ),
        (
            "finish split({}, \",\")",
            finish_builtin(
                "split",
                vec![builders::record(Vec::new()), builders::string(",")],
            ),
        ),
        (
            "finish join(1, \",\")",
            finish_builtin("join", vec![builders::num(1.0), builders::string(",")]),
        ),
        (
            "finish trim({})",
            finish_builtin("trim", vec![builders::record(Vec::new())]),
        ),
        (
            "finish slice(1, 0, 1)",
            finish_builtin(
                "slice",
                vec![builders::num(1.0), builders::num(0.0), builders::num(1.0)],
            ),
        ),
        (
            "finish to_int({})",
            finish_builtin("to_int", vec![builders::record(Vec::new())]),
        ),
        (
            "finish to_int(\"x\")",
            finish_builtin("to_int", vec![builders::string("x")]),
        ),
        (
            "finish to_float({})",
            finish_builtin("to_float", vec![builders::record(Vec::new())]),
        ),
        (
            "finish to_float(\"x\")",
            finish_builtin("to_float", vec![builders::string("x")]),
        ),
        (
            "finish json_parse(\"{\")",
            finish_builtin("json_parse", vec![builders::string("{")]),
        ),
        ("finish format()", finish_builtin("format", Vec::new())),
        (
            "finish format({})",
            finish_builtin("format", vec![builders::record(Vec::new())]),
        ),
        (
            "finish format(\"{1}\", \"x\")",
            finish_builtin(
                "format",
                vec![builders::string("{1}"), builders::string("x")],
            ),
        ),
        (
            "finish format(\"{}\", \"x\", \"y\")",
            finish_builtin(
                "format",
                vec![
                    builders::string("{}"),
                    builders::string("x"),
                    builders::string("y"),
                ],
            ),
        ),
        (
            "finish format(\"{} {1}\", \"x\", \"y\")",
            finish_builtin(
                "format",
                vec![
                    builders::string("{} {1}"),
                    builders::string("x"),
                    builders::string("y"),
                ],
            ),
        ),
        (
            "finish format(\"{x}\")",
            finish_builtin("format", vec![builders::string("{x}")]),
        ),
        (
            "finish format(\"{\")",
            finish_builtin("format", vec![builders::string("{")]),
        ),
        (
            "finish format(\"}\")",
            finish_builtin("format", vec![builders::string("}")]),
        ),
        (
            "finish validate({ name: \"pkg\" }, { type: \"object\" })",
            finish_builtin(
                "validate",
                vec![
                    builders::record(vec![("name", builders::string("pkg"))]),
                    builders::record(vec![("type", builders::string("object"))]),
                ],
            ),
        ),
        ("finish range()", finish_builtin("range", Vec::new())),
        (
            "finish range(1, 2, 3, 4)",
            finish_builtin(
                "range",
                vec![
                    builders::num(1.0),
                    builders::num(2.0),
                    builders::num(3.0),
                    builders::num(4.0),
                ],
            ),
        ),
        (
            "finish range(\"3\")",
            finish_builtin("range", vec![builders::string("3")]),
        ),
        (
            "finish range(1.5)",
            finish_builtin("range", vec![builders::num(1.5)]),
        ),
        (
            "finish range(0, 5, 0)",
            finish_builtin(
                "range",
                vec![builders::num(0.0), builders::num(5.0), builders::num(0.0)],
            ),
        ),
        (
            "finish range(0, 1000001)",
            finish_builtin(
                "range",
                vec![builders::num(0.0), builders::num(1_000_001.0)],
            ),
        ),
        (
            "finish range(1000001, 0, -1)",
            finish_builtin(
                "range",
                vec![
                    builders::num(1_000_001.0),
                    builders::num(0.0),
                    builders::num(-1.0),
                ],
            ),
        ),
        ("finish ceil_div()", finish_builtin("ceil_div", Vec::new())),
        (
            "finish ceil_div(1.5, 1)",
            finish_builtin("ceil_div", vec![builders::num(1.5), builders::num(1.0)]),
        ),
        (
            "finish ceil_div(1, 0)",
            finish_builtin("ceil_div", vec![builders::num(1.0), builders::num(0.0)]),
        ),
        (
            "finish floor_div(\"1\", 1)",
            finish_builtin("floor_div", vec![builders::string("1"), builders::num(1.0)]),
        ),
        (
            "finish push(1, 2)",
            finish_builtin("push", vec![builders::num(1.0), builders::num(2.0)]),
        ),
        (
            "finish push([1])",
            finish_builtin("push", vec![builders::list(vec![builders::num(1.0)])]),
        ),
        (
            "finish no_such_builtin()",
            finish_builtin("no_such_builtin", Vec::new()),
        ),
    ];

    for (source, program) in cases {
        let err = exec(program).await.expect_err("builtin should fail");
        assert!(
            matches!(
                err,
                RuntimeError::InvalidArgumentCount { .. }
                    | RuntimeError::EmptyUnsupported
                    | RuntimeError::KeysUnsupported
                    | RuntimeError::ValuesUnsupported
                    | RuntimeError::LenUnsupported
                    | RuntimeError::ContainsUnsupported
                    | RuntimeError::InUnsupported
                    | RuntimeError::InvalidCharacterIndex { .. }
                    | RuntimeError::ExpectedText { .. }
                    | RuntimeError::EmptyGrepNeedle
                    | RuntimeError::JoinUnsupported
                    | RuntimeError::SliceUnsupported
                    | RuntimeError::ExpectedNumber
                    | RuntimeError::ExpectedNumberType { .. }
                    | RuntimeError::InvalidJson { .. }
                    | RuntimeError::FormatTemplateMissing
                    | RuntimeError::FormatTemplateInvalid { .. }
                    | RuntimeError::Format(_)
                    | RuntimeError::ValidateTypeLiteralRequired
                    | RuntimeError::InvalidRangeBound
                    | RuntimeError::InvalidRangeBoundType { .. }
                    | RuntimeError::ZeroRangeStep
                    | RuntimeError::RangeTooLarge { .. }
                    | RuntimeError::InvalidIntegerDivisionArgument { .. }
                    | RuntimeError::InvalidIntegerDivisionArgumentType { .. }
                    | RuntimeError::IntegerDivisionByZero { .. }
                    | RuntimeError::PushUnsupported
                    | RuntimeError::UnknownBuiltin { .. }
            ),
            "{source}: {err:?}"
        );
    }

    // `finish len()`
    let err = exec(finish_builtin("len", Vec::new()))
        .await
        .expect_err("arity error should fail");
    assert!(matches!(err, RuntimeError::InvalidArgumentCount { .. }));
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
async fn validate_union_rejects_value_matching_no_variant() {
    let err = exec(finish_validate_email(
        builders::num(42.0),
        email_union_type(),
    ))
    .await
    .expect_err("number should not match str | null");
    let RuntimeError::ValidationFailed { reason } = err else {
        panic!("expected ValidationFailed");
    };
    assert!(
        reason.contains("$.email"),
        "error should point at the failing field: {reason}",
    );
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

#[tokio::test(flavor = "current_thread")]
async fn helper_functions_are_covered_directly() {
    assert!(expect_arg_count("x", &[Value::Null], 1).is_ok());
    assert!(expect_arg_count("x", &[], 1).is_err());
    assert_eq!(as_number(&Value::Number(1.0)).expect("number"), 1.0);
    assert_eq!(as_number(&Value::Bool(true)).expect("bool"), 1.0);
    assert_eq!(as_number(&Value::Null).expect("null"), 0.0);
    assert_eq!(
        as_number(&Value::String("2.5".to_string().into())).expect("numeric"),
        2.5
    );
    assert_eq!(
        coerce_string(&Value::String("x".to_string().into())).expect("string"),
        "x"
    );
    assert_eq!(coerce_string(&Value::Bool(true)).expect("bool"), "true");
    assert_eq!(as_offset(&Value::Number(-1.0)).expect("offset"), -1);
    assert_eq!(as_slice_bound(&Value::Null).expect("null bound"), None);
    assert_eq!(
        as_slice_bound(&Value::Number(2.0)).expect("numeric bound"),
        Some(2)
    );
    assert_eq!(
        as_slice_bound(&Value::Number(-2.0)).expect("negative numeric bound"),
        Some(-2)
    );
    assert_eq!(slice_string("héllo", Some(1), Some(4)), "éll");
    assert_eq!(slice_string("abcdef", Some(-2), None), "ef");
    assert_eq!(slice_string("abcdef", None, Some(-1)), "abcde");
    assert_eq!(slice_string("abcdef", Some(-5), Some(-2)), "bcd");
    assert_eq!(slice_string("abc", Some(1), None), "bc");
    assert_eq!(slice_string("abc", Some(3), Some(1)), "");
    assert_eq!(clamp_slice_bounds(Some(1), Some(3), 4), Some((1, 3)));
    assert_eq!(clamp_slice_bounds(Some(1), None, 4), Some((1, 4)));
    assert_eq!(clamp_slice_bounds(None, Some(2), 4), Some((0, 2)));
    assert_eq!(clamp_slice_bounds(Some(-2), None, 4), Some((2, 4)));
    assert_eq!(clamp_slice_bounds(None, Some(-1), 4), Some((0, 3)));
    assert_eq!(clamp_slice_bounds(Some(-10), Some(10), 4), Some((0, 4)));
    assert_eq!(clamp_slice_bounds(Some(3), Some(1), 4), None);
    assert_eq!(
        resolve_index(&Value::Number(-1.0), 3).expect("resolved"),
        Some(2)
    );
    assert_eq!(
        resolve_index(&Value::Number(-4.0), 3).expect("resolved"),
        None
    );

    assert_eq!(
        compare_numbers(Value::Number(1.0), Value::Number(2.0), |a, b| a < b).expect("compare"),
        Value::Bool(true)
    );
    assert_eq!(
        compare_ordered(
            Value::String("abc".to_string().into()),
            Value::String("def".to_string().into()),
            |a, b| a < b,
            |a, b| a < b,
        )
        .expect("string compare"),
        Value::Bool(true)
    );
    assert_eq!(
        add_values(Value::Number(1.0), Value::Number(2.0)).expect("add"),
        Value::Number(3.0)
    );
    assert_eq!(
        add_values(Value::String("a".to_string().into()), Value::Bool(true)).expect("concat"),
        Value::String("atrue".to_string().into())
    );
    assert_eq!(
        add_values(Value::Bool(true), Value::Number(2.0)).expect("numeric coercion"),
        Value::Number(3.0)
    );
    assert_eq!(
        success(Value::Number(1.0)).as_record().unwrap()["ok"],
        Value::Bool(true)
    );
    assert_eq!(
        execution_host_error_value(ExecutionHostError::new("x"), "test")
            .as_record()
            .unwrap()["error"],
        Value::String("x".to_string().into())
    );
    assert_eq!(stringify_value(&Value::Null).expect("stringify"), "null");
    assert_eq!(
        stringify_value(&Value::Number(1.0)).expect("stringify"),
        "1"
    );
    assert_eq!(
        stringify_value(&Value::List(
            vec![Value::Number(1.0), Value::Number(2.0)].into()
        ))
        .expect("stringify"),
        "[1,2]"
    );
    assert_eq!(
        stringify_value(&Value::Tuple(
            vec![Value::Number(1.0), Value::String("x".into())].into()
        ))
        .expect("stringify"),
        r#"(1, "x")"#
    );
    assert_eq!(
        stringify_value(&Value::Tuple(vec![Value::Number(1.0)].into())).expect("stringify"),
        "(1,)"
    );
    assert_eq!(
        stringify_value(&Value::Tuple(Vec::new().into())).expect("stringify"),
        "()"
    );
    assert_eq!(
        add_values(
            Value::Tuple(vec![Value::Number(1.0)].into()),
            Value::Tuple(vec![Value::Number(2.0)].into())
        )
        .expect("tuple concat"),
        Value::Tuple(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
    let mut appended = String::from("prefix:");
    append_stringified_value(&mut appended, &Value::Bool(true)).expect("append stringify");
    assert_eq!(appended, "prefix:true");
    assert_eq!(
        apply_format("a{}b", &[Value::Number(1.0)]).expect("format"),
        "a1b"
    );
    let compiled_one_arg = compile_format_template("a{}b", 1);
    let one_arg = compiled_one_arg
        .one_arg
        .as_ref()
        .expect("single placeholder template should keep its direct shape");
    assert_eq!(one_arg.prefix.as_deref(), Some("a"));
    assert_eq!(one_arg.suffix.as_deref(), Some("b"));
    assert_eq!(
        execute_compiled_format_one_number_compact_direct(&compiled_one_arg, 42.0)
            .expect("compiled one-number format")
            .as_str(),
        "a42b"
    );
    assert!(
        compile_format_template("{}:{}", 2).one_arg.is_none(),
        "multi-arg templates should keep the generic compiled format path"
    );
    assert_eq!(
        apply_format(
            "b={1} a={0}",
            &[Value::String("x".into()), Value::String("y".into())]
        )
        .expect("indexed format"),
        "b=y a=x"
    );
    assert_eq!(
        apply_format("{{{}}}", &[Value::Number(1.0)]).expect("escaped braces"),
        "{1}"
    );
    assert_eq!(
        apply_format("{999999999999999999999999999999999999}", &[])
            .expect_err("overflow slot should fail"),
        RuntimeError::Format(FormatError::InvalidSlot {
            slot: "999999999999999999999999999999999999".to_string()
        })
    );
    assert_eq!(
        apply_format("{x}", &[]).expect_err("invalid placeholder should fail"),
        RuntimeError::Format(FormatError::InvalidPlaceholder)
    );
    assert_eq!(
        apply_format(
            "{} {1}",
            &[Value::String("x".into()), Value::String("y".into())]
        )
        .expect_err("mixed placeholder styles should fail"),
        RuntimeError::Format(FormatError::MixedPlaceholderKinds)
    );
    assert_eq!(
        apply_format("{", &[]).expect_err("unmatched open brace should fail"),
        RuntimeError::Format(FormatError::UnmatchedOpenBrace)
    );
    assert_eq!(
        apply_format("}", &[]).expect_err("unmatched close brace should fail"),
        RuntimeError::Format(FormatError::UnmatchedCloseBrace)
    );
    assert_eq!(
        apply_format("plain", &[Value::Number(1.0)]).expect_err("unused arg should fail"),
        RuntimeError::Format(FormatError::UnusedArgument { index: 0 })
    );
    assert_eq!(
        value_type_name(&Value::Record(Record::default().into())),
        "record"
    );
    assert_eq!(value_type_name(&Value::Tuple(Vec::new().into())), "tuple");
    assert_eq!(
        RuntimeError::UndefinedVariable {
            name: "x".to_string()
        }
        .to_string(),
        "unknown name `x`"
    );
    assert_eq!(
        RuntimeError::CannotIndex {
            actual: "record".to_string()
        }
        .to_string(),
        "can't index record"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn json_helpers_cover_special_paths() {
    let json = to_json(&Value::Number(f64::NAN));
    assert_eq!(json, serde_json::Value::Null);
    assert_eq!(to_json(&Value::Null), serde_json::Value::Null);
    assert_eq!(to_json(&Value::Bool(true)), serde_json::Value::Bool(true));
    assert_eq!(
        to_json(&Value::String("x".to_string().into())),
        serde_json::Value::String("x".to_string())
    );
    assert_eq!(
        to_json(&Value::List(vec![Value::Number(1.0)].into())),
        serde_json::json!([1])
    );
    assert_eq!(
        to_json(&Value::Tuple(vec![Value::Number(1.0)].into())),
        serde_json::json!([1])
    );
    assert_eq!(
        to_json(&Value::Record({
            let mut record = Record::default();
            record.insert("a".to_string(), Value::Number(1.0));
            record.into()
        })),
        serde_json::json!({"a": 1})
    );

    let value = from_json(serde_json::json!({
        "a": [1, true, null, "x"]
    }));
    let record = value.as_record().expect("expected record");
    assert!(matches!(record["a"], Value::List(_)));
}
