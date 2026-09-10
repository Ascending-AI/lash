use super::*;

#[tokio::test(flavor = "current_thread")]
async fn parser_handles_precedence_and_await_record() {
    let program = parse(
        r#"
        total = 1 + 2 * 3
        process read(pattern: str, path: str) {
          finish { pattern: pattern, path: path }
        }
        fanout = await {
          left: start read(pattern: "src/*.rs", path: ""),
          right: start read(pattern: "", path: "src/lib.rs")
        }
        finish total
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_double_slash_comments() {
    let program = parse(
        r#"
        // setup
        total = 1 + 2
        // finish
        finish total
        "#,
    )
    .expect("program should parse");

    let lashlang::Expr::Block(expressions) = &program.main else {
        panic!("program should be a block");
    };
    assert_eq!(expressions.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_semicolons_as_statement_separators() {
    let program = parse(
        r#"
        x = 1; y = 2;
        finish x;
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn start_is_contextual_not_reserved() {
    let host = TestHost::default().with_file("a.txt", "async");
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            process read_file(path: str) { finish path }
            start = 1
            for start in range(3) {
              last = start
            }
            rec = { start: last }
            h = start read_file(path: "a.txt")
            finish { value: start, field: rec.start, awaited: (await h)? }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("contextual start program should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(record["value"], Value::Number(1.0));
    assert_eq!(record["field"], Value::Number(2.0));
    assert_eq!(record["awaited"], Value::String("async".to_string().into()));

    let err = runtime_error("finish start()").await;
    assert!(matches!(err, RuntimeError::UnknownBuiltin { name } if name == "start"));
}

#[tokio::test(flavor = "current_thread")]
async fn range_supports_python_style_steps() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            stepped = []
            for i in range(5, 0, -2) {
              stepped = push(stepped, i)
            }
            finish {
              up: range(0, 5, 2),
              down: range(5, 0, -2),
              empty_up: range(5, 0, 2),
              empty_down: range(0, 5, -2),
              iterated: stepped
            }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("stepped ranges should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(
        record["up"],
        Value::List(vec![Value::Number(0.0), Value::Number(2.0), Value::Number(4.0)].into())
    );
    assert_eq!(
        record["down"],
        Value::List(vec![Value::Number(5.0), Value::Number(3.0), Value::Number(1.0)].into())
    );
    assert_eq!(record["empty_up"], Value::List(Vec::new().into()));
    assert_eq!(record["empty_down"], Value::List(Vec::new().into()));
    assert_eq!(record["iterated"], record["down"]);
}

#[tokio::test(flavor = "current_thread")]
async fn integer_division_helpers_use_mathematical_rounding() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            items = range(10)
            stride = ceil_div(len(items), 3)
            starts = []
            for i in range(0, len(items), stride) {
              starts = push(starts, i)
            }
            finish {
              ceil_pos: ceil_div(10, 3),
              floor_pos: floor_div(10, 3),
              ceil_neg: ceil_div(-10, 3),
              floor_neg: floor_div(-10, 3),
              starts: starts
            }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("division helpers should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(record["ceil_pos"], Value::Number(4.0));
    assert_eq!(record["floor_pos"], Value::Number(3.0));
    assert_eq!(record["ceil_neg"], Value::Number(-3.0));
    assert_eq!(record["floor_neg"], Value::Number(-4.0));
    assert_eq!(
        record["starts"],
        Value::List(vec![Value::Number(0.0), Value::Number(4.0), Value::Number(8.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn numeric_helper_errors_are_rejected() {
    for source in [
        "finish range(0, 5, 0)",
        "finish range(0, 5, 1.5)",
        "finish range(1000001, 0, -1)",
        "finish ceil_div(1.5, 1)",
        "finish floor_div(1, 0)",
        "finish ceil_div(\"1\", 1)",
    ] {
        let err = runtime_error(source).await;
        assert!(matches!(
            err,
            RuntimeError::ZeroRangeStep
                | RuntimeError::InvalidRangeBound
                | RuntimeError::InvalidRangeBoundType { .. }
                | RuntimeError::RangeTooLarge { .. }
                | RuntimeError::InvalidIntegerDivisionArgument { .. }
                | RuntimeError::InvalidIntegerDivisionArgumentType { .. }
                | RuntimeError::IntegerDivisionByZero { .. }
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_trailing_semicolon_after_raw_string() {
    let program = parse(
        r#"
        msg = r"hello";
        finish msg
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_treats_semicolon_like_whitespace_between_idents() {
    let with_semi = parse("x = 1;y = 2").expect("semicolon-separated should parse");
    let with_newline = parse("x = 1\ny = 2").expect("newline-separated should parse");
    assert_eq!(program_len(&with_semi), program_len(&with_newline));
    assert_eq!(program_len(&with_semi), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn tuple_comma_expressions_are_first_class_sequence_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            pair = 1, "x"
            singleton = (1,)
            empty_tuple = ()
            sliced = slice((1, 2, 3), 1, 3)
            seen = []
            for item in pair {
              seen = push(seen, to_string(item))
            }
            print pair
            finish {
              first: pair[0],
              second: pair[1],
              len_pair: len(pair),
              len_singleton: len(singleton),
              empty_empty: empty(empty_tuple),
              empty_pair: empty(pair),
              contains_x: contains(pair, "x"),
              joined: join(("a", "b"), "|"),
              slice_first: sliced[0],
              slice_len: len(sliced),
              truthy_pair: pair ? true : false,
              truthy_empty: empty_tuple ? true : false,
              concat_last: ((1,) + (2,))[1],
              seen: seen,
              tuple_eq: (1, 2) == (1, 2),
              tuple_not_list: (1, 2) == [1, 2],
              text: to_string((1, "x"))
            }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("tuple program should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(record["first"], Value::Number(1.0));
    assert_eq!(record["second"], Value::String("x".into()));
    assert_eq!(record["len_pair"], Value::Number(2.0));
    assert_eq!(record["len_singleton"], Value::Number(1.0));
    assert_eq!(record["empty_empty"], Value::Bool(true));
    assert_eq!(record["empty_pair"], Value::Bool(false));
    assert_eq!(record["contains_x"], Value::Bool(true));
    assert_eq!(record["joined"], Value::String("a|b".into()));
    assert_eq!(record["slice_first"], Value::Number(2.0));
    assert_eq!(record["slice_len"], Value::Number(2.0));
    assert_eq!(record["truthy_pair"], Value::Bool(true));
    assert_eq!(record["truthy_empty"], Value::Bool(false));
    assert_eq!(record["concat_last"], Value::Number(2.0));
    assert_eq!(
        record["seen"],
        Value::List(vec![Value::String("1".into()), Value::String("x".into())].into())
    );
    assert_eq!(record["tuple_eq"], Value::Bool(true));
    assert_eq!(record["tuple_not_list"], Value::Bool(false));
    assert_eq!(record["text"], Value::String(r#"(1, "x")"#.into()));

    let observations = host.observations.lock_recover();
    assert!(matches!(&observations[0], Value::Tuple(items) if items.len() == 2));
}

#[tokio::test(flavor = "current_thread")]
async fn tuples_are_immutable_and_do_not_mixed_concat_with_lists() {
    let err = runtime_error(
        r#"
        pair = (1, 2)
        pair[0] = 9
        finish pair
        "#,
    )
    .await;
    assert!(err.to_string().contains("tuples are immutable"), "{err:?}");

    let err = runtime_error("finish push((1, 2), 3)").await;
    assert!(
        err.to_string()
            .contains("`push` requires a list as the first argument"),
        "{err:?}"
    );

    let err = runtime_error("finish (1,) + [2]").await;
    assert!(
        err.to_string().contains("can't concatenate list and tuple"),
        "{err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tuple_snapshot_round_trip_preserves_tuple_identity() {
    let host = TestHost::default();
    let mut state = State::new();
    let outcome = execute("pair = 1, 2", &mut state, &host)
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

#[tokio::test(flavor = "current_thread")]
async fn multiline_strings_are_expression_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r####"
            finish """first\n"quoted"
second"""
            "####,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("first\n\"quoted\"\nsecond".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn single_quoted_strings_are_expression_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish 'it\'s ready\n'
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("it's ready\n".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn triple_single_strings_are_expression_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish '''first\n'second'
third'''
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("first\n'second'\nthird".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn raw_single_and_double_strings_preserve_backslashes() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish [r"path\to\file", R'\n stays raw']
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
                Value::String("path\\to\\file".into()),
                Value::String("\\n stays raw".into())
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_exact_shell_exec_date_command_string() {
    let program = parse(
        r#"
        now = await shell.exec({ cmd: "date '+%Y-%m-%d %H:%M:%S %Z (%z)'" })?
        finish now
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn record_strings_preserve_shell_date_command_text() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish { cmd: "date '+%Y-%m-%d %H:%M:%S %Z (%z)'" }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["cmd"],
        Value::String("date '+%Y-%m-%d %H:%M:%S %Z (%z)'".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn string_literals_cover_shell_quotes_formats_and_raw_forms() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#####"
            finish [
              "date '+%Y-%m-%d %H:%M:%S %Z (%z)'",
              'printf "%s\\n" "$value"',
              "json: {\"cmd\":\"echo 'ok'\"}",
              "// not a comment # also not a comment",
              "${HOME:-/tmp} && echo %done",
              r"C:\Users\sam\file.txt",
              R'\n stays slash-n',
              """line "quoted" and 'single'
next""",
              '''line "double" and 'single'
next''',
              r"""python3 - <<'PY'
print("ok")
PY"""
            ]
            "#####,
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
                Value::String("\\n stays slash-n".into()),
                Value::String("line \"quoted\" and 'single'\nnext".into()),
                Value::String("line \"double\" and 'single'\nnext".into()),
                Value::String("python3 - <<'PY'\nprint(\"ok\")\nPY".into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn strings_preserve_utf8_content() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish "Grüße 東京"
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("Grüße 東京".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn raw_triple_strings_preserve_patch_text() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r####"
            patch = r"""*** Begin Patch
*** Update File: src/lib.rs
@@
-old
+new
\n { braces stay raw }
*** End Patch"""
            finish patch
            "####,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(
        value,
        Value::String(
            "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n\\n { braces stay raw }\n*** End Patch"
                .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn raw_triple_strings_preserve_script_text() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#####"
            script = r'''python3 - <<'PY'
print("""hello""")
\n { braces stay raw }
PY'''
            finish script
            "#####,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(
        value,
        Value::String(
            "python3 - <<'PY'\nprint(\"\"\"hello\"\"\")\n\\n { braces stay raw }\nPY".into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rust_style_raw_strings_are_not_valid_lashlang_strings() {
    let err = runtime_error(r####"finish r#"hello"#"####).await;
    assert!(
        format!("{err}").contains("unknown name `r`"),
        "old raw syntax should not lex as a string, got {err}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_comment_only_program() {
    let program = parse(
        r#"
        // comment one
        // comment two
        "#,
    )
    .expect("program should parse");

    assert!(program_len(&program) == 0);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_inline_trailing_comments_in_blocks() {
    let program = parse(
        r#"
        if true { // enter block
          value = 1 // assign
        } else { // fallback
          value = 2
        }
        finish value // done
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_else_if_chains() {
    let program = parse(
        r#"
        if false {
          answer = 1
        } else if true {
          answer = 2
        } else {
          answer = 3
        }
        finish answer
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_allows_await_record_in_expression_position() {
    let program = parse(
        r#"
        results = await {
          left: start glob(pattern: "src/*.rs"),
          right: start read_file(path: "src/lib.rs")
        }
        finish results
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_allows_bare_expression_statements() {
    let program = parse(
        r#"
        "branch_a"
        finish "done"
        "#,
    )
    .expect("program should parse");

    let lashlang::Expr::Block(expressions) = &program.main else {
        panic!("program should be a block");
    };
    assert_eq!(expressions.len(), 2);
    assert!(matches!(expressions[0], lashlang::Expr::String(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn parser_allows_finish_null_at_the_end_of_a_block_or_program() {
    let program = parse(
        r#"
        if true {
          finish null
        }
        finish null
        "#,
    )
    .expect("program should parse");

    let lashlang::Expr::Block(expressions) = &program.main else {
        panic!("program should be a block");
    };
    assert!(matches!(
        expressions.as_slice(),
        [
            lashlang::Expr::If { then_block, .. },
            lashlang::Expr::Finish(_)
        ] if matches!(then_block.as_ref(), lashlang::Expr::Block(items) if matches!(items.as_slice(), [lashlang::Expr::Finish(_)]))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn executes_programs_with_double_slash_comments() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        // Create some values first
        total = 6 / 2
        // Return the result
        finish total
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
async fn bare_finish_requires_value() {
    let host = TestHost::default();
    let mut state = State::new();

    let err = execute("finish", &mut state, &host)
        .await
        .expect_err("bare finish should be rejected");
    assert_eq!(
        err.to_string(),
        "`finish` requires a value; use `finish null` to finish with null"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn executes_inline_trailing_comments_inside_blocks() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        if true { // choose this branch
          total = 1 + 2 // add
        } else {
          total = 0
        }
        finish total // final answer
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
        url = "https://example.com/a//b"
        finish url
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
async fn parser_accepts_ternary_in_call_arguments() {
    let program = parse(
        r#"
        result = format("{}", true ? "yes" : "no")
        finish result
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn executes_arithmetic_strings_and_finish() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        total = 1 + 2 * 3
        msg = format("total={}", total)
        finish msg
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
