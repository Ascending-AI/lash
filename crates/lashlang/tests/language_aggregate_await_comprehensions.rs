// Aggregate await, authored in TypeScript (FIG-2764, ADR 0096).
//
// The dialect spelled the fan-out shape `await [op(x)? for x in xs]`; the
// TypeScript spelling is `await Promise.all(xs.map((x) => op(x)))`. What the
// file pins is unchanged: every call in the aggregate starts before any of
// them is awaited, calls start in written order, the sequential shape stays
// sequential, and a failing leaf fails the aggregate.
//
// The wrapped form has no direct TypeScript spelling — a tool call yields the
// host's value and throws on failure instead of an `{ ok, value }` record — so
// the per-item-errors row is re-pinned to `Promise.allSettled`, whose
// `{ status, value | reason }` settlement is the ECMA shape that replaced it.

use super::*;
use crate::ast_support::{finish_program, number};

#[tokio::test(flavor = "current_thread")]
async fn promise_all_over_tool_calls_fans_out_and_returns_values() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const ids = ["a", "b", "c"];
        const results = await Promise.all(ids.map((id) => tools.sleep_echo({ value: id })));
        finish(results);
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
                Value::String("a".into()),
                Value::String("b".into()),
                Value::String("c".into()),
            ]
            .into()
        ),
        "an aggregate of tool calls must yield unwrapped values"
    );
    assert_eq!(
        host.max_active.load(Ordering::SeqCst),
        3,
        "every call in the aggregate must start before any of them is awaited"
    );
    assert_eq!(
        host.calls.lock_recover().as_slice(),
        ["a", "b", "c"],
        "calls start in written order"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn awaiting_each_call_in_turn_stays_sequential() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        let results = [];
        for (const id of ["a", "b", "c"]) {
          results = results.concat([await tools.sleep_echo({ value: id })]);
        }
        finish(results);
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
                Value::String("a".into()),
                Value::String("b".into()),
                Value::String("c".into()),
            ]
            .into()
        )
    );
    assert_eq!(
        host.max_active.load(Ordering::SeqCst),
        1,
        "awaiting inside the loop awaits each call before starting the next"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_aggregate_honours_filters_and_nested_iteration() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const filtered = await Promise.all(
          ["a", "b", "c"].filter((id) => id !== "b").map((id) => tools.sleep_echo({ value: id }))
        );
        let pairs = [];
        for (const x of ["a", "b"]) {
          for (const y of ["1", "2"]) {
            pairs = pairs.concat([x + y]);
          }
        }
        const nested = await Promise.all(pairs.map((pair) => tools.sleep_echo({ value: pair })));
        const empty = await Promise.all([].map((id) => tools.sleep_echo({ value: id })));
        finish({ filtered: filtered, nested: nested, empty: empty });
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
        record["filtered"],
        Value::List(vec![Value::String("a".into()), Value::String("c".into())].into())
    );
    assert_eq!(
        record["nested"],
        Value::List(
            vec![
                Value::String("a1".into()),
                Value::String("a2".into()),
                Value::String("b1".into()),
                Value::String("b2".into()),
            ]
            .into()
        ),
        "nested iteration fans out in written order"
    );
    assert_eq!(record["empty"], Value::List(Vec::new().into()));
    assert_eq!(
        host.max_active.load(Ordering::SeqCst),
        4,
        "the nested aggregate starts all four calls before awaiting"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn promise_all_fails_on_the_first_failed_leaf() {
    let host = TestHost::default().with_file("Cargo.toml", "abc");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        let reported = "";
        try {
          await Promise.all(
            ["Cargo.toml", "missing.rs"].map((path) => files.read({ path: path }))
          );
        } catch (error) {
          reported = error.message;
        }
        finish(reported);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::String(reported) = value else {
        panic!("expected string");
    };
    assert!(
        reported.contains("missing file: missing.rs"),
        "the rejection carries the failing leaf's own text: {reported}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn promise_all_settled_keeps_per_item_errors_in_place() {
    let host = TestHost::default().with_file("Cargo.toml", "abc");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const results = await Promise.allSettled(
          ["Cargo.toml", "missing.rs"].map((path) => files.read({ path: path }))
        );
        finish(results.map((result) => ({
          status: result.status,
          detail: result.status === "fulfilled" ? result.value : result.reason.message
        })));
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::List(results) = value else {
        panic!("expected list");
    };
    assert_eq!(results.len(), 2);
    let ok = results[0].as_record().expect("first settlement");
    assert_eq!(ok["status"], Value::String("fulfilled".into()));
    assert_eq!(ok["detail"], Value::String("abc".into()));
    let err = results[1].as_record().expect("second settlement");
    assert_eq!(err["status"], Value::String("rejected".into()));
    assert_eq!(
        err["detail"],
        Value::String("missing file: missing.rs".into())
    );
}

/// Awaiting a value that is not a pending handle is a VM guard rather than a
/// dialect rule: TypeScript's `await` of an ordinary value is legal ECMA and
/// never reaches it, so the guard is pinned against the AST it protects.
#[tokio::test(flavor = "current_thread")]
async fn awaiting_an_already_resolved_value_is_a_loud_runtime_error() {
    let host = TestHost::default();
    let mut state = State::new();
    let program = finish_program(lashlang::Expr::Await(Box::new(number(1.0))));
    let error = lashlang::execute(&program, &mut state, &host)
        .await
        .expect_err("awaiting a resolved value must fail");

    assert!(
        matches!(&error, RuntimeError::AwaitExpectsHandle { .. }),
        "awaiting resolved values must fail loudly instead of wrapping them, got {error:?}"
    );
    assert!(
        error.to_string().contains("already resolved"),
        "the diagnostic names the repair: {error}"
    );
}
