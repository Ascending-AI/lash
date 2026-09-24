// Control flow, operators and the host-call surface, authored in TypeScript.
//
// ADR 0096 makes TypeScript the sole authored RLM dialect, so every program
// here is TypeScript lowered through `lash_typescript`. What the file pins is
// unchanged: the VM's loops, loop control, conditional evaluation, truthiness,
// coercion and tool-call results. Where a construct had no TypeScript spelling
// the fact it pinned is either re-pinned to its ECMA equivalent or, when it was
// a property of the IR rather than of any dialect, built straight from the AST.

use super::*;

#[tokio::test(flavor = "current_thread")]
async fn executes_if_for_and_list_concat() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const nums = [1, 2, 3, 4];
        let sum = 0;
        let labels = [];
        for (const n of nums) {
          sum = sum + n;
          labels = labels.concat(["n=" + n]);
        }
        let result = "bad";
        if (sum === 10) {
          result = labels.join(",");
        }
        finish(result);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("n=1,n=2,n=3,n=4".to_string().into()));
}

/// The list comprehension's replacement: `filter` then `map`. The fact the
/// comprehension test pinned — that the element binding never leaks into the
/// enclosing scope — is an ECMA fact about callback parameters, so it is
/// re-pinned rather than dropped.
#[tokio::test(flavor = "current_thread")]
async fn filter_and_map_build_lists_without_clobbering_outer_bindings() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const n = "outer";
        const doubled = [1, 2, 3, 4].filter((n) => n % 2 === 0).map((n) => n * 2);
        finish({ doubled: doubled, n: n });
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

/// The nested comprehension's replacement: nested iteration. The ordering it
/// pinned — outer clause slowest, filter applied per inner element — is the
/// ordering nested `for...of` produces.
#[tokio::test(flavor = "current_thread")]
async fn nested_iteration_preserves_written_ordering() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        let pairs = [];
        for (const a of ["x", "y"]) {
          for (const b of [0, 1, 2]) {
            if (b !== 1) {
              pairs = pairs.concat([a + ":" + b]);
            }
          }
        }
        finish(pairs);
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

/// Host calls in both the filter and the element position of a loop that
/// builds a list, which is what the effectful comprehension pinned.
#[tokio::test(flavor = "current_thread")]
async fn a_loop_may_await_host_calls_in_its_filter_and_its_element() {
    let host = TestHost::default()
        .with_file("Cargo.toml", "abc")
        .with_file("README.md", "");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const paths = ["Cargo.toml", "README.md"];
        let sizes = [];
        for (const path of paths) {
          if ((await files.read({ path: path })).length > 0) {
            sizes = sizes.concat([(await files.read({ path: path })).length]);
          }
        }
        finish(sizes);
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
async fn break_exits_loop_and_leaves_the_outer_binding_alone() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const item = "outer";
        let seen = [];
        for (const entry of [1, 2, 3]) {
          if (entry === 2) {
            break;
          }
          seen = seen.concat([entry]);
        }
        finish({ seen: seen, item: item });
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
        let seen = [];
        for (const n of [1, 2, 3, 4]) {
          if (n === 2) {
            continue;
          }
          seen = seen.concat([n]);
        }
        finish(seen);
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
        let n = 0;
        let seen = [];
        while (n < 4) {
          seen = seen.concat([n]);
          n = n + 1;
        }
        finish({ n: n, seen: seen });
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
        let n = 0;
        while (true) {
          n = n + 1;
          if (n === 3) {
            break;
          }
        }
        finish(n);
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
        let n = 0;
        let seen = [];
        while (n < 5) {
          n = n + 1;
          if (n === 2) {
            continue;
          }
          if (n === 4) {
            continue;
          }
          seen = seen.concat([n]);
        }
        finish(seen);
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
        let seen = [];
        for (const outer of [1, 2]) {
          for (const inner of [1, 2, 3]) {
            if (inner === 2) {
              continue;
            }
            if (inner === 3) {
              break;
            }
            seen = seen.concat([outer + ":" + inner]);
          }
          seen = seen.concat(["outer=" + outer]);
        }
        finish(seen);
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
        let seen = [];
        for (const outer of [1, 2]) {
          let inner = 0;
          while (inner < 3) {
            inner = inner + 1;
            if (inner === 2) {
              continue;
            }
            if (inner === 3) {
              break;
            }
            seen = seen.concat([outer + ":" + inner]);
          }
          seen = seen.concat(["outer=" + outer]);
        }
        finish(seen);
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
        for (const n of [1, 2, 3]) {
          finish(n);
        }
        finish(99);
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
        const truthy = true ? "left" : "right";
        const falsy = false ? "left" : "right";
        finish(truthy + ":" + falsy);
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
        const result = false ? 1 : true ? 2 : 3;
        finish(result);
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
        const result = false || true ? "yes" : "no";
        finish(result);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("yes".to_string().into()));
}

