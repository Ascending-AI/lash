// The value model and the numeric helpers, authored in TypeScript.
//
// ADR 0096 makes TypeScript the sole authored dialect, and the TypeScript front-end brings its
// own grammar, lexing and rejection suites
// (`crates/lash-typescript/tests/{grammar_coverage,ecma_regressions,rejections,
// test262_conformance}.rs`), so those rows are not re-spelled here — a TypeScript program
// cannot express the syntax they pinned, and re-authoring them would duplicate the front-end's
// own coverage.
//
// What remains are the facts that outlive the surface: the VM keeps string
// content byte-exact, the numeric helpers round the way the IR says they do,
// and a tuple is still an IR value with its own identity across a snapshot.
// The last two have no TypeScript spelling, so they are built from the AST.

use super::*;
use crate::ast_support::{call, finish, finish_program, list, number, program, string};

#[tokio::test(flavor = "current_thread")]
async fn executes_arithmetic_strings_and_finish() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const total = 1 + 2 * 3;
        const msg = "total=" + total;
        finish(msg);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("total=7".to_string().into()));
    assert_eq!(state.globals()["total"], Value::Number(7.0));
}

#[tokio::test(flavor = "current_thread")]
async fn executes_programs_with_comments() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        // Create some values first
        const total = 6 / 2; // divide
        /* and return the result */
        finish(total);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::Number(3.0));
}

#[tokio::test(flavor = "current_thread")]
async fn double_slash_inside_strings_is_not_a_comment() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const url = "https://example.com/a//b";
        finish(url);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String("https://example.com/a//b".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn strings_preserve_utf8_content() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish("Grüße 東京");
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("Grüße 東京".into()));
}

/// Shell-shaped text is the payload these cells carry most often, and every
/// character of it has to survive the front-end and the VM untouched.
#[tokio::test(flavor = "current_thread")]
async fn string_values_preserve_shell_quotes_and_escapes() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish([
          { cmd: "date '+%Y-%m-%d %H:%M:%S %Z (%z)'" }.cmd,
          'printf "%s\\n" "$value"',
          "json: {\"cmd\":\"echo 'ok'\"}",
          "// not a comment # also not a comment",
          "${HOME:-/tmp} && echo %done",
          "C:\\Users\\sam\\file.txt",
          "python3 - <<'PY'\nprint(\"ok\")\nPY"
        ]);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("date '+%Y-%m-%d %H:%M:%S %Z (%z)'".into()),
                Value::String("printf \"%s\\n\" \"$value\"".into()),
                Value::String("json: {\"cmd\":\"echo 'ok'\"}".into()),
                Value::String("// not a comment # also not a comment".into()),
                Value::String("${HOME:-/tmp} && echo %done".into()),
                Value::String("C:\\Users\\sam\\file.txt".into()),
                Value::String("python3 - <<'PY'\nprint(\"ok\")\nPY".into()),
            ]
            .into()
        )
    );
}

/// `range` is an IR builtin with no TypeScript spelling (ADR 0096) — a
/// TypeScript author writes a counting loop — but the VM still implements it,
/// and its Python-style step rules are what this row pins. The programs are
/// therefore built from the AST rather than authored.
#[tokio::test(flavor = "current_thread")]
async fn range_supports_python_style_steps() {
    for (args, expected) in [
        (
            vec![number(0.0), number(5.0), number(2.0)],
            vec![0.0, 2.0, 4.0],
        ),
        (
            vec![number(5.0), number(0.0), number(-2.0)],
            vec![5.0, 3.0, 1.0],
        ),
        (vec![number(5.0), number(0.0), number(2.0)], Vec::new()),
        (vec![number(0.0), number(5.0), number(-2.0)], Vec::new()),
    ] {
        let host = TestHost::default();
        let mut state = State::new();
        let value = finished(
            lashlang::execute(&finish_program(call("range", args)), &mut state, &host)
                .await
                .expect("range should run"),
        );
        assert_eq!(
            value,
            Value::List(
                expected
                    .into_iter()
                    .map(Value::Number)
                    .collect::<Vec<_>>()
                    .into()
            )
        );
    }
}

