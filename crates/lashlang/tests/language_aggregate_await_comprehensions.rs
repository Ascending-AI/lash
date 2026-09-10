// Aggregate await over list comprehensions (FIG-2764).
//
// `await [op(x)? for x in xs]` is an aggregate-await shape: every call starts
// before any of them is awaited, and `?` on the leaf unwraps each element
// exactly as it does in a literal list. `[await op(x)? for x in xs]` stays
// the sequential form.

#[tokio::test(flavor = "current_thread")]
async fn await_list_comprehension_of_unwrapped_calls_fans_out_and_returns_values() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        ids = ["a", "b", "c"]
        results = await [tools.sleep_echo({ value: id })? for id in ids]
        finish results
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
        "`?` on the comprehension leaf must yield unwrapped values, not result records"
    );
    assert_eq!(
        host.max_active.load(Ordering::SeqCst),
        3,
        "every comprehension call must start before any of them is awaited"
    );
    assert_eq!(
        host.calls.lock_recover().as_slice(),
        ["a", "b", "c"],
        "calls start in comprehension order"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_comprehension_of_wrapped_calls_returns_wrappers_in_order() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        results = await [tools.sleep_echo({ value: id }) for id in ["a", "b"]]
        finish results
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
    for (result, expected) in results.iter().zip(["a", "b"]) {
        let record = result.as_record().expect("wrapped result record");
        assert_eq!(record["ok"], Value::Bool(true));
        assert_eq!(record["value"], Value::String(expected.into()));
    }
    assert_eq!(host.max_active.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn list_comprehension_of_awaited_calls_stays_sequential() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        results = [await tools.sleep_echo({ value: id })? for id in ["a", "b", "c"]]
        finish results
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
        "`[await op(x)? for x in xs]` awaits each call before starting the next"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_comprehension_honours_filters_and_nested_clauses() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        filtered = await [tools.sleep_echo({ value: id })? for id in ["a", "b", "c"] if id != "b"]
        nested = await [
          tools.sleep_echo({ value: format("{}{}", x, y) })?
          for x in ["a", "b"]
          if x != "c"
          for y in ["1", "2"]
          if y != "3"
        ]
        empty = await [tools.sleep_echo({ value: id })? for id in []]
        finish { filtered: filtered, nested: nested, empty: empty }
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
        "nested clauses fan out in Python clause order"
    );
    assert_eq!(record["empty"], Value::List(Vec::new().into()));
    assert_eq!(
        host.max_active.load(Ordering::SeqCst),
        4,
        "the nested comprehension starts all four calls before awaiting"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_comprehension_with_unwrap_fails_on_the_first_failed_leaf() {
    let host = TestHost::default().with_file("Cargo.toml", "abc");
    let mut state = State::new();

    let error = match execute(
        r#"
        contents = await [files.read({ path: path })? for path in ["Cargo.toml", "missing.rs"]]
        finish contents
        "#,
        &mut state,
        &host,
    )
    .await
    {
        Err(ExecuteError::Runtime(error)) => error,
        other => panic!("expected the `?` leaf to fail the cell, got {other:?}"),
    };

    let RuntimeError::UnwrappedModuleOperationFailed { source } = &error else {
        panic!("expected the `?` diagnostic, got {error:?}");
    };
    assert_eq!(source.to_string(), "missing file: missing.rs");
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_comprehension_without_unwrap_keeps_per_item_errors_in_place() {
    let host = TestHost::default().with_file("Cargo.toml", "abc");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        results = await [files.read({ path: path }) for path in ["Cargo.toml", "missing.rs"]]
        finish results
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
    let ok = results[0].as_record().expect("first result record");
    assert_eq!(ok["ok"], Value::Bool(true));
    assert_eq!(ok["value"], Value::String("abc".into()));
    let err = results[1].as_record().expect("second result record");
    assert_eq!(err["ok"], Value::Bool(false));
    assert_eq!(
        err["error"],
        Value::String("missing file: missing.rs".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn awaiting_an_already_resolved_value_is_a_loud_runtime_error() {
    let error = runtime_error(
        r#"
        values = [await tools.sleep_echo({ value: id })? for id in ["a"]]
        finish await values
        "#,
    )
    .await;

    assert!(
        matches!(&error, RuntimeError::AwaitExpectsHandle { .. }),
        "awaiting resolved values must fail loudly instead of wrapping them, got {error:?}"
    );
    assert!(
        error.to_string().contains("already resolved"),
        "the diagnostic names the repair: {error}"
    );
}
use super::*;
