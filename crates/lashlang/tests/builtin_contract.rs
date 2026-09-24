//! The IR's builtin library: every name the registry advertises, what it
//! returns, and how it fails.
//!
//! This file replaces the retired dialect's prompt-claim suite. That suite
//! existed to keep the lashlang RLM system prompt and the runtime from
//! drifting apart, and it spelled every claim in lashlang source. ADR 0096
//! makes TypeScript the sole authored dialect: the TypeScript prompt's claims
//! are pinned by the TypeScript dialect's own contract and fluency suites
//! (`crates/lash-protocol-rlm/src/prompt_contract_tests.rs` and
//! `crates/lash-typescript/tests/`), and re-spelling them here would duplicate
//! that coverage against a prompt this crate no longer renders.
//!
//! What does not move is the builtin library itself. It is an IR facility with
//! no TypeScript spelling — a TypeScript author reaches ECMA methods, which
//! the VM implements separately — so the only remaining caller is a program
//! built from the AST, and that is how every row here is written. The drift
//! guard survives too: the smoke table below must name exactly the registry,
//! so a builtin cannot be added or removed without this file saying so.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, Expr,
    RuntimeError, State, TypeExpr, TypeField, Value,
};

use crate::ast_support::{call, finish_program, list, number, program, string};

/// The builtins are pure, so no host ability is reachable from these programs.
#[derive(Default)]
struct PureHost;

impl ExecutionHost for PureHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "a builtin program performed an ability: {other:?}"
            ))),
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "the fixture programs are well formed; only execution errors are under test"
)]
async fn run(program: lashlang::Program) -> Result<Value, RuntimeError> {
    let mut state = State::new();
    match lashlang::execute(
        &lashlang_compile_program(&program).expect("the program compiles"),
        &mut state,
        &PureHost,
    )
    .await?
    {
        ExecutionOutcome::Finished(value) => Ok(value),
        other => panic!("expected the program to finish, got {other:?}"),
    }
}

async fn finish_call(name: &str, args: Vec<Expr>) -> Value {
    run(finish_program(call(name, args)))
        .await
        .unwrap_or_else(|error| panic!("`{name}` should run: {error}"))
}

fn numbers(values: [f64; 3]) -> Expr {
    list(values.into_iter().map(number).collect())
}