/// `ceil_div` and `floor_div` round toward positive and negative infinity
/// respectively — mathematical rounding, not truncation. Another IR fact with
/// no TypeScript spelling.
#[tokio::test(flavor = "current_thread")]
async fn integer_division_helpers_use_mathematical_rounding() {
    for (name, left, right, expected) in [
        ("ceil_div", 10.0, 3.0, 4.0),
        ("floor_div", 10.0, 3.0, 3.0),
        ("ceil_div", -10.0, 3.0, -3.0),
        ("floor_div", -10.0, 3.0, -4.0),
    ] {
        let host = TestHost::default();
        let mut state = State::new();
        let program = finish_program(call(name, vec![number(left), number(right)]));
        let value = finished(
            lashlang::execute(&program, &mut state, &host)
                .await
                .expect("division helper should run"),
        );
        assert_eq!(value, Value::Number(expected), "{name}({left}, {right})");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn numeric_helper_errors_are_rejected() {
    for (name, args) in [
        ("range", vec![number(0.0), number(5.0), number(0.0)]),
        ("range", vec![number(0.0), number(5.0), number(1.5)]),
        (
            "range",
            vec![number(1_000_001.0), number(0.0), number(-1.0)],
        ),
        ("ceil_div", vec![number(1.5), number(1.0)]),
        ("floor_div", vec![number(1.0), number(0.0)]),
        ("ceil_div", vec![string("1"), number(1.0)]),
    ] {
        let host = TestHost::default();
        let mut state = State::new();
        let program = finish_program(call(name, args));
        let err = lashlang::execute(&program, &mut state, &host)
            .await
            .expect_err("numeric helper should reject");
        assert!(
            matches!(
                err,
                RuntimeError::ZeroRangeStep
                    | RuntimeError::InvalidRangeBound
                    | RuntimeError::InvalidRangeBoundType { .. }
                    | RuntimeError::RangeTooLarge { .. }
                    | RuntimeError::InvalidIntegerDivisionArgument { .. }
                    | RuntimeError::InvalidIntegerDivisionArgumentType { .. }
                    | RuntimeError::IntegerDivisionByZero { .. }
            ),
            "{name}: {err:?}"
        );
    }
}

/// A tuple is an IR value the retired surface spelled `(a, b)`; TypeScript has
/// no tuple literal (ADR 0096), so the value's own rules — immutable, not a
/// list, distinct from a list under equality — are pinned against the AST.
#[tokio::test(flavor = "current_thread")]
async fn a_tuple_is_an_immutable_value_distinct_from_a_list() {
    let pair = || lashlang::Expr::Tuple(vec![number(1.0), string("x")]);
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        lashlang::execute(
            &program(vec![finish(lashlang::Expr::Record(vec![
                ("len_pair".into(), call("len", vec![pair()])),
                (
                    "contains_x".into(),
                    call("contains", vec![pair(), string("x")]),
                ),
                ("empty_pair".into(), call("empty", vec![pair()])),
                (
                    "tuple_eq".into(),
                    lashlang::Expr::Binary {
                        op: lashlang::BinaryOp::Equal,
                        left: Box::new(pair()),
                        right: Box::new(pair()),
                    },
                ),
                (
                    "tuple_not_list".into(),
                    lashlang::Expr::Binary {
                        op: lashlang::BinaryOp::Equal,
                        left: Box::new(pair()),
                        right: Box::new(list(vec![number(1.0), string("x")])),
                    },
                ),
                ("text".into(), call("to_string", vec![pair()])),
            ]))]),
            &mut state,
            &host,
        )
        .await
        .expect("tuple program should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(record["len_pair"], Value::Number(2.0));
    assert_eq!(record["contains_x"], Value::Bool(true));
    assert_eq!(record["empty_pair"], Value::Bool(false));
    assert_eq!(record["tuple_eq"], Value::Bool(true));
    assert_eq!(record["tuple_not_list"], Value::Bool(false));
    assert_eq!(record["text"], Value::String(r#"(1, "x")"#.into()));

    let host = TestHost::default();
    let mut state = State::new();
    let err = lashlang::execute(
        &finish_program(call("push", vec![pair(), number(3.0)])),
        &mut state,
        &host,
    )
    .await
    .expect_err("a tuple is not a list");
    assert!(
        err.to_string()
            .contains("`push` requires a list as the first argument"),
        "{err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tuple_snapshot_round_trip_preserves_tuple_identity() {
    let host = TestHost::default();
    let mut state = State::new();
    let outcome = lashlang::execute(
        &program(vec![lashlang::Expr::Assign {
            target: lashlang::AssignTarget::variable("pair".into()),
            expr: Box::new(lashlang::Expr::Tuple(vec![number(1.0), number(2.0)])),
        }]),
        &mut state,
        &host,
    )
    .await
    .expect("tuple assignment should run");
    assert!(matches!(outcome, ExecutionOutcome::Continued));

    let encoded = state
        .snapshot()
        .to_canonical_bytes()
        .expect("snapshot encode");
    let snapshot = lashlang::Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
    let restored = State::from_snapshot(snapshot);
    assert!(matches!(restored.globals()["pair"], Value::Tuple(_)));
}
