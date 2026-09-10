use super::*;

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_resource_calls_run_concurrently_and_preserve_record_shape() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        results = await {
          left: tools.sleep_echo({ value: "a" })?,
          right: tools.sleep_echo({ value: "b" })?
        }
        finish results
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
    assert_eq!(record["left"], Value::String("a".into()));
    assert_eq!(record["right"], Value::String("b".into()));
    assert_eq!(
        host.max_active.load(Ordering::SeqCst),
        2,
        "aggregate await should dispatch independent tools concurrently"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_start_and_await_merges_distinct_results() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        left = start sleep_echo(value: "a")
        right = start sleep_echo(value: "b")
        results = await { left: left, right: right }
        finish { left: results.left?, right: results.right? }
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
    assert_eq!(record["left"], Value::String("a".to_string().into()));
    assert_eq!(record["right"], Value::String("b".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_returns_branch_results_in_order() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        results = await [
          start sleep_echo(value: "a"),
          start sleep_echo(value: "b")
        ]
        finish results
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::List(results) = value else {
        panic!("expected result list");
    };
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].as_record().unwrap()["value"],
        Value::String("a".to_string().into())
    );
    assert_eq!(
        results[1].as_record().unwrap()["value"],
        Value::String("b".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn await_record_returns_record_results() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        results = await {
          first: start sleep_echo(value: "a"),
          second: start sleep_echo(value: "b")
        }
        finish {
          first: results.first?,
          second: results.second?
        }
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
    assert_eq!(record["first"], Value::String("a".to_string().into()));
    assert_eq!(record["second"], Value::String("b".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn removed_parallel_keyword_is_parse_error() {
    let err = lashlang::compile(
        r#"
        parallel {
          start sleep_echo(value: "a")
        }
        "#,
    )
    .expect_err("parallel keyword should be removed");
    assert!(format!("{err}").contains("unexpected `parallel`"));
}

#[tokio::test(flavor = "current_thread")]
async fn slice_null_bounds_default_to_start_or_end() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        values = [10, 20, 30, 40, 50]
        finish {
          list_tail: slice(values, 3, null),
          list_head: slice(values, null, 2),
          string_tail: slice("abcdef", 4, null),
          string_head: slice("abcdef", null, 2)
        }
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
        record["list_tail"],
        Value::List(vec![Value::Number(40.0), Value::Number(50.0)].into())
    );
    assert_eq!(
        record["list_head"],
        Value::List(vec![Value::Number(10.0), Value::Number(20.0)].into())
    );
    assert_eq!(
        record["string_tail"],
        Value::String("ef".to_string().into())
    );
    assert_eq!(
        record["string_head"],
        Value::String("ab".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn negative_indices_and_record_contains_are_supported() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        values = [10, 20, 30]
        text = "abc"
        finish {
          tail: values[-1],
          before_tail: values[-2],
          oob: values[-4],
          last_char: text[-1],
          record_has_key: contains({ foo: 1, bar: 2 }, "foo"),
          record_missing_key: contains({ foo: 1, bar: 2 }, "baz")
        }
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
    assert_eq!(record["tail"], Value::Number(30.0));
    assert_eq!(record["before_tail"], Value::Number(20.0));
    assert_eq!(record["oob"], Value::Null);
    assert_eq!(record["last_char"], Value::String("c".to_string().into()));
    assert_eq!(record["record_has_key"], Value::Bool(true));
    assert_eq!(record["record_missing_key"], Value::Bool(false));
}

#[tokio::test(flavor = "current_thread")]
async fn membership_operator_supports_lists_record_keys_and_string_substrings() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        needle = 2
        haystack = [1, 2, 3]
        substring = "bc"
        text = "abcd"
        finish {
          list_present: needle in haystack,
          list_missing: 4 in [1, 2, 3],
          record_present: "foo" in { foo: 1, bar: 2 },
          record_missing: "baz" in { foo: 1, bar: 2 },
          string_present: substring in text,
          string_missing: "xz" in text,
          string_negated: !("xz" in text)
        }
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
    assert_eq!(record["list_present"], Value::Bool(true));
    assert_eq!(record["list_missing"], Value::Bool(false));
    assert_eq!(record["record_present"], Value::Bool(true));
    assert_eq!(record["record_missing"], Value::Bool(false));
    assert_eq!(record["string_present"], Value::Bool(true));
    assert_eq!(record["string_missing"], Value::Bool(false));
    assert_eq!(record["string_negated"], Value::Bool(true));
}

#[tokio::test(flavor = "current_thread")]
async fn shaping_builtins_are_deterministic_and_preserve_stable_order() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        rows = [
          { id: "first", profile: { score: 2 } },
          { id: "second", profile: { score: 1 } },
          { id: "third", profile: { score: 2 } }
        ]
        finish {
          sorted: sort([3, 1, 2]),
          sorted_by: sort_by(rows, "profile.score"),
          sum: sum([1, 2, 3]),
          min: min([3, 1, 2]),
          max: max([3, 1, 2]),
          replaced: replace("a-b-a", "a", "x"),
          lower: lower("Straße"),
          upper: upper("Straße"),
          unique: unique([1, 2, 1, 3, 2]),
          reversed: reverse([1, 2, 3])
        }
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
        record["sorted"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );
    let Value::List(sorted_by) = &record["sorted_by"] else {
        panic!("expected sorted rows");
    };
    let ids = sorted_by
        .iter()
        .map(|row| row.as_record().expect("row")["id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            Value::String("second".into()),
            Value::String("first".into()),
            Value::String("third".into()),
        ]
    );
    assert_eq!(record["sum"], Value::Number(6.0));
    assert_eq!(record["min"], Value::Number(1.0));
    assert_eq!(record["max"], Value::Number(3.0));
    assert_eq!(record["replaced"], Value::String("x-b-x".into()));
    assert_eq!(record["lower"], Value::String("straße".into()));
    assert_eq!(record["upper"], Value::String("STRASSE".into()));
    assert_eq!(
        record["unique"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );
    assert_eq!(
        record["reversed"],
        Value::List(vec![Value::Number(3.0), Value::Number(2.0), Value::Number(1.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn empty_extrema_are_typed_runtime_errors() {
    let host = TestHost::default();
    for builtin in ["min", "max"] {
        let mut state = State::new();
        let error = execute(&format!("finish {builtin}([])"), &mut state, &host)
            .await
            .expect_err("empty extrema must fail");
        assert!(
            matches!(error, ExecuteError::Runtime(RuntimeError::ShapingEmptyList { builtin: actual }) if actual == builtin)
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_record_indexing_reads_fields() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        key = "foo"
        record = { foo: 42 }
        finish { found: record[key], missing: record["missing"] }
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
    assert_eq!(record["found"], Value::Number(42.0));
    assert_eq!(record["missing"], Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_and_field_assignment_update_collections() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        record = {}
        key = "count"
        record[key] = 1
        record.count = record.count + 1
        record.extra = "ok"
        items = [1, 2, 3]
        items[1] = 20
        items[-1] = 30
        finish { record: record, items: items }
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
    let counts = record["record"]
        .as_record()
        .expect("expected nested record");
    assert_eq!(counts["count"], Value::Number(2.0));
    assert_eq!(counts["extra"], Value::String("ok".into()));
    assert_eq!(
        record["items"],
        Value::List(vec![Value::Number(1.0), Value::Number(20.0), Value::Number(30.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_path_assignment_and_histogram_loops_work() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        state = { groups: { a: { counts: [1, 2] }, b: { counts: [3] } } }
        g = "a"
        state.groups[g].counts[1] = 5
        counts = {}
        labels = ["a", "b", "a", "c", "b", "a"]
        for label in labels {
          counts[label] = counts[label] + 1
        }
        finish { state: state, counts: counts }
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
    let state = record["state"].as_record().expect("expected state record");
    let groups = state["groups"].as_record().expect("expected groups record");
    let group_a = groups["a"].as_record().expect("expected group record");
    assert_eq!(
        group_a["counts"],
        Value::List(vec![Value::Number(1.0), Value::Number(5.0)].into())
    );
    let counts = record["counts"]
        .as_record()
        .expect("expected counts record");
    assert_eq!(counts["a"], Value::Number(3.0));
    assert_eq!(counts["b"], Value::Number(2.0));
    assert_eq!(counts["c"], Value::Number(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn path_assignment_preserves_alias_isolation() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        record = { x: 1, nested: { y: 1 }, items: [1, 2] }
        alias = record
        record.x = 2
        record.nested.y = 3
        record.items[0] = 9
        finish { record: record, alias: alias }
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
    let updated = record["record"]
        .as_record()
        .expect("expected updated record");
    let alias = record["alias"].as_record().expect("expected alias record");
    assert_eq!(updated["x"], Value::Number(2.0));
    assert_eq!(
        updated["nested"].as_record().unwrap()["y"],
        Value::Number(3.0)
    );
    assert_eq!(
        updated["items"],
        Value::List(vec![Value::Number(9.0), Value::Number(2.0)].into())
    );
    assert_eq!(alias["x"], Value::Number(1.0));
    assert_eq!(
        alias["nested"].as_record().unwrap()["y"],
        Value::Number(1.0)
    );
    assert_eq!(
        alias["items"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn path_assignment_rhs_matches_pre_heap_value_semantics() {
    let host = TestHost::default();
    let mut state = State::new();
    execute(
        r#"
            a = {}
            b = []
            a.x = b
            b = push(b, 1)
        "#,
        &mut state,
        &host,
    )
    .await
    .expect("setup should succeed");
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("snapshot should encode");
    let snapshot =
        lashlang::Snapshot::from_canonical_bytes(&bytes).expect("snapshot should decode");
    let mut restored = State::from_snapshot(snapshot);
    let value = finished(
        execute("finish a.x", &mut restored, &host)
            .await
            .expect("restored execution should succeed"),
    );

    assert_eq!(value, Value::List(Vec::new().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn iterator_binding_matches_pre_heap_value_semantics() {
    let host = TestHost::default();
    let mut state = State::new();
    execute(
        r#"
            a = [[1]]
            for x in a {
              x = push(x, 2)
            }
        "#,
        &mut state,
        &host,
    )
    .await
    .expect("setup should succeed");
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("snapshot should encode");
    let snapshot =
        lashlang::Snapshot::from_canonical_bytes(&bytes).expect("snapshot should decode");
    let mut restored = State::from_snapshot(snapshot);
    let value = finished(
        execute("finish a", &mut restored, &host)
            .await
            .expect("restored execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_insertion_matches_pre_heap_value_semantics() {
    let value = finished(
        execute(
            r#"
            acc = []
            for n in range(0, 3) {
              row = [n]
              acc = push(acc, row)
              row = push(row, 99)
            }
            finish acc
            "#,
            &mut State::new(),
            &TestHost::default(),
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::List(vec![Value::Number(0.0)].into()),
                Value::List(vec![Value::Number(1.0)].into()),
                Value::List(vec![Value::Number(2.0)].into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn iterator_value_pushed_into_container_matches_pre_heap_semantics() {
    let value = finished(
        execute(
            r#"
            xs = [[1]]
            acc = []
            for x in xs {
              acc = push(acc, x)
              x = push(x, 9)
            }
            finish acc
            "#,
            &mut State::new(),
            &TestHost::default(),
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_field_index_and_comprehension_inserts_keep_value_semantics() {
    let value = finished(
        execute(
            r#"
            source = { rows: [[[1]], [[2]]] }
            flattened = [inner for outer in source.rows for inner in outer]
            picked = source.rows[0][0]
            holder = { items: [] }
            holder.items = push(holder.items, picked)
            picked = push(picked, 9)
            for inner in source.rows[1] { inner = push(inner, 8) }
            finish { flattened: flattened, held: holder.items, source: source }
            "#,
            &mut State::new(),
            &TestHost::default(),
        )
        .await
        .expect("nested insertion program should succeed"),
    );
    let record = value.as_record().expect("result should be a record");
    assert_eq!(
        record["flattened"],
        Value::List(
            vec![
                Value::List(vec![Value::Number(1.0)].into()),
                Value::List(vec![Value::Number(2.0)].into()),
            ]
            .into()
        )
    );
    assert_eq!(
        record["held"],
        Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
    );
    assert_eq!(
        record["source"],
        Value::Record(Arc::new(Record::from_iter([(
            "rows".to_string(),
            Value::List(
                vec![
                    Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into()),
                    Value::List(vec![Value::List(vec![Value::Number(2.0)].into())].into()),
                ]
                .into()
            ),
        )])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn effect_result_to_path_isolated_from_later_field_reads() {
    let host = TestHost::default().with_file("a.txt", "original");
    let source = r#"
            holder = { value: null }
            holder.value = await files.read({ path: "a.txt" })?
            local = holder.value
            finish { local: local, stored: holder.value }
            "#;
    let linked = lashlang::LinkedModule::link(
        parse(source).expect("effect path program should parse"),
        test_host_environment(),
    )
    .expect("effect path program should link");
    let compiled = lashlang::compile_linked(&linked);
    let mut state = State::new();
    let value = finished(
        lashlang::execute(&compiled, &mut state, &host)
            .await
            .expect("effect path insertion should succeed"),
    );
    assert_eq!(
        value,
        Value::Record(Arc::new(Record::from_iter([
            ("local".to_string(), Value::String("original".into())),
            ("stored".to_string(), Value::String("original".into())),
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn heap_aware_global_patches_survive_next_cell_and_cold_restore() {
    let host = TestHost::default();
    let mut state = State::new();
    execute("seed = [1]", &mut state, &host)
        .await
        .expect("heap setup should succeed");
    assert!(
        state
            .set_default(
                "diary",
                Value::List(vec![Value::String("kept".into())].into()),
            )
            .expect("default should be metered")
    );

    let value = finished(
        execute("finish diary", &mut state, &host)
            .await
            .expect("patched global should reach the next cell"),
    );
    assert_eq!(
        value,
        Value::List(vec![Value::String("kept".into())].into())
    );

    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("patched snapshot should encode");
    let snapshot =
        lashlang::Snapshot::from_canonical_bytes(&bytes).expect("snapshot should decode");
    let mut restored = State::from_snapshot(snapshot);
    let value = finished(
        execute("finish diary", &mut restored, &host)
            .await
            .expect("patched global should survive cold restore"),
    );
    assert_eq!(
        value,
        Value::List(vec![Value::String("kept".into())].into())
    );

    assert!(restored.remove_global("diary").is_some());
    assert!(matches!(
        execute("finish diary", &mut restored, &host).await,
        Err(ExecuteError::Runtime(
            RuntimeError::UndefinedVariable { .. }
        ))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn path_assignment_reports_invalid_targets() {
    assert!(matches!(
        runtime_error("items = [1]\nitems[2] = 2").await,
        RuntimeError::ListAssignmentIndexOutOfBounds
    ));
    assert!(matches!(
        runtime_error("items = [1]\nitems[0.5] = 2").await,
        RuntimeError::InvalidListAssignmentIndex
    ));
    assert!(matches!(
        runtime_error("items = [1]\nitems[\"0\"] = 2").await,
        RuntimeError::InvalidListAssignmentIndex
    ));
    assert!(matches!(
        runtime_error("text = \"abc\"\ntext[0] = \"x\"").await,
        RuntimeError::CannotAssignIndex { actual } if actual == "string"
    ));
    assert!(matches!(
        runtime_error("record = {}\nrecord.missing.value = 1").await,
        RuntimeError::MissingAssignmentField { field } if field == "missing"
    ));
    assert!(matches!(
        runtime_error("record = { item: 1 }\nrecord.item.value = 2").await,
        RuntimeError::CannotAssignField { actual, .. } if actual == "number"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn else_if_chains_execute_without_extra_braces() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        score = 7
        if score > 10 {
          label = "large"
        } else if score > 5 {
          label = "medium"
        } else {
          label = "small"
        }
        finish label
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("medium".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn slice_supports_negative_bounds() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        values = [10, 20, 30, 40, 50]
        finish {
          list_tail: slice(values, -2, null),
          list_without_last: slice(values, null, -1),
          list_middle: slice(values, -4, -1),
          string_tail: slice("abcdef", -2, null),
          string_without_last: slice("abcdef", null, -1),
          string_middle: slice("abcdef", -5, -2)
        }
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
        record["list_tail"],
        Value::List(vec![Value::Number(40.0), Value::Number(50.0)].into())
    );
    assert_eq!(
        record["list_without_last"],
        Value::List(
            vec![
                Value::Number(10.0),
                Value::Number(20.0),
                Value::Number(30.0),
                Value::Number(40.0),
            ]
            .into()
        )
    );
    assert_eq!(
        record["list_middle"],
        Value::List(
            vec![
                Value::Number(20.0),
                Value::Number(30.0),
                Value::Number(40.0),
            ]
            .into()
        )
    );
    assert_eq!(
        record["string_tail"],
        Value::String("ef".to_string().into())
    );
    assert_eq!(
        record["string_without_last"],
        Value::String("abcde".to_string().into())
    );
    assert_eq!(
        record["string_middle"],
        Value::String("bcd".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn range_and_push_cover_common_collection_building() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        indexes = range(0, 3)
        extended = push(indexes, 3)
        loop_total = 0
        for n in range(0, 4) {
          loop_total = loop_total + n
        }
        finish {
          indexes: indexes,
          extended: extended,
          from_zero: range(3),
          negative: range(-2, 1),
          empty: range(5, 2),
          loop_total: loop_total
        }
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
        record["indexes"],
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(
        record["extended"],
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
    assert_eq!(
        record["from_zero"],
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(
        record["negative"],
        Value::List(vec![Value::Number(-2.0), Value::Number(-1.0), Value::Number(0.0)].into())
    );
    assert_eq!(record["empty"], Value::List(Vec::new().into()));
    assert_eq!(record["loop_total"], Value::Number(6.0));
}

#[tokio::test(flavor = "current_thread")]
async fn for_loop_assignments_carry_across_iterations() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        raw = split(" x , y , z ", ",")
        parts = []
        count = 0
        snapshots = []
        for part in raw {
          parts = push(parts, trim(part))
          count = count + 1
          snapshots = push(snapshots, { part: trim(part), parts: parts, count: count })
        }
        finish { parts: parts, count: count, snapshots: snapshots }
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
        record["parts"],
        Value::List(
            vec![
                Value::String("x".into()),
                Value::String("y".into()),
                Value::String("z".into()),
            ]
            .into()
        )
    );
    assert_eq!(record["count"], Value::Number(3.0));
    let Value::List(snapshots) = &record["snapshots"] else {
        panic!("expected snapshots list");
    };
    assert_eq!(snapshots.len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn await_record_accepts_commas_and_keyword_record_keys_execute() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        result = await {
          fanout: start sleep_echo(value: "ok"),
          "with space": start sleep_echo(value: "quoted"),
        }
        finish {
          branch: result.fanout?,
          quoted_value: result["with space"]?
        }
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
    assert_eq!(record["branch"], Value::String("ok".into()));
    assert_eq!(record["quoted_value"], Value::String("quoted".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn string_comparisons_are_lexicographic() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish {
          lt: "abc" < "def",
          gt: "xyz" > "abc",
          le: "abc" <= "abc",
          ge: "xyz" >= "abc"
        }
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
    assert_eq!(record["lt"], Value::Bool(true));
    assert_eq!(record["gt"], Value::Bool(true));
    assert_eq!(record["le"], Value::Bool(true));
    assert_eq!(record["ge"], Value::Bool(true));
}

#[tokio::test(flavor = "current_thread")]
async fn stringification_preserves_integer_format_inside_containers() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish {
          list_text: to_string([1, 2]),
          record_text: to_string({ a: 1, b: 2.5 })
        }
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
        record["list_text"],
        Value::String("[1,2]".to_string().into())
    );
    assert_eq!(
        record["record_text"],
        Value::String("{\"a\":1,\"b\":2.5}".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_round_trip_preserves_repl_like_state() {
    let host = TestHost::default();
    let mut state = State::new();

    finished(
        execute(
            r#"
        counter = 1
        finish counter
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("first execution should succeed"),
    );

    let snapshot = state.snapshot();
    let encoded = snapshot
        .to_canonical_bytes()
        .expect("snapshot should serialize");
    let decoded =
        lashlang::Snapshot::from_canonical_bytes(&encoded).expect("snapshot should deserialize");
    let mut restored = State::from_snapshot(decoded);

    let value = finished(
        execute(
            r#"
        counter = counter + 1
        finish counter
        "#,
            &mut restored,
            &host,
        )
        .await
        .expect("restored execution should succeed"),
    );

    assert_eq!(value, Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn json_and_record_helpers_work() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        obj = json_parse("{\"path\":\"src/lib.rs\",\"line\":7}")
        finish format("{}:{}", obj.path, obj.line)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("src/lib.rs:7".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn parse_errors_are_parse_level_and_precise() {
    let error = parse(
        r#"
        if true {
          answer = 1
        "#,
    )
    .expect_err("parse should fail");

    match error {
        lashlang::ParseError::Expected { expected, .. } => assert_eq!(expected, "`}`"),
        other => panic!("unexpected parse error: {other:?}"),
    }
}
