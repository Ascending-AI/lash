use super::*;

#[tokio::test(flavor = "current_thread")]
async fn executes_if_for_and_list_concat() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        nums = [1, 2, 3, 4]
        sum = 0
        labels = []
        for n in nums {
          sum = sum + n
          labels = labels + [format("n={}", n)]
        }
        if sum == 10 {
          result = join(labels, ",")
        } else {
          result = "bad"
        }
        finish result
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("n=1,n=2,n=3,n=4".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn list_comprehension_builds_filtered_lists_without_clobbering_outer_bindings() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = "outer"
        doubled = [n * 2 for n in [1, 2, 3, 4] if n % 2 == 0]
        finish { doubled: doubled, n: n }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(
        record["doubled"],
        Value::List(vec![Value::Number(4.0), Value::Number(8.0)].into())
    );
    assert_eq!(record["n"], Value::String("outer".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn list_comprehension_nested_clauses_preserve_python_ordering() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        pairs = [format("{}:{}", a, b) for a in ["x", "y"] for b in range(0, 3) if b != 1]
        finish pairs
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("x:0".into()),
                Value::String("x:2".into()),
                Value::String("y:0".into()),
                Value::String("y:2".into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn list_comprehension_allows_effectful_iterables_filters_and_elements() {
    let host = TestHost::default()
        .with_file("Cargo.toml", "abc")
        .with_file("README.md", "");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        paths = ["Cargo.toml", "README.md"]
        sizes = [
          len(await files.read({ path: path })?)
          for path in paths
          if len(await files.read({ path: path })?) > 0
        ]
        finish sizes
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::List(vec![Value::Number(3.0)].into()));
}

#[tokio::test(flavor = "current_thread")]
async fn break_exits_loop_and_restores_loop_binding() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        item = "outer"
        seen = []
        for item in [1, 2, 3] {
          if item == 2 {
            break
          }
          seen = seen + [item]
        }
        finish { seen: seen, item: item }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(record["seen"], Value::List(vec![Value::Number(1.0)].into()));
    assert_eq!(record["item"], Value::String("outer".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn continue_skips_to_next_iteration() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        seen = []
        for n in [1, 2, 3, 4] {
          if n == 2 {
            continue
          }
          seen = seen + [n]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Number(1.0), Value::Number(3.0), Value::Number(4.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn while_loop_runs_until_condition_is_false() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = 0
        seen = []
        while n < 4 {
          seen = seen + [n]
          n = n + 1
        }
        finish { n: n, seen: seen }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("while loop should run"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(record["n"], Value::Number(4.0));
    assert_eq!(
        record["seen"],
        Value::List(
            vec![
                Value::Number(0.0),
                Value::Number(1.0),
                Value::Number(2.0),
                Value::Number(3.0),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_exits_while_loop() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = 0
        while true {
          n = n + 1
          if n == 3 {
            break
          }
        }
        finish n
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("break should exit while"),
    );

    assert_eq!(value, Value::Number(3.0));
}

#[tokio::test(flavor = "current_thread")]
async fn continue_skips_to_next_while_condition() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = 0
        seen = []
        while n < 5 {
          n = n + 1
          if n == 2 {
            continue
          }
          if n == 4 {
            continue
          }
          seen = seen + [n]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("continue should jump to while condition"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Number(1.0), Value::Number(3.0), Value::Number(5.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_loop_control_targets_nearest_loop() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        seen = []
        for outer in [1, 2] {
          for inner in [1, 2, 3] {
            if inner == 2 {
              continue
            }
            if inner == 3 {
              break
            }
            seen = seen + [format("{}:{}", outer, inner)]
          }
          seen = seen + [format("outer={}", outer)]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("1:1".to_string().into()),
                Value::String("outer=1".to_string().into()),
                Value::String("2:1".to_string().into()),
                Value::String("outer=2".to_string().into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_for_and_while_loop_control_targets_nearest_loop() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        seen = []
        for outer in [1, 2] {
          inner = 0
          while inner < 3 {
            inner = inner + 1
            if inner == 2 {
              continue
            }
            if inner == 3 {
              break
            }
            seen = seen + [format("{}:{}", outer, inner)]
          }
          seen = seen + [format("outer={}", outer)]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("nested for/while control should run"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("1:1".to_string().into()),
                Value::String("outer=1".to_string().into()),
                Value::String("2:1".to_string().into()),
                Value::String("outer=2".to_string().into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_inside_loop_still_terminates_program() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        for n in [1, 2, 3] {
          finish n
        }
        finish 99
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::Number(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_selects_the_correct_branch() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        truthy = true ? "left" : "right"
        falsy = false ? "left" : "right"
        finish format("{}:{}", truthy, falsy)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("left:right".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_is_right_associative() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        result = false ? 1 : true ? 2 : 3
        finish result
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_has_lower_precedence_than_boolean_ops() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        result = false or true ? "yes" : "no"
        finish result
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("yes".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_short_circuits_unselected_branch() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        yes = true ? "ok" : missing_name
        no = false ? missing_name : "ok"
        finish format("{}:{}", yes, no)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("ok:ok".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn unary_bang_aliases_not() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        a = !false
        b = !true
        finish [a, b]
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Bool(true), Value::Bool(false)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn symbolic_boolean_aliases_match_word_operators() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        a = true && false
        b = false || true
        c = !false && (false || true)
        finish [a, b, c]
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Bool(false), Value::Bool(true), Value::Bool(true)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn conditions_and_ternary_use_bounded_truthiness() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        a = 1 ? "yes" : "no"
        b = "" ? "yes" : "no"
        c = !0
        d = ![]
        finish [a, b, c, d]
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("yes".to_string().into()),
                Value::String("no".to_string().into()),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn string_concatenation_stringifies_non_string_side() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        found = await files.read({ path: "src/lib.rs" })
        finish "status=" + found.ok + " value=" + found.value
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String("status=true value=pub fn main() {}".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn arithmetic_and_string_builtins_coerce_scalars() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        total = true + 2
        scaled = "3" * 2
        joined = join(["a", 2, true], "-")
        split_num = split(101, 0)
        prefix = starts_with(123, 12)
        finish {
          total: total,
          scaled: scaled,
          joined: joined,
          split_num: split_num,
          prefix: prefix
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(record["total"], Value::Number(3.0));
    assert_eq!(record["scaled"], Value::Number(6.0));
    assert_eq!(
        record["joined"],
        Value::String("a-2-true".to_string().into())
    );
    assert_eq!(
        record["split_num"],
        Value::List(
            vec![
                Value::String("1".to_string().into()),
                Value::String("1".to_string().into())
            ]
            .into()
        )
    );
    assert_eq!(record["prefix"], Value::Bool(true));
}

#[tokio::test(flavor = "current_thread")]
async fn to_string_stringifies_records() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish to_string({ ok: true, count: 2 })
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String("{\"count\":2,\"ok\":true}".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn observe_captures_intermediate_values_without_ending_execution() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        item = { ok: true, count: 2 }
        print item
        print "step done"
        finish "final"
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("final".to_string().into()));
    let observed = host.observations.lock_recover();
    assert_eq!(observed.len(), 2);
    assert_eq!(
        observed[0],
        Value::Record({
            let mut record = Record::default();
            record.insert("ok".to_string(), Value::Bool(true));
            record.insert("count".to_string(), Value::Number(2.0));
            record.into()
        })
    );
    assert_eq!(observed[1], Value::String("step done".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn execution_can_continue_without_finish() {
    let host = TestHost::default();
    let mut state = State::new();

    let outcome = execute(
        r#"
        counter = 1
        print counter
        "#,
        &mut state,
        &host,
    )
    .await
    .expect("execution should succeed");

    assert_eq!(outcome, ExecutionOutcome::Continued);
    assert_eq!(state.globals()["counter"], Value::Number(1.0));
    let observed = host.observations.lock_recover();
    assert_eq!(observed.as_slice(), &[Value::Number(1.0)]);
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_fixes_tool_result_formatting_pattern() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        found = await files.read({ path: "src/lib.rs" })
        missing = await files.read({ path: "src/missing.rs" })
        summary = format(
          "found={} missing={}",
          found.ok ? "ok" : format("failed: {}", found.error),
          missing.ok ? "ok" : format("failed: {}", missing.error)
        )
        finish summary
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::String(text) = value else {
        panic!("expected string");
    };
    assert!(text.contains("found=ok"));
    assert!(text.contains("missing=failed:"));
}

#[tokio::test(flavor = "current_thread")]
async fn format_supports_indexed_reordering() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish format("b={1} a={0}", "x", "y")
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("b=y a=x".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn format_without_placeholders_returns_literal_string() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish format("plain")
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("plain".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn format_supports_escaped_braces() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish format("{{{}}}", 1)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("{1}".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn format_accepts_multiline_markdown_string_templates() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r####"
        finish format("""## Installed {0}

`{1}` is installed and available.

Tail:
{2}""", "cargo-machete", "cargo machete", "ok")
        "####,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String(
            "## Installed cargo-machete\n\n`cargo machete` is installed and available.\n\nTail:\nok"
                .to_string()
                .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_accepts_raw_markdown_templates_with_literal_braces() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r####"
        finish format(r"""## {0}

```json
{{"status":"{1}","ok":true}}
```

Output:
{2}""", "cargo-machete", "installed", "ready")
        "####,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String(
            "## cargo-machete\n\n```json\n{\"status\":\"installed\",\"ok\":true}\n```\n\nOutput:\nready"
                .to_string()
                .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_rejects_mixed_placeholder_styles_end_to_end() {
    let error = runtime_error(
        r#"
        finish format("{} {1}", "x", "y")
        "#,
    )
    .await;

    assert_eq!(
        error,
        RuntimeError::Format(lashlang::FormatError::MixedPlaceholderKinds)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_rejects_unused_args_end_to_end() {
    let error = runtime_error(
        r#"
        finish format("plain", 1)
        "#,
    )
    .await;

    assert_eq!(
        error,
        RuntimeError::Format(lashlang::FormatError::UnusedArgument { index: 0 })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_rejects_unmatched_braces_end_to_end() {
    let open_error = runtime_error(
        r#"
        finish format("{")
        "#,
    )
    .await;
    assert_eq!(
        open_error,
        RuntimeError::Format(lashlang::FormatError::UnmatchedOpenBrace)
    );

    let close_error = runtime_error(
        r#"
        finish format("}")
        "#,
    )
    .await;
    assert_eq!(
        close_error,
        RuntimeError::Format(lashlang::FormatError::UnmatchedCloseBrace)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tool_calls_return_result_records() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        found = await files.read({ path: "src/lib.rs" })
        missing = await files.read({ path: "src/missing.rs" })
        finish { found: found, missing: missing }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["found"].as_record().unwrap()["ok"],
        Value::Bool(true)
    );
    assert_eq!(
        record["missing"].as_record().unwrap()["ok"],
        Value::Bool(false)
    );
}
