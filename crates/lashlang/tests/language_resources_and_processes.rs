// Host calls, processes, collections and the heap, authored in TypeScript.
//
// ADR 0096 makes TypeScript the sole authored RLM dialect. The facts this file
// pins are the VM's, not the retired surface's: aggregates dispatch
// concurrently, a started process settles through the host, indexing and path
// assignment share heap objects across aliases and across a snapshot, and the
// shaping builtins keep their declared order and errors. Where a construct had
// no TypeScript spelling — the shaping builtins, `range`/`push`, the
// assignment-target guards — the program is built from the AST, which is the
// only path those IR nodes still have.

use super::*;
use crate::ast_support::{number, program, string};

#[expect(
    clippy::expect_used,
    reason = "the fixture programs are well formed; only execution errors are under test"
)]
async fn run(program: lashlang::Program) -> Result<Value, RuntimeError> {
    let host = TestHost::default();
    let mut state = State::new();
    lashlang::execute(
        &lashlang_compile_program(&program).expect("the program compiles"),
        &mut state,
        &host,
    )
    .await
    .map(finished)
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_resource_calls_run_concurrently_and_preserve_record_shape() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const settled = await Promise.all([
          tools.sleep_echo({ value: "a" }),
          tools.sleep_echo({ value: "b" })
        ]);
        finish({ left: settled[0], right: settled[1] });
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
        const sleep_echo = async (value: string) => { return value; };
        const left = await processes.start({ definition: sleep_echo, args: { value: "a" } });
        const right = await processes.start({ definition: sleep_echo, args: { value: "b" } });
        finish({ left: await left, right: await right });
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

/// A started process is not a leaf of an awaited aggregate.
///
/// ADR 0087 settled process handles in a second phase after the tool batch, so
/// writing them into `Promise.all` worked. There is one recorded batch order
/// now (ADR 0095) and a durable wait joins it as `processes.await(handle)`,
/// the leaf tool that parks on it. Awaiting each handle on its own — the form
/// `explicit_start_and_await_merges_distinct_results` pins — is unaffected.
#[tokio::test(flavor = "current_thread")]
async fn a_started_process_is_not_an_aggregate_leaf() {
    let host = TestHost::default();
    let mut state = State::new();

    let error = execute(
        r#"
        const sleep_echo = async (value: string) => { return value; };
        const left = await processes.start({ definition: sleep_echo, args: { value: "a" } });
        const right = await processes.start({ definition: sleep_echo, args: { value: "b" } });
        const results = await Promise.all([left, right]);
        finish({ first: results[0], second: results[1] });
        "#,
        &mut state,
        &host,
    )
    .await
    .expect_err("a process handle is not a leaf of the batch");
    assert!(
        error.to_string().contains("processes.await(handle)"),
        "the refusal must name the tool that parks on the wait: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn slice_bounds_default_to_start_or_end() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const values = [10, 20, 30, 40, 50];
        finish({
          list_tail: values.slice(3),
          list_head: values.slice(0, 2),
          string_tail: "abcdef".slice(4),
          string_head: "abcdef".slice(0, 2)
        });
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
async fn slice_supports_negative_bounds() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const values = [10, 20, 30, 40, 50];
        finish({
          list_tail: values.slice(-2),
          list_without_last: values.slice(0, -1),
          list_middle: values.slice(-4, -1),
          string_tail: "abcdef".slice(-2),
          string_without_last: "abcdef".slice(0, -1),
          string_middle: "abcdef".slice(-5, -2)
        });
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
async fn out_of_range_index_reads_are_undefined_and_key_membership_is_supported() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const values = [10, 20, 30];
        const text = "abc";
        finish({
          tail: values[-1],
          before_tail: values[-2],
          oob: values[-4],
          last_char: text[-1],
          record_has_key: "foo" in { foo: 1, bar: 2 },
          record_missing_key: "baz" in { foo: 1, bar: 2 }
        });
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
    // A negative index is an ordinary property name, not an offset from the
    // end, so every one of these reads is `undefined` (ADR 0096).
    assert_eq!(record["tail"], Value::Undefined);
    assert_eq!(record["before_tail"], Value::Undefined);
    assert_eq!(record["oob"], Value::Undefined);
    assert_eq!(record["last_char"], Value::Undefined);
    assert_eq!(record["record_has_key"], Value::Bool(true));
    assert_eq!(record["record_missing_key"], Value::Bool(false));
}

/// The dialect's `in` operator covered lists, object keys and substrings with
/// one spelling. ECMA splits them: `in` is key membership, `includes` is
/// element and substring membership (ADR 0096).
#[tokio::test(flavor = "current_thread")]
async fn membership_covers_lists_object_keys_and_string_substrings() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const needle = 2;
        const haystack = [1, 2, 3];
        const substring = "bc";
        const text = "abcd";
        finish({
          list_present: haystack.includes(needle),
          list_missing: [1, 2, 3].includes(4),
          record_present: "foo" in { foo: 1, bar: 2 },
          record_missing: "baz" in { foo: 1, bar: 2 },
          string_present: text.includes(substring),
          string_missing: text.includes("xz"),
          string_negated: !text.includes("xz")
        });
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
async fn dynamic_record_indexing_reads_fields() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const key = "foo";
        const source = { foo: 42 };
        finish({ found: source[key], missing: source["missing"] });
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
    // An absent key reads as `undefined`, not `null` (ADR 0096).
    assert_eq!(record["missing"], Value::Undefined);
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_and_field_assignment_update_collections() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const record = {};
        const key = "count";
        record[key] = 1;
        record.count = record.count + 1;
        record.extra = "ok";
        const items = [1, 2, 3];
        items[1] = 20;
        items[2] = 30;
        finish({ record: record, items: items });
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
        const shape = { groups: { a: { counts: [1, 2] }, b: { counts: [3] } } };
        const g = "a";
        shape.groups[g].counts[1] = 5;
        const counts = {};
        for (const label of ["a", "b", "a", "c", "b", "a"]) {
          counts[label] = (counts[label] === undefined ? 0 : counts[label]) + 1;
        }
        finish({ shape: shape, counts: counts });
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
    let shape = record["shape"].as_record().expect("expected shape record");
    let groups = shape["groups"].as_record().expect("expected groups record");
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
async fn path_assignment_is_visible_through_every_alias() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const record = { x: 1, nested: { y: 1 }, items: [1, 2] };
        const alias = record;
        record.x = 2;
        record.nested.y = 3;
        record.items[0] = 9;
        finish({ record: record, alias: alias });
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
    // `alias` names the same record, so every write above is visible through
    // it (ADR 0096).
    assert_eq!(alias["x"], Value::Number(2.0));
    assert_eq!(
        alias["nested"].as_record().unwrap()["y"],
        Value::Number(3.0)
    );
    assert_eq!(
        alias["items"],
        Value::List(vec![Value::Number(9.0), Value::Number(2.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn path_assignment_rhs_shares_the_assigned_object_across_a_snapshot() {
    let host = TestHost::default();
    let mut state = State::new();
    execute(
        r#"
            const a = {};
            const b = [];
            a.x = b;
            b.push(1);
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
        execute("finish(a.x);", &mut restored, &host)
            .await
            .expect("restored execution should succeed"),
    );

    assert_eq!(value, Value::List(vec![Value::Number(1.0)].into()));
}

#[tokio::test(flavor = "current_thread")]
async fn iterator_binding_aliases_the_iterated_element() {
    let host = TestHost::default();
    let mut state = State::new();
    execute(
        r#"
            const a = [[1]];
            for (const x of a) {
              x.push(2);
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
        execute("finish(a);", &mut restored, &host)
            .await
            .expect("restored execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![Value::List(
                vec![Value::Number(1.0), Value::Number(2.0)].into()
            )]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_field_index_and_flattening_inserts_stay_shared() {
    let value = finished(
        execute(
            r#"
            const source = { rows: [[[1]], [[2]]] };
            let flattened = [];
            for (const outer of source.rows) {
              for (const inner of outer) {
                flattened.push(inner);
              }
            }
            const picked = source.rows[0][0];
            const holder = { items: [] };
            holder.items.push(picked);
            picked.push(9);
            for (const inner of source.rows[1]) { inner.push(8); }
            finish({ flattened: flattened, held: holder.items, source: source });
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
                Value::List(vec![Value::Number(1.0), Value::Number(9.0)].into()),
                Value::List(vec![Value::Number(2.0), Value::Number(8.0)].into()),
            ]
            .into()
        )
    );
    assert_eq!(
        record["held"],
        Value::List(
            vec![Value::List(
                vec![Value::Number(1.0), Value::Number(9.0)].into()
            )]
            .into()
        )
    );
    assert_eq!(
        record["source"],
        Value::Record(Arc::new(Record::from_iter([(
            "rows".to_string(),
            Value::List(
                vec![
                    Value::List(
                        vec![Value::List(
                            vec![Value::Number(1.0), Value::Number(9.0)].into()
                        )]
                        .into()
                    ),
                    Value::List(
                        vec![Value::List(
                            vec![Value::Number(2.0), Value::Number(8.0)].into()
                        )]
                        .into()
                    ),
                ]
                .into()
            ),
        )])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn effect_result_to_path_isolated_from_later_field_reads() {
    let host = TestHost::default().with_file("a.txt", "original");
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            const holder = { value: null };
            holder.value = await files.read({ path: "a.txt" });
            const local = holder.value;
            finish({ local: local, stored: holder.value });
            "#,
            &mut state,
            &host,
        )
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
    execute("const seed = [1];", &mut state, &host)
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
        execute("finish(diary);", &mut state, &host)
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
        execute("finish(diary);", &mut restored, &host)
            .await
            .expect("patched global should survive cold restore"),
    );
    assert_eq!(
        value,
        Value::List(vec![Value::String("kept".into())].into())
    );

    assert!(restored.remove_global("diary"));
    // With the global gone the name is nobody's, which the TypeScript
    // front-end reports at parse rather than letting it reach the VM.
    assert!(matches!(
        execute("finish(diary);", &mut restored, &host).await,
        Err(ExecuteError::Parse(_))
    ));
}

/// The assignment-target guards are VM facts about paths that have no slot to
/// write. TypeScript never reaches two of them — a string index write is a
/// silent no-op in ECMA — so they are pinned against the AST that does.
#[tokio::test(flavor = "current_thread")]
async fn path_assignment_reports_invalid_targets() {
    let assign_path = |root: &str, root_value: lashlang::Expr, steps, value| {
        program(vec![
            lashlang::Expr::Assign {
                target: lashlang::AssignTarget::variable(root.into()),
                expr: Box::new(root_value),
            },
            lashlang::Expr::Assign {
                target: lashlang::AssignTarget {
                    root: root.into(),
                    steps,
                },
                expr: Box::new(value),
            },
        ])
    };

    let error = run(assign_path(
        "text",
        string("abc"),
        vec![lashlang::AssignPathStep::Index(number(0.0))],
        string("x"),
    ))
    .await
    .expect_err("a string has no index slot");
    assert!(
        matches!(&error, RuntimeError::CannotAssignIndex { actual } if actual == "string"),
        "{error:?}"
    );

    let error = run(assign_path(
        "record",
        lashlang::Expr::Record(Vec::new()),
        vec![
            lashlang::AssignPathStep::Field("missing".into()),
            lashlang::AssignPathStep::Field("value".into()),
        ],
        number(1.0),
    ))
    .await
    .expect_err("an absent intermediate field has no slot");
    assert!(
        matches!(&error, RuntimeError::MissingAssignmentField { field } if field == "missing"),
        "{error:?}"
    );

    let error = run(assign_path(
        "record",
        lashlang::Expr::Record(vec![("item".into(), number(1.0))]),
        vec![
            lashlang::AssignPathStep::Field("item".into()),
            lashlang::AssignPathStep::Field("value".into()),
        ],
        number(2.0),
    ))
    .await
    .expect_err("a scalar has no field slot");
    assert!(
        matches!(&error, RuntimeError::CannotAssignField { actual, .. } if actual == "number"),
        "{error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn else_if_chains_execute_without_extra_braces() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const score = 7;
        let label = "small";
        if (score > 10) {
          label = "large";
        } else if (score > 5) {
          label = "medium";
        }
        finish(label);
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
async fn for_loop_assignments_carry_across_iterations() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const raw = " x , y , z ".split(",");
        const parts = [];
        let count = 0;
        const snapshots = [];
        for (const part of raw) {
          parts.push(part.trim());
          count = count + 1;
          snapshots.push({ part: part.trim(), parts: parts, count: count });
        }
        finish({ parts: parts, count: count, snapshots: snapshots });
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
async fn object_literals_accept_quoted_keys_around_awaited_results() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        const settled = await Promise.all([
          tools.sleep_echo({ value: "ok" }),
          tools.sleep_echo({ value: "quoted" })
        ]);
        const result = {
          fanout: settled[0],
          "with space": settled[1]
        };
        finish({
          branch: result.fanout,
          quoted_value: result["with space"]
        });
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
        finish({
          lt: "abc" < "def",
          gt: "xyz" > "abc",
          le: "abc" <= "abc",
          ge: "xyz" >= "abc"
        });
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
        finish({
          list_text: JSON.stringify([1, 2]),
          record_text: JSON.stringify({ a: 1, b: 2.5 })
        });
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
        const counter = 1;
        finish(counter);
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
        const next = counter + 1;
        finish(next);
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
        const obj = JSON.parse("{\"path\":\"src/lib.rs\",\"line\":7}");
        finish(obj.path + ":" + obj.line);
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("src/lib.rs:7".to_string().into()));
}

/// A malformed program is refused by the dialect front-end before anything
/// reaches the IR, and the refusal carries its own span (ADR 0096).
#[tokio::test(flavor = "current_thread")]
async fn source_errors_are_reported_by_the_dialect_front_end() {
    let host = TestHost::default();
    let mut state = State::new();
    let error = execute(
        r#"
        if (true) {
          const answer = 1;
        "#,
        &mut state,
        &host,
    )
    .await
    .expect_err("an unterminated block must be refused");

    let ExecuteError::Parse(diagnostic) = error else {
        panic!("expected a front-end diagnostic, got {error:?}");
    };
    assert_eq!(
        diagnostic.code,
        lash_typescript::DiagnosticCode::SyntaxError
    );
    assert_eq!(diagnostic.message, "Expected '}', got '<eof>'");
    assert!(
        diagnostic.span.is_some(),
        "the refusal must carry a span: {diagnostic:?}"
    );
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