fn record(fields: Vec<(&str, Expr)>) -> Expr {
    Expr::Record(
        fields
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}

fn strings(values: &[&str]) -> Value {
    Value::List(
        values
            .iter()
            .map(|value| Value::String((*value).into()))
            .collect::<Vec<_>>()
            .into(),
    )
}

fn list_of(values: &[f64]) -> Value {
    Value::List(
        values
            .iter()
            .copied()
            .map(Value::Number)
            .collect::<Vec<_>>()
            .into(),
    )
}

// ── Length and emptiness ──────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn len_measures_strings_lists_records_and_null() {
    assert_eq!(
        finish_call("len", vec![string("hi")]).await,
        Value::Number(2.0)
    );
    assert_eq!(
        finish_call("len", vec![numbers([1.0, 2.0, 3.0])]).await,
        Value::Number(3.0)
    );
    assert_eq!(
        finish_call(
            "len",
            vec![record(vec![("a", number(1.0)), ("b", number(2.0))])]
        )
        .await,
        Value::Number(2.0)
    );
    assert_eq!(
        finish_call("len", vec![Expr::Null]).await,
        Value::Number(0.0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn empty_checks_zero_length() {
    assert_eq!(
        finish_call("empty", vec![string("")]).await,
        Value::Bool(true)
    );
    assert_eq!(
        finish_call("empty", vec![string("x")]).await,
        Value::Bool(false)
    );
    assert_eq!(
        finish_call("empty", vec![list(Vec::new())]).await,
        Value::Bool(true)
    );
    assert_eq!(
        finish_call("empty", vec![list(vec![number(1.0)])]).await,
        Value::Bool(false)
    );
}

// ── Slicing, splitting and joining ────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn slice_supports_null_and_negative_bounds() {
    assert_eq!(
        finish_call("slice", vec![string("abcdef"), Expr::Null, number(3.0)]).await,
        Value::String("abc".into())
    );
    assert_eq!(
        finish_call("slice", vec![string("abcdef"), number(3.0), Expr::Null]).await,
        Value::String("def".into())
    );
    assert_eq!(
        finish_call("slice", vec![string("abcdef"), number(0.0), number(-2.0)]).await,
        Value::String("abcd".into())
    );
    assert_eq!(
        finish_call(
            "slice",
            vec![
                list(vec![number(1.0), number(2.0), number(3.0), number(4.0)]),
                number(1.0),
                number(3.0),
            ]
        )
        .await,
        list_of(&[2.0, 3.0])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn split_and_join_are_inverses_over_a_separator() {
    assert_eq!(
        finish_call("split", vec![string("a,b,c"), string(",")]).await,
        strings(&["a", "b", "c"])
    );
    assert_eq!(
        finish_call(
            "join",
            vec![
                list(vec![string("a"), string("b"), string("c")]),
                string("-")
            ]
        )
        .await,
        Value::String("a-b-c".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn trim_strips_surrounding_whitespace() {
    assert_eq!(
        finish_call("trim", vec![string("  hi  ")]).await,
        Value::String("hi".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn starts_with_ends_with_and_contains_test_membership() {
    assert_eq!(
        finish_call("starts_with", vec![string("foobar"), string("foo")]).await,
        Value::Bool(true)
    );
    assert_eq!(
        finish_call("ends_with", vec![string("foobar"), string("bar")]).await,
        Value::Bool(true)
    );
    assert_eq!(
        finish_call("contains", vec![string("foobar"), string("oob")]).await,
        Value::Bool(true)
    );
    assert_eq!(
        finish_call("contains", vec![numbers([1.0, 2.0, 3.0]), number(2.0)]).await,
        Value::Bool(true)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn find_returns_an_offset_or_null_and_grep_text_returns_located_matches() {
    assert_eq!(
        finish_call("find", vec![string("alpha beta"), string("beta")]).await,
        Value::Number(6.0)
    );
    assert_eq!(
        finish_call("find", vec![string("alpha"), string("z")]).await,
        Value::Null
    );

    let value = finish_call(
        "grep_text",
        vec![string("one\nmatch here\r\nmatch again\n"), string("match")],
    )
    .await;
    let Value::List(matches) = value else {
        panic!("expected a list of matches");
    };
    assert_eq!(matches.len(), 2);
    let first = matches[0].as_record().expect("match record");
    assert_eq!(first["line"], Value::Number(2.0));
    assert_eq!(first["text"], Value::String("match here".into()));
    assert_eq!(first["match"], Value::String("match".into()));
    assert_eq!(first["start"], Value::Number(0.0));
    assert_eq!(first["end"], Value::Number(5.0));
}

// ── Record projection and conversion ──────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn keys_and_values_project_a_record_in_insertion_order() {
    let source = || record(vec![("a", number(1.0)), ("b", number(2.0))]);
    assert_eq!(
        finish_call("keys", vec![source()]).await,
        strings(&["a", "b"])
    );
    assert_eq!(
        finish_call("values", vec![source()]).await,
        list_of(&[1.0, 2.0])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn to_string_to_int_and_to_float_convert_scalars() {
    assert_eq!(
        finish_call("to_string", vec![number(42.0)]).await,
        Value::String("42".into())
    );
    assert_eq!(
        finish_call("to_int", vec![string("7")]).await,
        Value::Number(7.0)
    );
    assert_eq!(
        finish_call("to_float", vec![string("3.5")]).await,
        Value::Number(3.5)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn json_parse_reads_a_json_document_into_values() {
    let value = finish_call("json_parse", vec![string("{\"a\": 1}")]).await;
    let record = value.as_record().expect("expected a record");
    assert_eq!(record["a"], Value::Number(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn format_fills_auto_numbered_indexed_and_escaped_placeholders() {
    assert_eq!(
        finish_call(
            "format",
            vec![string("hi {} you are {}"), string("sam"), number(3.0)]
        )
        .await,
        Value::String("hi sam you are 3".into())
    );
    assert_eq!(
        finish_call(
            "format",
            vec![string("{1} {0}"), string("world"), string("hello")]
        )
        .await,
        Value::String("hello world".into())
    );
    assert_eq!(
        finish_call("format", vec![string("{{ {} }}"), string("x")]).await,
        Value::String("{ x }".into())
    );
    // A placeholder-free template returns its literal text.
    assert_eq!(
        finish_call("format", vec![string("plain")]).await,
        Value::String("plain".into())
    );
    assert_eq!(
        finish_call(
            "format",
            vec![
                string("## {0}\n\n```json\n{{\"status\":\"{1}\",\"exit_code\":{2}}}\n```\n\n{3}"),
                string("cargo-machete"),
                string("timed_out"),
                number(124.0),
                string("The install command exceeded the timeout."),
            ]
        )
        .await,
        Value::String(
            "## cargo-machete\n\n```json\n{\"status\":\"timed_out\",\"exit_code\":124}\n```\n\nThe install command exceeded the timeout."
                .into()
        )
    );
}

fn type_literal(fields: Vec<(&str, TypeExpr)>) -> Expr {
    Expr::TypeLiteral(Box::new(TypeExpr::Object(
        fields
            .into_iter()
            .map(|(name, ty)| TypeField {
                name: name.into(),
                ty,
                optional: false,
            })
            .collect(),
    )))
}

#[tokio::test(flavor = "current_thread")]
async fn validate_checks_a_value_against_a_type_and_aborts_on_a_bad_shape() {
    let value = finish_call(
        "validate",
        vec![
            record(vec![
                ("name", string("lashlang")),
                ("labels", list(vec![string("agent"), string("runtime")])),
            ]),
            type_literal(vec![
                ("name", TypeExpr::Str),
                ("labels", TypeExpr::List(Box::new(TypeExpr::Str))),
            ]),
        ],
    )
    .await;
    let record_value = value.as_record().expect("validated record");
    assert_eq!(record_value["name"], Value::String("lashlang".into()));

    let error = run(finish_program(call(
        "validate",
        vec![
            record(vec![("labels", list(vec![string("agent"), number(42.0)]))]),
            type_literal(vec![("labels", TypeExpr::List(Box::new(TypeExpr::Str)))]),
        ],
    )))
    .await
    .expect_err("validate should abort on a bad shape");
    let RuntimeError::ValidationFailed { reason } = error else {
        panic!("expected validation failure: {error}");
    };
    assert!(
        reason.contains("$.labels[1]: expected string, got number"),
        "unexpected error: {reason}"
    );
}

// ── Numeric helpers ───────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn range_and_push_build_lists() {
    assert_eq!(
        finish_call("range", vec![number(3.0)]).await,
        list_of(&[0.0, 1.0, 2.0])
    );
    assert_eq!(
        finish_call("range", vec![number(-2.0), number(1.0)]).await,
        list_of(&[-2.0, -1.0, 0.0])
    );
    assert_eq!(
        finish_call("range", vec![number(3.0), number(3.0)]).await,
        Value::List(Vec::new().into())
    );
    assert_eq!(
        finish_call("range", vec![number(0.0), number(5.0), number(2.0)]).await,
        list_of(&[0.0, 2.0, 4.0])
    );
    assert_eq!(
        finish_call("range", vec![number(5.0), number(0.0), number(-2.0)]).await,
        list_of(&[5.0, 3.0, 1.0])
    );
    // A step pointing away from the bound yields an empty list.
    assert_eq!(
        finish_call("range", vec![number(5.0), number(0.0), number(2.0)]).await,
        Value::List(Vec::new().into())
    );
    assert_eq!(
        finish_call("range", vec![number(0.0), number(5.0), number(-2.0)]).await,
        Value::List(Vec::new().into())
    );

    // `push` returns the grown list; the original binding is unchanged.
    let value = run(program(vec![
        Expr::Assign {
            target: lashlang::AssignTarget::variable("base".into()),
            expr: Box::new(list(vec![string("a")])),
        },
        Expr::Assign {
            target: lashlang::AssignTarget::variable("extended".into()),
            expr: Box::new(call(
                "push",
                vec![Expr::Variable("base".into()), string("b")],
            )),
        },
        crate::ast_support::finish(record(vec![
            ("base", Expr::Variable("base".into())),
            ("extended", Expr::Variable("extended".into())),
        ])),
    ]))
    .await
    .expect("push should run");
    let record_value = value.as_record().expect("record");
    assert_eq!(record_value["base"], strings(&["a"]));
    assert_eq!(record_value["extended"], strings(&["a", "b"]));
}

#[tokio::test(flavor = "current_thread")]
async fn integer_division_helpers_support_chunk_math() {
    assert_eq!(
        finish_call("ceil_div", vec![number(10.0), number(3.0)]).await,
        Value::Number(4.0)
    );
    assert_eq!(
        finish_call("floor_div", vec![number(10.0), number(3.0)]).await,
        Value::Number(3.0)
    );
    assert_eq!(
        finish_call("ceil_div", vec![number(-10.0), number(3.0)]).await,
        Value::Number(-3.0)
    );
    assert_eq!(
        finish_call("floor_div", vec![number(-10.0), number(3.0)]).await,
        Value::Number(-4.0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shaping_builtins_order_reduce_and_rewrite_collections() {
    assert_eq!(
        finish_call("sort", vec![numbers([3.0, 1.0, 2.0])]).await,
        list_of(&[1.0, 2.0, 3.0])
    );
    assert_eq!(
        finish_call("sum", vec![numbers([1.0, 2.0, 3.0])]).await,
        Value::Number(6.0)
    );
    assert_eq!(
        finish_call("min", vec![numbers([3.0, 1.0, 2.0])]).await,
        Value::Number(1.0)
    );
    assert_eq!(
        finish_call("max", vec![numbers([3.0, 1.0, 2.0])]).await,
        Value::Number(3.0)
    );
    assert_eq!(
        finish_call("replace", vec![string("a-b-a"), string("a"), string("x")]).await,
        Value::String("x-b-x".into())
    );
    assert_eq!(
        finish_call("lower", vec![string("ABC")]).await,
        Value::String("abc".into())
    );
    assert_eq!(
        finish_call("upper", vec![string("abc")]).await,
        Value::String("ABC".into())
    );
    // Case mapping follows Unicode, not ASCII.
    assert_eq!(
        finish_call("lower", vec![string("Straße")]).await,
        Value::String("straße".into())
    );
    assert_eq!(
        finish_call("upper", vec![string("Straße")]).await,
        Value::String("STRASSE".into())
    );
    assert_eq!(
        finish_call(
            "unique",
            vec![list(vec![number(1.0), number(1.0), number(2.0)])]
        )
        .await,
        list_of(&[1.0, 2.0])
    );
    assert_eq!(
        finish_call("reverse", vec![list(vec![number(1.0), number(2.0)])]).await,
        list_of(&[2.0, 1.0])
    );

    let value = finish_call(
        "sort_by",
        vec![
            list(vec![
                record(vec![("id", string("first")), ("score", number(2.0))]),
                record(vec![("id", string("second")), ("score", number(1.0))]),
                record(vec![("id", string("third")), ("score", number(2.0))]),
            ]),
            string("score"),
        ],
    )
    .await;
    let Value::List(rows) = value else {
        panic!("expected sorted rows");
    };
    let ids = rows
        .iter()
        .map(|row| row.as_record().expect("row")["id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            Value::String("second".into()),
            Value::String("first".into()),
            Value::String("third".into()),
        ],
        "equal keys keep their written order"
    );

    // The sort key may be a dotted path into a nested record.
    let value = finish_call(
        "sort_by",
        vec![
            list(vec![
                record(vec![
                    ("id", string("first")),
                    ("profile", record(vec![("score", number(2.0))])),
                ]),
                record(vec![
                    ("id", string("second")),
                    ("profile", record(vec![("score", number(1.0))])),
                ]),
            ]),
            string("profile.score"),
        ],
    )
    .await;
    let Value::List(rows) = value else {
        panic!("expected sorted rows");
    };
    let ids = rows
        .iter()
        .map(|row| row.as_record().expect("row")["id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            Value::String("second".into()),
            Value::String("first".into())
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn extrema_over_an_empty_list_are_typed_runtime_errors() {
    for builtin in ["min", "max"] {
        let error = run(finish_program(call(builtin, vec![list(Vec::new())])))
            .await
            .expect_err("empty extrema must fail");
        assert!(
            matches!(&error, RuntimeError::ShapingEmptyList { builtin: actual } if actual == builtin),
            "{error:?}"
        );
    }
}

// ── The drift guard ───────────────────────────────────────────────────

/// The registry and this file's inventory must name exactly the same
/// builtins, and every one of them must run. Adding or removing a builtin
/// without touching this table fails here.
#[tokio::test(flavor = "current_thread")]
async fn every_registered_builtin_is_covered_and_runs() {
    let smoke: Vec<(&str, Vec<Expr>)> = vec![
        ("len", vec![string("a")]),
        ("empty", vec![string("")]),
        ("slice", vec![string("abc"), number(0.0), number(1.0)]),
        ("split", vec![string("a,b"), string(",")]),
        (
            "join",
            vec![list(vec![string("a"), string("b")]), string(",")],
        ),
        ("trim", vec![string(" a ")]),
        ("find", vec![string("abc"), string("b")]),
        ("grep_text", vec![string("abc"), string("b")]),
        ("starts_with", vec![string("abc"), string("a")]),
        ("ends_with", vec![string("abc"), string("c")]),
        ("contains", vec![string("abc"), string("b")]),
        ("keys", vec![record(vec![("a", number(1.0))])]),
        ("values", vec![record(vec![("a", number(1.0))])]),
        ("to_string", vec![number(1.0)]),
        ("to_int", vec![string("1")]),
        ("to_float", vec![string("1.5")]),
        ("json_parse", vec![string("1")]),
        ("format", vec![string("x")]),
        (
            "validate",
            vec![
                record(vec![("value", string("x"))]),
                type_literal(vec![("value", TypeExpr::Str)]),
            ],
        ),
        ("range", vec![number(1.0)]),
        ("ceil_div", vec![number(3.0), number(2.0)]),
        ("floor_div", vec![number(3.0), number(2.0)]),
        ("push", vec![list(Vec::new()), string("x")]),
        ("sort", vec![list(vec![number(2.0), number(1.0)])]),
        (
            "sort_by",
            vec![
                list(vec![
                    record(vec![("a", number(2.0))]),
                    record(vec![("a", number(1.0))]),
                ]),
                string("a"),
            ],
        ),
        ("sum", vec![list(vec![number(1.0), number(2.0)])]),
        ("min", vec![list(vec![number(1.0), number(2.0)])]),
        ("max", vec![list(vec![number(1.0), number(2.0)])]),
        ("replace", vec![string("aba"), string("a"), string("x")]),
        ("lower", vec![string("ABC")]),
        ("upper", vec![string("abc")]),
        ("unique", vec![list(vec![number(1.0), number(1.0)])]),
        ("reverse", vec![list(vec![number(1.0), number(2.0)])]),
    ];

    let mut smoke_names = smoke.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    smoke_names.sort_unstable();
    let mut registry_names = lashlang::builtin_names().collect::<Vec<_>>();
    registry_names.sort_unstable();
    assert_eq!(smoke_names, registry_names);

    for (name, args) in smoke {
        run(finish_program(call(name, args)))
            .await
            .unwrap_or_else(|error| panic!("builtin `{name}` failed to execute: {error}"));
    }
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
