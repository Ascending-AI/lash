// The value model and the numeric helpers, authored in TypeScript.
//
// ADR 0096 makes TypeScript the sole authored dialect, and the TypeScript front-end brings its
// own grammar, lexing and rejection suites
// (`crates/lash-typescript/tests/{grammar_coverage,ecma_regressions,rejections,
// test262_sample,test262_full}.rs`), so those rows are not re-spelled here — a TypeScript program
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

/// Quote- and escape-heavy text — command lines, embedded JSON, paths — has to
/// survive the front-end and the VM untouched, character for character.
#[tokio::test(flavor = "current_thread")]
async fn string_values_preserve_quotes_and_escapes() {
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

/// A tuple is an IR value the retired surface spelled `(a, b)`; TypeScript has
/// no tuple literal (ADR 0096), so the value's own rules — immutable, not a
/// list, distinct from a list under equality — are pinned against a seeded
/// runtime value.
#[tokio::test(flavor = "current_thread")]
async fn a_tuple_is_an_immutable_value_distinct_from_a_list() {
    // Nothing spells a tuple in TypeScript, so the value is seeded as a global
    // the way a restored snapshot would carry one.
    let pair = || lashlang::Expr::Variable("pair".into());
    let host = TestHost::default();
    let mut state = State::new();
    state
        .insert_global(
            "pair",
            Value::Tuple(vec![Value::Number(1.0), Value::String("x".into())].into()),
        )
        .expect("tuple global seeds");
    let value = finished(
        lashlang::execute(
            &lashlang_compile_program(&program(vec![finish(lashlang::Expr::Record(vec![
                ("len_pair".into(), call("len", vec![pair()])),
                (
                    "contains_x".into(),
                    call("contains", vec![pair(), string("x")]),
                ),
                ("empty_pair".into(), call("empty", vec![pair()])),
                (
                    "tuple_eq".into(),
                    lashlang::Expr::JavaScriptBinary {
                        op: lashlang::JavaScriptBinaryOp::StrictEqual,
                        left: Box::new(pair()),
                        right: Box::new(pair()),
                    },
                ),
                (
                    "tuple_not_list".into(),
                    lashlang::Expr::JavaScriptBinary {
                        op: lashlang::JavaScriptBinaryOp::StrictEqual,
                        left: Box::new(pair()),
                        right: Box::new(list(vec![number(1.0), string("x")])),
                    },
                ),
                ("text".into(), call("to_string", vec![pair()])),
            ]))]))
            .expect("the program compiles"),
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
    state
        .insert_global(
            "pair",
            Value::Tuple(vec![Value::Number(1.0), Value::String("x".into())].into()),
        )
        .expect("tuple global seeds");
    let err = lashlang::execute(
        &lashlang_compile_program(&finish_program(call("push", vec![pair(), number(3.0)])))
            .expect("the program compiles"),
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
    // A stored snapshot can still carry a tuple from before the surface
    // spelled one; the wire round trip must keep its identity.
    let snapshot = lashlang::Snapshot::new(
        [(
            "pair".to_string(),
            Value::Tuple(vec![Value::Number(1.0), Value::Number(2.0)].into()),
        )]
        .into_iter()
        .collect(),
    );
    let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
    let snapshot = lashlang::Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
    let restored = State::from_snapshot(snapshot);
    assert!(matches!(restored.globals()["pair"], Value::Tuple(_)));
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