/// The unselected branch is never evaluated, so a call the VM would refuse
/// sits harmlessly in the arm that is not taken.
#[tokio::test(flavor = "current_thread")]
async fn ternary_short_circuits_unselected_branch() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const yes = true ? "ok" : JSON.parse("{");
        const no = false ? JSON.parse("{") : "ok";
        finish(yes + ":" + no);
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
async fn boolean_operators_evaluate_as_ecma_does() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const a = true && false;
        const b = false || true;
        const c = !false && (false || true);
        const d = !true;
        finish([a, b, c, d]);
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
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false)
            ]
            .into()
        )
    );
}

/// Truthiness in conditions and in `!`. The dialect's old "bounded truthiness"
/// agreed with ECMA on every one of these rows, so the row set is unchanged and
/// the claim is simply re-pinned to ECMA: `0` and `""` are falsy, and an empty
/// array is truthy.
#[tokio::test(flavor = "current_thread")]
async fn conditions_and_ternary_use_ecma_truthiness() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const a = 1 ? "yes" : "no";
        const b = "" ? "yes" : "no";
        const c = !0;
        const d = ![];
        finish([a, b, c, d]);
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
        const found = await files.read({ path: "src/lib.rs" });
        finish("status=" + true + " value=" + found);
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

/// Scalar coercion in arithmetic and in the string standard library. The
/// dialect's `join`/`split`/`starts_with` builtins have no TypeScript spelling;
/// their ECMA counterparts are instance methods, and they coerce the same way.
#[tokio::test(flavor = "current_thread")]
async fn arithmetic_and_string_methods_coerce_scalars() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const total = true + 2;
        const scaled = "3" * 2;
        const joined = ["a", 2, true].join("-");
        const split_num = "101".split("0");
        const prefix = "123".startsWith("12");
        finish({
          total: total,
          scaled: scaled,
          joined: joined,
          split_num: split_num,
          prefix: prefix
        });
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

/// The dialect's `to_string` sorted a record's keys; `JSON.stringify` keeps
/// insertion order (ADR 0096), so the claim is re-pinned to the ECMA order
/// rather than to the retired builtin's.
#[tokio::test(flavor = "current_thread")]
async fn json_stringify_serialises_objects_in_insertion_order() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish(JSON.stringify({ ok: true, count: 2 }));
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String("{\"ok\":true,\"count\":2}".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn print_captures_intermediate_values_without_ending_execution() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const item = { ok: true, count: 2 };
        print(item);
        print("step done");
        finish("final");
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
        const counter = 1;
        print(counter);
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

/// A failed host call is a thrown error rather than an `{ ok, error }` record
/// (ADR 0096), so the "summarise both outcomes" pattern is written with
/// `try`/`catch`. What it pins is unchanged: both branches of the summary are
/// reachable in one program, and the failure carries the host's own text.
#[tokio::test(flavor = "current_thread")]
async fn a_summary_can_report_both_a_successful_and_a_failed_host_call() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        let found = "";
        try {
          await files.read({ path: "src/lib.rs" });
          found = "ok";
        } catch (error) {
          found = "failed: " + error.message;
        }
        let missing = "";
        try {
          await files.read({ path: "src/missing.rs" });
          missing = "ok";
        } catch (error) {
          missing = "failed: " + error.message;
        }
        finish("found=" + found + " missing=" + missing);
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
    assert!(text.contains("found=ok"), "{text}");
    assert!(text.contains("missing=failed:"), "{text}");
}

/// A tool call yields the host's value directly and throws on failure, which is
/// the replacement for the dialect's `{ ok, value }` result record (ADR 0096).
#[tokio::test(flavor = "current_thread")]
async fn tool_calls_return_values_and_throw_on_failure() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const found = await files.read({ path: "src/lib.rs" });
        let missing = null;
        try {
          missing = await files.read({ path: "src/missing.rs" });
        } catch (error) {
          missing = error instanceof Error;
        }
        finish({ found: found, missing: missing });
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
    assert_eq!(record["found"], Value::String("pub fn main() {}".into()));
    assert_eq!(record["missing"], Value::Bool(true));
}
