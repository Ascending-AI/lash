use super::case_builders::*;
use super::*;

/// `@label(title: "Spawn subagent with web search")`
/// / `result = await tools.echo({ value: { ok: true } })?` / `finish result`
fn labeled_spawn_program() -> Program {
    builders::program(vec![
        builders::labelled(
            builders::label("Spawn subagent with web search", None),
            builders::assign(
                "result",
                await_echo_unwrap(builders::record(vec![("ok", builders::bool_lit(true))])),
            ),
        ),
        builders::finish(builders::var("result")),
    ])
}

/// `process search_test() {` the body of [`labeled_spawn_program`], then
/// `finish result` `}`.
fn labeled_spawn_process_program() -> Program {
    builders::module(
        vec![builders::process(
            "search_test",
            Vec::new(),
            builders::block(vec![
                builders::labelled(
                    builders::label("Spawn subagent with web search", None),
                    builders::assign(
                        "result",
                        await_echo_unwrap(builders::record(vec![("ok", builders::bool_lit(true))])),
                    ),
                ),
                builders::finish(builders::var("result")),
            ]),
        )],
        Vec::new(),
    )
}

/// `@label(title: "First call")` / `first = await tools.echo({ value: "first" })?`
/// / `if true { @label(title: "Selected call") selected = await tools.echo({ value: first })? }`
/// / `else { @label(title: "Skipped call") selected = await tools.echo({ value: "skipped" })? }`
/// / `@label(title: "Finish selected")` / `finish selected`
fn labeled_branch_program() -> Program {
    builders::program(vec![
        builders::labelled(
            builders::label("First call", None),
            builders::assign("first", await_echo_unwrap(builders::string("first"))),
        ),
        builders::if_else(
            builders::bool_lit(true),
            builders::block(vec![builders::labelled(
                builders::label("Selected call", None),
                builders::assign("selected", await_echo_unwrap(builders::var("first"))),
            )]),
            builders::block(vec![builders::labelled(
                builders::label("Skipped call", None),
                builders::assign("selected", await_echo_unwrap(builders::string("skipped"))),
            )]),
        ),
        builders::labelled(
            builders::label("Finish selected", None),
            builders::finish(builders::var("selected")),
        ),
    ])
}

fn loop_container_program() -> Program {
    builders::program(vec![
        builders::for_in(
            "value",
            builders::list(vec![builders::num(1.0)]),
            builders::block(vec![builders::var("value")]),
        ),
        builders::while_loop(builders::bool_lit(false), builders::block(Vec::new())),
        builders::finish(builders::null()),
    ])
}

#[test]
fn label_on_await_assignment_attaches_to_await_instruction() {
    // `@label(title: "Wait for child")` / `result = await handle` / `finish result`
    let program = builders::program(vec![
        builders::labelled(
            builders::label("Wait for child", None),
            builders::assign("result", builders::await_expr(builders::var("handle"))),
        ),
        builders::finish(builders::var("result")),
    ]);
    let surface = runtime_test_environment()
        .with_language_features(crate::LashlangLanguageFeatures::default().with_label_annotations())
        .with_globals(["handle"]);
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    let compiled = crate::testing::harness::compile_linked_main(&linked);
    let await_instruction = compiled
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::AwaitHandle))
        .expect("await handle instruction");

    assert!(
        compiled
            .chunk
            .lashlang_execution_sites
            .get(await_instruction)
            .and_then(Option::as_ref)
            .is_some(),
        "label should attach to the awaited effect instruction"
    );
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ObserveStep)),
        "label on awaited assignment should not emit a standalone observe step"
    );
}

#[test]
fn aggregate_await_record_of_resource_calls_emits_batch_instruction() {
    // `results = await { first: tools.echo({ value: "a" }),`
    // `second: tools.echo({ value: "b" }) }` / `finish results`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "results",
            builders::await_expr(builders::record(vec![
                ("first", echo(builders::string("a"))),
                ("second", echo(builders::string("b"))),
            ])),
        ),
        builders::finish(builders::var("results")),
    ]));
    let listing = compiled_instruction_listing(&compiled);
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceOperationBatch(_))),
        "aggregate await should compile to one batch instruction:\n{listing}"
    );
    assert!(
        !compiled.chunk.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::ResourceCall { .. } | Instruction::ResourceCallUnwrap { .. }
        )),
        "aggregate await should not emit sequential resource calls:\n{listing}"
    );
}

#[test]
fn aggregate_await_list_comprehension_of_resource_calls_emits_list_batch_instruction() {
    // `results = await [tools.echo({ value: id })? for id in ["a", "b"] if id != "c"]`
    // / `finish results`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "results",
            builders::await_expr(builders::comprehension(
                echo_unwrap(builders::var("id")),
                vec![
                    builders::comprehension_for(
                        "id",
                        builders::list(vec![builders::string("a"), builders::string("b")]),
                    ),
                    builders::comprehension_if(builders::binary(
                        builders::var("id"),
                        BinaryOp::NotEqual,
                        builders::string("c"),
                    )),
                ],
            )),
        ),
        builders::finish(builders::var("results")),
    ]));
    let listing = compiled_instruction_listing(&compiled);
    assert_eq!(
        compiled
            .chunk
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::ResourceOperationListBatch(_)))
            .count(),
        1,
        "an awaited comprehension of calls compiles to one list-batch instruction:\n{listing}"
    );
    assert!(
        !compiled.chunk.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::ResourceCall { .. }
                | Instruction::ResourceCallUnwrap { .. }
                | Instruction::AwaitHandle
                | Instruction::AwaitHandleUnwrap
        )),
        "the comprehension leaves must not run as sequential calls or a bare await:\n{listing}"
    );
    assert!(
        listing.contains("resource_operation_list_batch echo argc=1 unwrap=true"),
        "the list batch carries the leaf operation and its `?`:\n{listing}"
    );

    // `results = [await tools.echo({ value: id })? for id in ["a", "b"]]`
    // / `finish results`
    let sequential = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "results",
            builders::comprehension(
                await_echo_unwrap(builders::var("id")),
                vec![builders::comprehension_for(
                    "id",
                    builders::list(vec![builders::string("a"), builders::string("b")]),
                )],
            ),
        ),
        builders::finish(builders::var("results")),
    ]));
    let listing = compiled_instruction_listing(&sequential);
    assert!(
        sequential
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceCallUnwrap { .. })),
        "`[await op(x)? for x in xs]` stays a sequential unwrapped call:\n{listing}"
    );
    assert!(
        !sequential.chunk.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::ResourceOperationListBatch(_) | Instruction::ResourceOperationBatch(_)
        )),
        "the sequential form must not batch:\n{listing}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_nested_resource_calls_reconstructs_shape() {
    // `result = await { outer: [ tools.echo({ value: "a" })?,`
    // `{ inner: tools.echo({ value: "b" })? } ] }` / `finish result`
    let value = exec(builders::program(vec![
        builders::assign(
            "result",
            builders::await_expr(builders::record(vec![(
                "outer",
                builders::list(vec![
                    echo_unwrap(builders::string("a")),
                    builders::record(vec![("inner", echo_unwrap(builders::string("b")))]),
                ]),
            )])),
        ),
        builders::finish(builders::var("result")),
    ]))
    .await
    .expect("program should run");

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    let Value::List(outer) = &record["outer"] else {
        panic!("expected outer list");
    };
    assert_eq!(outer[0], Value::String("a".into()));
    assert_eq!(
        outer[1].as_record().unwrap()["inner"],
        Value::String("b".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_tuple_of_resource_calls_batches_and_reconstructs_tuple() {
    // `result = await (tools.echo({ value: "left" })?, tools.echo({ value: "right" })?)`
    // / `finish result`
    let program = || {
        builders::program(vec![
            builders::assign(
                "result",
                builders::await_expr(builders::tuple(vec![
                    echo_unwrap(builders::string("left")),
                    echo_unwrap(builders::string("right")),
                ])),
            ),
            builders::finish(builders::var("result")),
        ])
    };
    let compiled = compile_program_for_tests(program());
    let listing = compiled_instruction_listing(&compiled);
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceOperationBatch(_))),
        "aggregate tuple await should compile to one batch instruction:\n{listing}"
    );

    let value = exec(program()).await.expect("program should run");
    let Value::Tuple(items) = value else {
        panic!("expected tuple result");
    };
    assert_eq!(
        &items[..],
        [Value::String("left".into()), Value::String("right".into())]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_mixed_pure_values_batch_resource_leaves_and_reconstructs_shape() {
    struct MixedHost {
        batch_len: std::sync::atomic::AtomicUsize,
    }

    impl ExecutionHost for MixedHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperationBatch(batch) => {
                    self.batch_len
                        .store(batch.leaves.len(), std::sync::atomic::Ordering::SeqCst);
                    Ok(AbilityResult::ResourceOperationBatch(
                        batch.answer_in_leaf_order(
                            batch
                                .leaves
                                .iter()
                                .filter_map(crate::ResourceOperationBatchLeaf::operation)
                                .map(|operation| {
                                    ResourceOperationResult::Value(
                                        operation
                                            .args
                                            .first()
                                            .and_then(Value::as_record)
                                            .and_then(|record| record.get("value"))
                                            .cloned()
                                            .unwrap_or(Value::Null),
                                    )
                                })
                                .collect(),
                        ),
                    ))
                }
                AbilityOp::ResourceOperation(_) => Err(ExecutionHostError::new(
                    "mixed aggregate should use the batch host ability",
                )),
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }

    let host = MixedHost {
        batch_len: std::sync::atomic::AtomicUsize::new(0),
    };
    // `label = "cache-miss"` / `result = await { first: tools.echo({ value: "a" })?,`
    // `source: label, nested: [3, tools.echo({ value: "b" })?,`
    // `{ ok: true, total: len([1, 2, 3]) }] }` / `finish result`
    let program = builders::program(vec![
        builders::assign("label", builders::string("cache-miss")),
        builders::assign(
            "result",
            builders::await_expr(builders::record(vec![
                ("first", echo_unwrap(builders::string("a"))),
                ("source", builders::var("label")),
                (
                    "nested",
                    builders::list(vec![
                        builders::num(3.0),
                        echo_unwrap(builders::string("b")),
                        builders::record(vec![
                            ("ok", builders::bool_lit(true)),
                            (
                                "total",
                                builders::builtin(
                                    "len",
                                    vec![builders::list(vec![
                                        builders::num(1.0),
                                        builders::num(2.0),
                                        builders::num(3.0),
                                    ])],
                                ),
                            ),
                        ]),
                    ]),
                ),
            ])),
        ),
        builders::finish(builders::var("result")),
    ]);
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("program should finish");
    };

    assert_eq!(host.batch_len.load(std::sync::atomic::Ordering::SeqCst), 2);
    let record = value.as_record().expect("result record");
    assert_eq!(record["first"], Value::String("a".into()));
    assert_eq!(record["source"], Value::String("cache-miss".into()));
    let Value::List(nested) = &record["nested"] else {
        panic!("nested list");
    };
    assert_eq!(nested[0], Value::Number(3.0));
    assert_eq!(nested[1], Value::String("b".into()));
    let nested_record = nested[2].as_record().expect("nested record");
    assert_eq!(nested_record["ok"], Value::Bool(true));
    assert_eq!(nested_record["total"], Value::Number(3.0));
}

#[test]
fn aggregate_await_effectful_non_tool_leaf_keeps_existing_path() {
    // `result = await { signal: wait_signal("ready"), tool: tools.echo({ value: "x" })? }`
    // / `finish result`
    //
    // FIG-2999 retired `start`, which used to be this test's non-tool leaf;
    // `wait_signal` is the remaining effect that is not a resource operation,
    // so it is the one that must keep the unbatched aggregate-await path.
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "result",
            builders::await_expr(builders::record(vec![
                ("signal", builders::wait_signal("ready")),
                ("tool", echo_unwrap(builders::string("x"))),
            ])),
        ),
        builders::finish(builders::var("result")),
    ]));
    let listing = compiled_instruction_listing(&compiled);
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceOperationBatch(_))),
        "effectful non-tool leaves should keep the existing await path:\n{listing}"
    );
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ProcessWaitSignal { .. })),
        "test should cover a non-tool effect leaf:\n{listing}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_evaluates_arguments_once_in_source_order_before_batch() {
    #[derive(Default)]
    struct OrderHost {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl OrderHost {
        fn echo_value(operation: &ResourceOperation) -> Value {
            operation
                .args
                .first()
                .and_then(Value::as_record)
                .and_then(|record| record.get("value"))
                .cloned()
                .unwrap_or(Value::Null)
        }
    }

    impl ExecutionHost for OrderHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => {
                    let value = Self::echo_value(&operation);
                    self.events.lock_recover().push(format!("single:{value}"));
                    Ok(AbilityResult::Value(value))
                }
                AbilityOp::ResourceOperationBatch(batch) => {
                    let values = batch
                        .leaves
                        .iter()
                        .filter_map(crate::ResourceOperationBatchLeaf::operation)
                        .map(Self::echo_value)
                        .collect::<Vec<_>>();
                    self.events.lock_recover().push(format!(
                        "batch:{}",
                        values
                            .iter()
                            .map(Value::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                    Ok(AbilityResult::ResourceOperationBatch(
                        batch.answer_in_leaf_order(
                            values
                                .into_iter()
                                .map(ResourceOperationResult::Value)
                                .collect(),
                        ),
                    ))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }

    let host = OrderHost::default();
    // `result = await { first: tools.echo({ value: tools.echo({ value: "arg-a" })? })?,`
    // `second: tools.echo({ value: tools.echo({ value: "arg-b" })? })? }` / `finish result`
    let program = builders::program(vec![
        builders::assign(
            "result",
            builders::await_expr(builders::record(vec![
                ("first", echo_unwrap(echo_unwrap(builders::string("arg-a")))),
                (
                    "second",
                    echo_unwrap(echo_unwrap(builders::string("arg-b"))),
                ),
            ])),
        ),
        builders::finish(builders::var("result")),
    ]);
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &host)
        .await
        .expect("program should run");
    assert!(matches!(outcome, ExecutionOutcome::Finished(_)));
    assert_eq!(
        host.events.lock_recover().as_slice(),
        ["single:arg-a", "single:arg-b", "batch:arg-a,arg-b"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_leaf_unwrap_waits_for_all_siblings_then_reports_first_error() {
    struct CountingBatchHost {
        batch_len: std::sync::atomic::AtomicUsize,
    }

    impl ExecutionHost for CountingBatchHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperationBatch(batch) => {
                    self.batch_len
                        .store(batch.leaves.len(), std::sync::atomic::Ordering::SeqCst);
                    Ok(AbilityResult::ResourceOperationBatch(
                        batch.answer_in_leaf_order(
                            batch
                                .leaves
                                .iter()
                                .filter_map(crate::ResourceOperationBatchLeaf::operation)
                                .map(|operation| {
                                    if operation.operation == "err" {
                                        ResourceOperationResult::Error(ExecutionHostError::new(
                                            "boom",
                                        ))
                                    } else {
                                        ResourceOperationResult::Value(Value::String("ok".into()))
                                    }
                                })
                                .collect(),
                        ),
                    ))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unexpected non-batch host ability")),
            }
        }
    }

    let host = CountingBatchHost {
        batch_len: std::sync::atomic::AtomicUsize::new(0),
    };
    // `result = await { bad: tools.err()?, good: tools.echo({ value: "ok" })? }`
    // / `finish result`
    let program = builders::program(vec![
        builders::assign(
            "result",
            builders::await_expr(builders::record(vec![
                (
                    "bad",
                    builders::unwrap(builders::receiver_call(
                        builders::resource(&["tools"]),
                        "err",
                        Vec::new(),
                    )),
                ),
                ("good", echo_unwrap(builders::string("ok"))),
            ])),
        ),
        builders::finish(builders::var("result")),
    ]);
    let mut state = State::new();
    let err = execute_program(&program, &mut state, &host)
        .await
        .expect_err("program should fail after batch completes");
    assert_eq!(host.batch_len.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        err.to_string()
            .contains("`?` unwrapped failed module operation: boom"),
        "{err}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn generic_iterator_loops_cover_range_list_keys_nested_control_and_mutation() {
    // `counts = {}` / `items = ["a", "b", "a"]` / `seen = []` / `total = 0`
    // / `for i in range(0, 5) { if i == 1 { continue } if i == 4 { break }`
    // `total = total + i }`
    // / `for item in items { counts[item] = counts[item] + 1`
    // `seen = push(seen, format("{}:{}", item, counts[item])) }`
    // / `pairs = []`
    // / `for key in keys(counts) { for n in range(0, counts[key]) {`
    // `pairs = pairs + [format("{}{}", key, n)] } }`
    // / `finish { total: total, counts: counts, seen: seen, pairs: pairs }`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign("counts", builders::record(Vec::new())),
        builders::assign(
            "items",
            builders::list(vec![
                builders::string("a"),
                builders::string("b"),
                builders::string("a"),
            ]),
        ),
        builders::assign("seen", builders::list(Vec::new())),
        builders::assign("total", builders::num(0.0)),
        builders::for_in(
            "i",
            builders::builtin("range", vec![builders::num(0.0), builders::num(5.0)]),
            builders::block(vec![
                builders::if_else(
                    builders::binary(builders::var("i"), BinaryOp::Equal, builders::num(1.0)),
                    builders::block(vec![Expr::Continue]),
                    builders::block(Vec::new()),
                ),
                builders::if_else(
                    builders::binary(builders::var("i"), BinaryOp::Equal, builders::num(4.0)),
                    builders::block(vec![Expr::Break]),
                    builders::block(Vec::new()),
                ),
                builders::assign(
                    "total",
                    builders::binary(builders::var("total"), BinaryOp::Add, builders::var("i")),
                ),
            ]),
        ),
        builders::for_in(
            "item",
            builders::var("items"),
            builders::block(vec![
                builders::assign_path(
                    "counts",
                    vec![builders::index_step(builders::var("item"))],
                    builders::binary(
                        builders::index(builders::var("counts"), builders::var("item")),
                        BinaryOp::Add,
                        builders::num(1.0),
                    ),
                ),
                builders::assign(
                    "seen",
                    builders::builtin(
                        "push",
                        vec![
                            builders::var("seen"),
                            builders::builtin(
                                "format",
                                vec![
                                    builders::string("{}:{}"),
                                    builders::var("item"),
                                    builders::index(builders::var("counts"), builders::var("item")),
                                ],
                            ),
                        ],
                    ),
                ),
            ]),
        ),
        builders::assign("pairs", builders::list(Vec::new())),
        builders::for_in(
            "key",
            builders::builtin("keys", vec![builders::var("counts")]),
            builders::block(vec![builders::for_in(
                "n",
                builders::builtin(
                    "range",
                    vec![
                        builders::num(0.0),
                        builders::index(builders::var("counts"), builders::var("key")),
                    ],
                ),
                builders::block(vec![builders::assign(
                    "pairs",
                    builders::binary(
                        builders::var("pairs"),
                        BinaryOp::Add,
                        builders::list(vec![builders::builtin(
                            "format",
                            vec![
                                builders::string("{}{}"),
                                builders::var("key"),
                                builders::var("n"),
                            ],
                        )]),
                    ),
                )]),
            )]),
        ),
        builders::finish(builders::record(vec![
            ("total", builders::var("total")),
            ("counts", builders::var("counts")),
            ("seen", builders::var("seen")),
            ("pairs", builders::var("pairs")),
        ])),
    ]));
    let begin_iterators = compiled
        .chunk
        .code
        .iter()
        .filter(|instruction| {
            matches!(
                instruction,
                Instruction::BeginIter(_) | Instruction::BeginRangeIter { .. }
            )
        })
        .count();
    assert!(
        begin_iterators >= 4,
        "every `for` loop should compile to the generic iterator bytecode, got {begin_iterators}"
    );

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &Host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(Value::Record(record)) = outcome else {
        panic!("expected record result");
    };
    assert_eq!(record["total"], Value::Number(5.0));
    let counts = record["counts"].as_record().expect("counts record");
    assert_eq!(counts["a"], Value::Number(2.0));
    assert_eq!(counts["b"], Value::Number(1.0));
    let Value::List(seen) = &record["seen"] else {
        panic!("seen should be a list");
    };
    assert_eq!(seen.len(), 3);
    let Value::List(pairs) = &record["pairs"] else {
        panic!("pairs should be a list");
    };
    assert_eq!(pairs.len(), 3);
}

#[test]
fn list_comprehension_compiles_to_iterator_and_append_bytecode() {
    // `finish [n * 2 for n in range(0, 4) if n % 2 == 0]`
    let compiled = compile_program_for_tests(builders::program(vec![builders::finish(
        builders::comprehension(
            builders::binary(builders::var("n"), BinaryOp::Multiply, builders::num(2.0)),
            vec![
                builders::comprehension_for(
                    "n",
                    builders::builtin("range", vec![builders::num(0.0), builders::num(4.0)]),
                ),
                builders::comprehension_if(builders::binary(
                    builders::binary(builders::var("n"), BinaryOp::Modulo, builders::num(2.0)),
                    BinaryOp::Equal,
                    builders::num(0.0),
                )),
            ],
        ),
    )]));
    let listing = compiled_instruction_listing(&compiled);

    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::BeginRangeIter { .. })),
        "range comprehension should use iterator bytecode:\n{listing}"
    );
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ListAppend)),
        "comprehension should append into the result list directly:\n{listing}"
    );
}

// `every_container_insertion_lowering_emits_value_isolation` was deleted with
// the Lashlang value-isolation opcodes it pinned (`DeepCopy`,
// `DeepCopyLoopBinding`). TypeScript is the sole RLM dialect (ADR 0096), so
// container insertion shares heap references by ECMA rules and there is no
// isolation lowering left to assert.
#[test]
fn effectful_loop_bodies_compile_to_generic_iterator_bytecode() {
    // `items = [1, 2]` / `for item in items { print item }` / `finish null`
    let program = builders::program(vec![
        builders::assign(
            "items",
            builders::list(vec![builders::num(1.0), builders::num(2.0)]),
        ),
        builders::for_in(
            "item",
            builders::var("items"),
            builders::block(vec![builders::print(builders::var("item"))]),
        ),
        builders::finish(builders::null()),
    ]);
    let compiled = compile_program(&program);
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::BeginIter(_))),
        "effectful loops should use the generic iterator bytecode"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn constant_propagation_does_not_cross_control_flow_boundaries() {
    // `x = 1` / `if false { x = 2 }` / `y = x + 1` / `finish y`
    let value = exec(builders::program(vec![
        builders::assign("x", builders::num(1.0)),
        builders::if_else(
            builders::bool_lit(false),
            builders::block(vec![builders::assign("x", builders::num(2.0))]),
            builders::block(Vec::new()),
        ),
        builders::assign(
            "y",
            builders::binary(builders::var("x"), BinaryOp::Add, builders::num(1.0)),
        ),
        builders::finish(builders::var("y")),
    ]))
    .await
    .expect("program should succeed");

    assert_eq!(value, Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn reusable_execution_scratch_preserves_results_across_runs() {
    // `items = [1, 2, 3]` / `total = 0`
    // / `for item in items { total = total + item }` / `finish total`
    let program = builders::program(vec![
        builders::assign(
            "items",
            builders::list(vec![
                builders::num(1.0),
                builders::num(2.0),
                builders::num(3.0),
            ]),
        ),
        builders::assign("total", builders::num(0.0)),
        builders::for_in(
            "item",
            builders::var("items"),
            builders::block(vec![builders::assign(
                "total",
                builders::binary(builders::var("total"), BinaryOp::Add, builders::var("item")),
            )]),
        ),
        builders::finish(builders::var("total")),
    ]);
    let compiled = compile_program(&program);
    let mut scratch = ExecutionScratch::new();

    for _ in 0..3 {
        let mut state = State::new();
        let outcome = execute_compiled_with_scratch(&compiled, &mut state, &Host, &mut scratch)
            .await
            .expect("program should run");
        assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(6.0)));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_and_undefined_variable_are_reported() {
    let outcome = exec_outcome(builders::program(vec![builders::assign(
        "x",
        builders::num(1.0),
    )]))
    .await
    .expect("missing finish should continue");
    assert_eq!(outcome, ExecutionOutcome::Continued);

    // The `ParseError::MissingFinishValue` assertion that stood here was a
    // refusal of the retired dialect's parser; `Expr::Finish` always carries a
    // value, so the AST cannot express the rejected program.

    let err = exec(finish_program(builders::var("x")))
        .await
        .expect_err("undefined variable should fail");
    assert_eq!(
        err,
        RuntimeError::UndefinedVariable {
            name: "x".to_string()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn condition_and_iteration_errors_are_reported() {
    // `if <condition> { finish 1 } else { finish 2 }`
    let branch_on = |condition: Expr| {
        builders::program(vec![builders::if_else(
            condition,
            builders::block(vec![builders::finish(builders::num(1.0))]),
            builders::block(vec![builders::finish(builders::num(2.0))]),
        )])
    };

    let value = exec(branch_on(builders::num(1.0)))
        .await
        .expect("numeric truthiness should be accepted");
    assert_eq!(value, Value::Number(1.0));

    let value = exec(branch_on(builders::string("")))
        .await
        .expect("empty string should be falsy");
    assert_eq!(value, Value::Number(2.0));

    // `for x in 1 { finish x }`
    let err = exec(builders::program(vec![builders::for_in(
        "x",
        builders::num(1.0),
        builders::block(vec![builders::finish(builders::var("x"))]),
    )]))
    .await
    .expect_err("non-list iteration should fail");
    assert_eq!(err, RuntimeError::NonListIteration);
}

#[tokio::test(flavor = "current_thread")]
async fn stmt_call_and_tool_results_cover_success_and_error() {
    // `await tools.echo({ value: 1 })` / `finish 1`
    exec(builders::program(vec![
        builders::await_expr(echo(builders::num(1.0))),
        builders::finish(builders::num(1.0)),
    ]))
    .await
    .expect("statement module operation should succeed");

    // `bad = await tools.missing({})` / `finish bad`
    let missing = exec(builders::program(vec![
        builders::assign(
            "bad",
            builders::await_expr(builders::receiver_call(
                builders::resource(&["tools"]),
                "missing",
                vec![builders::record(Vec::new())],
            )),
        ),
        builders::finish(builders::var("bad")),
    ]))
    .await
    .expect("missing module operation should be wrapped");
    assert_eq!(
        missing.as_record().expect("result should be a record")["ok"],
        Value::Bool(false)
    );

    // `ok = await tools.echo({ value: 7 })` / `bad = await tools.err({})`
    // / `finish { ok: ok, bad: bad }`
    let value = exec(builders::program(vec![
        builders::assign("ok", builders::await_expr(echo(builders::num(7.0)))),
        builders::assign("bad", builders::await_expr(err_call())),
        builders::finish(builders::record(vec![
            ("ok", builders::var("ok")),
            ("bad", builders::var("bad")),
        ])),
    ]))
    .await
    .expect("module operation program should succeed");
    let record = value.as_record().expect("expected record");
    assert_eq!(record["ok"].as_record().unwrap()["ok"], Value::Bool(true));
    assert_eq!(record["bad"].as_record().unwrap()["ok"], Value::Bool(false));
}

#[tokio::test(flavor = "current_thread")]
async fn result_unwrap_extracts_success_and_preserves_manual_handling() {
    // `finish (await tools.echo({ value: 7 })?)`
    let value = exec(finish_program(await_echo_unwrap(builders::num(7.0))))
        .await
        .expect("unwrap should succeed");
    assert_eq!(value, Value::Number(7.0));

    // `result = await tools.err({})`
    // / `finish result.ok ? result.error : "unexpected"`
    let value = exec(builders::program(vec![
        builders::assign("result", builders::await_expr(err_call())),
        builders::finish(builders::if_else(
            builders::field(builders::var("result"), "ok"),
            builders::field(builders::var("result"), "error"),
            builders::string("unexpected"),
        )),
    ]))
    .await
    .expect("manual wrapper handling should still work");
    assert_eq!(value, Value::String("unexpected".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn unparenthesized_await_module_operation_unwrap_skips_handle_await() {
    // `value = await tools.echo({ value: 7 })?` / `finish value`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign("value", await_echo_unwrap(builders::num(7.0))),
        builders::finish(builders::var("value")),
    ]));
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));
}

#[tokio::test(flavor = "current_thread")]
async fn parenthesized_await_module_operation_unwrap_skips_handle_await() {
    // `value = (await tools.echo({ value: 7 }))?` / `finish value`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "value",
            builders::unwrap(builders::await_expr(echo(builders::num(7.0)))),
        ),
        builders::finish(builders::var("value")),
    ]));
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_await_module_operation_unwrap_skips_handle_await() {
    // `@label(title: "Echo")` / `value = await tools.echo({ value: { answer: "ok" } })?`
    // / `finish value`
    let compiled = compile_labeled_program(builders::program(vec![
        builders::labelled(
            builders::label("Echo", None),
            builders::assign(
                "value",
                await_echo_unwrap(builders::record(vec![("answer", builders::string("ok"))])),
            ),
        ),
        builders::finish(builders::var("value")),
    ]));
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finished outcome");
    };
    let record = value
        .as_record()
        .expect("finished value should be a record");
    assert_eq!(record["answer"], Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_parenthesized_await_module_operation_unwrap_skips_handle_await() {
    // `@label(title: "Echo")` / `value = (await tools.echo({ value: { answer: "ok" } }))?`
    // / `finish value`
    let compiled = compile_labeled_program(builders::program(vec![
        builders::labelled(
            builders::label("Echo", None),
            builders::assign(
                "value",
                builders::unwrap(builders::await_expr(echo(builders::record(vec![(
                    "answer",
                    builders::string("ok"),
                )])))),
            ),
        ),
        builders::finish(builders::var("value")),
    ]));
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finished outcome");
    };
    let record = value
        .as_record()
        .expect("finished value should be a record");
    assert_eq!(record["answer"], Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_process_await_module_operation_unwrap_skips_handle_await() {
    // `process echo_from_process() { @label(title: "Echo")`
    // `value = await tools.echo({ value: { answer: "ok" } })?` / `finish value }`
    let compiled = compile_labeled_process_program(
        builders::module(
            vec![builders::process(
                "echo_from_process",
                Vec::new(),
                builders::block(vec![
                    builders::labelled(
                        builders::label("Echo", None),
                        builders::assign(
                            "value",
                            await_echo_unwrap(builders::record(vec![(
                                "answer",
                                builders::string("ok"),
                            )])),
                        ),
                    ),
                    builders::finish(builders::var("value")),
                ]),
            )],
            Vec::new(),
        ),
        "echo_from_process",
    );
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled_process(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("process should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finished outcome");
    };
    let record = value
        .as_record()
        .expect("finished value should be a record");
    assert_eq!(record["answer"], Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn direct_module_operation_unwrap_skips_observable_wrapper() {
    // `finish (await tools.echo({ value: 7 })?)`
    let compiled = compile_program_for_tests(finish_program(await_echo_unwrap(builders::num(7.0))));
    assert_resource_call_unwrap_without_handle_await(&compiled);
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResultUnwrap))
    );

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));

    // `finish (await tools.err({})?)`
    let err = exec(finish_program(builders::await_expr(builders::unwrap(
        err_call(),
    ))))
    .await
    .expect_err("failed unwrap should abort");
    assert_eq!(
        err,
        RuntimeError::UnwrappedModuleOperationFailed {
            source: ExecutionHostError::new("boom"),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn result_unwrap_reports_failed_and_malformed_wrappers() {
    // `finish (await tools.err({})?)`
    let err = exec(finish_program(builders::await_expr(builders::unwrap(
        err_call(),
    ))))
    .await
    .expect_err("failed module operation unwrap should abort");
    assert_eq!(
        err,
        RuntimeError::UnwrappedModuleOperationFailed {
            source: ExecutionHostError::new("boom"),
        }
    );

    let err = exec(finish_program(builders::unwrap(builders::num(1.0))))
        .await
        .expect_err("non-wrapper should fail");
    assert_eq!(
        err,
        RuntimeError::ToolResultExpected {
            actual: "number".to_string(),
        }
    );

    // `finish { ok: true }?`
    let err = exec(finish_program(builders::unwrap(builders::record(vec![(
        "ok",
        builders::bool_lit(true),
    )]))))
    .await
    .expect_err("missing value should fail");
    assert_eq!(err, RuntimeError::ToolResultMissingValue);
}

#[tokio::test(flavor = "current_thread")]
async fn field_index_unary_and_boolean_paths_are_covered() {
    // `rec = { nested: { name: "lash" } }` / `xs = ["a", "b"]`
    // / `ok = false and missing` / `alt = true or missing`
    // / `finish [rec.nested.name, xs[1], "abc"[2], -1, not false, !false, ok, alt]`
    let value = exec(builders::program(vec![
        builders::assign(
            "rec",
            builders::record(vec![(
                "nested",
                builders::record(vec![("name", builders::string("lash"))]),
            )]),
        ),
        builders::assign(
            "xs",
            builders::list(vec![builders::string("a"), builders::string("b")]),
        ),
        builders::assign(
            "ok",
            builders::binary(
                builders::bool_lit(false),
                BinaryOp::And,
                builders::var("missing"),
            ),
        ),
        builders::assign(
            "alt",
            builders::binary(
                builders::bool_lit(true),
                BinaryOp::Or,
                builders::var("missing"),
            ),
        ),
        builders::finish(builders::list(vec![
            builders::field(builders::field(builders::var("rec"), "nested"), "name"),
            builders::index(builders::var("xs"), builders::num(1.0)),
            builders::index(builders::string("abc"), builders::num(2.0)),
            builders::unary(crate::ast::UnaryOp::Negate, builders::num(1.0)),
            builders::unary(crate::ast::UnaryOp::Not, builders::bool_lit(false)),
            builders::unary(crate::ast::UnaryOp::Not, builders::bool_lit(false)),
            builders::var("ok"),
            builders::var("alt"),
        ])),
    ]))
    .await
    .expect("program should succeed");

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("lash".to_string().into()),
                Value::String("b".to_string().into()),
                Value::String("c".to_string().into()),
                Value::Number(-1.0),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
            ]
            .into()
        )
    );

    // `finish true and false`
    let value = exec(finish_binary(
        builders::bool_lit(true),
        BinaryOp::And,
        builders::bool_lit(false),
    ))
    .await
    .expect("and path should succeed");
    assert_eq!(value, Value::Bool(false));

    // `finish false or true`
    let value = exec(finish_binary(
        builders::bool_lit(false),
        BinaryOp::Or,
        builders::bool_lit(true),
    ))
    .await
    .expect("or path should succeed");
    assert_eq!(value, Value::Bool(true));
}

/// Member and index reads follow ECMA reference semantics (ADR 0096): a missing
/// property or an out-of-range index is `undefined` rather than a typed refusal
/// or a null, and only a read *through* `undefined` throws. The refusals this
/// test pinned before the cutover -- `CannotReadField` on a number, `CannotIndex`
/// on a non-container, `InvalidIndex` on a fractional subscript, and the
/// from-the-end negative index -- were Lashlang value semantics and are gone
/// with the dialect.
#[tokio::test(flavor = "current_thread")]
async fn field_index_and_type_errors_are_covered() {
    for (source, program, expected) in [
        (
            "n = 1 finish n.name",
            builders::program(vec![
                builders::assign("n", builders::num(1.0)),
                builders::finish(builders::field(builders::var("n"), "name")),
            ]),
            Value::Undefined,
        ),
        (
            "rec = {} finish rec.name",
            builders::program(vec![
                builders::assign("rec", builders::record(Vec::new())),
                builders::finish(builders::field(builders::var("rec"), "name")),
            ]),
            Value::Undefined,
        ),
        (
            "finish 1[0]",
            finish_program(builders::index(builders::num(1.0), builders::num(0.0))),
            Value::Undefined,
        ),
        (
            "finish [1][2]",
            finish_program(builders::index(
                builders::list(vec![builders::num(1.0)]),
                builders::num(2.0),
            )),
            Value::Undefined,
        ),
        (
            "finish \"a\"[2]",
            finish_program(builders::index(builders::string("a"), builders::num(2.0))),
            Value::Undefined,
        ),
        (
            "finish [1][1.5]",
            finish_program(builders::index(
                builders::list(vec![builders::num(1.0)]),
                builders::num(1.5),
            )),
            Value::Undefined,
        ),
        (
            "finish [1][-1]",
            finish_program(builders::index(
                builders::list(vec![builders::num(1.0)]),
                builders::unary(crate::ast::UnaryOp::Negate, builders::num(1.0)),
            )),
            Value::Undefined,
        ),
        (
            "finish not 1",
            finish_program(builders::unary(
                crate::ast::UnaryOp::Not,
                builders::num(1.0),
            )),
            Value::Bool(false),
        ),
        (
            "finish not 0",
            finish_program(builders::unary(
                crate::ast::UnaryOp::Not,
                builders::num(0.0),
            )),
            Value::Bool(true),
        ),
    ] {
        assert_eq!(
            exec(program).await.expect("program should run"),
            expected,
            "{source}"
        );
    }

    // `rec = { ok: false }` / `finish len(rec.value.items)`
    let err = exec(builders::program(vec![
        builders::assign(
            "rec",
            builders::record(vec![("ok", builders::bool_lit(false))]),
        ),
        builders::finish(builders::builtin(
            "len",
            vec![builders::field(
                builders::field(builders::var("rec"), "value"),
                "items",
            )],
        )),
    ]))
    .await
    .expect_err("reading a field through undefined should throw");
    assert!(
        matches!(
            &err,
            RuntimeError::CannotReadField { field, actual } if field == "items" && actual == "undefined"
        ),
        "{err:?}"
    );
}

#[test]
fn existing_execution_site_ids_are_unchanged() {
    let fixtures = [
        (
            labeled_spawn_program(),
            &[
                "node:1de6eca7fbb5c02fa3b32d47",
                "node:e5da609e1296a1d8e31ae439",
            ][..],
            "9e286714722511639d33657596b77b6d0c09d2a074ffbe2e801cb29dd5448e37",
        ),
        (
            labeled_branch_program(),
            &[
                "node:1de6eca7fbb5c02fa3b32d47",
                "node:e5da609e1296a1d8e31ae439",
                "node:9abd5c00875b00cb66b8eace",
                "node:c0542a493d0c724cb4fc8881",
                "node:5d5c395c7ff475d867aea1a7",
            ],
            "e228cedab83aeeb459fe65926fefdbe25f168a372cdb549522ed4e9394092929",
        ),
        (
            loop_container_program(),
            &[
                "node:1de6eca7fbb5c02fa3b32d47",
                "node:e5da609e1296a1d8e31ae439",
                "node:5d5c395c7ff475d867aea1a7",
            ][..],
            "0000000000000000000000000000000000000000000000000000000000000001",
        ),
    ];

    for (program, expected, historical_module_hash) in fixtures {
        let (current, historical) =
            compile_labeled_program_with_historical_context(program, historical_module_hash);
        assert_eq!(
            compiled_site_descriptors(&current),
            compiled_site_descriptors(&historical),
            "only the recorded execution context may differ"
        );
        assert_eq!(execution_site_ids(&historical), expected);
    }

    let (current, historical) = compile_labeled_process_with_historical_context(
        labeled_spawn_process_program(),
        "search_test",
        "bb95c621902678eb6d160bf6962abc1d69076209a9f9b057b04c591e492415eb",
        "076195cb8c008534063267197f05271d61cca00df9a0fb08dc208881f3047181",
        0,
    );
    assert_eq!(
        compiled_site_descriptors(&current),
        compiled_site_descriptors(&historical),
        "only the recorded execution context may differ"
    );
    // Re-pinned by FIG-3460 because the empty path is now reserved for the
    // non-executable process container. Direct process bodies begin at `[0]`,
    // so both executable child ids move together while remaining independent
    // of the recorded module and process context above.
    assert_eq!(
        execution_site_ids(&historical),
        [
            "node:20b7fff7062b8c5ed89e1c96",
            "node:20adcad7b99b68a910628f1a",
        ]
    );
}

#[test]
fn aggregate_resource_sites_share_their_structural_node() {
    // `result = await (tools.echo({ value: "left" })?, tools.echo({ value: "right" })?)`
    // / `finish result`
    let tuple = compile_labeled_program(builders::program(vec![
        builders::assign(
            "result",
            builders::await_expr(builders::tuple(vec![
                echo_unwrap(builders::string("left")),
                echo_unwrap(builders::string("right")),
            ])),
        ),
        builders::finish(builders::var("result")),
    ]));
    let tuple_sites = tuple.chunk.resource_operation_batches[0]
        .leaves
        .iter()
        .map(|leaf| leaf.site.as_ref().expect("tuple batch leaf site"))
        .collect::<Vec<_>>();
    assert_eq!(
        tuple_sites
            .iter()
            .map(|site| site.workflow_site.path.clone())
            .collect::<Vec<_>>(),
        [vec![0], vec![0]]
    );
    assert!(tuple_sites.iter().all(|site| {
        site.node_kind == lash_sansio::ExecutionNodeKind::ResourceOperation && site.label == "echo"
    }));
    assert_eq!(
        tuple_sites[0].node_id, tuple_sites[1].node_id,
        "aggregate leaves are occurrences of one authored workflow node"
    );

    // `results = await [tools.echo({ value: id })? for id in ["a", "b"] if id != "c"]`
    // / `finish results`
    let list = compile_labeled_program(builders::program(vec![
        builders::assign(
            "results",
            builders::await_expr(builders::comprehension(
                echo_unwrap(builders::var("id")),
                vec![
                    builders::comprehension_for(
                        "id",
                        builders::list(vec![builders::string("a"), builders::string("b")]),
                    ),
                    builders::comprehension_if(builders::binary(
                        builders::var("id"),
                        BinaryOp::NotEqual,
                        builders::string("c"),
                    )),
                ],
            )),
        ),
        builders::finish(builders::var("results")),
    ]));
    let list_site = list.chunk.resource_operation_list_batches[0]
        .site
        .as_ref()
        .expect("list batch site");
    assert_eq!(list_site.workflow_site.path, [0]);
    assert_eq!(
        (list_site.node_kind.as_str(), list_site.label.as_str()),
        ("resource_operation", "echo")
    );
}

fn execution_site_ids(compiled: &CompiledProgram) -> Vec<&str> {
    compiled
        .chunk
        .lashlang_execution_sites
        .iter()
        .flatten()
        .map(|site| site.node_id.as_str())
        .collect()
}

fn compiled_site_descriptors(compiled: &CompiledProgram) -> Vec<(String, String, Vec<u32>)> {
    let mut sites = compiled
        .chunk
        .lashlang_execution_sites
        .iter()
        .flatten()
        .map(|site| {
            (
                site.node_kind.to_string(),
                site.label.clone(),
                site.workflow_site.path.clone(),
            )
        })
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.2.cmp(&right.2));
    sites
}

fn compile_labeled_program_with_historical_context(
    program: Program,
    historical_module_hash: &str,
) -> (CompiledProgram, CompiledProgram) {
    let surface = runtime_test_environment().with_language_features(
        crate::LashlangLanguageFeatures::default().with_label_annotations(),
    );
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    let current = crate::testing::harness::compile_linked_main(&linked);
    let mut historical_context = crate::artifact::CompiledModuleContext::from(&linked.artifact);
    historical_context.module_ref = historical_module_ref(historical_module_hash);
    let (chunk, compile_stats) = Compiler::compile_linked_program(
        linked.artifact.ir(),
        Default::default(),
        historical_context,
        crate::tracking::LashlangExecutionContext::main(),
    );
    let historical = CompiledProgram {
        chunk,
        compile_stats,
    };
    (current, historical)
}

fn compile_labeled_process_with_historical_context(
    program: Program,
    process_name: &str,
    historical_module_hash: &str,
    historical_process_component: &str,
    historical_process_position: u32,
) -> (CompiledProgram, CompiledProgram) {
    let surface = runtime_test_environment().with_language_features(
        crate::LashlangLanguageFeatures::default().with_label_annotations(),
    );
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    let current = crate::testing::harness::compile_linked_process_named(&linked, process_name)
        .expect("current process should compile");
    let process = linked
        .artifact
        .ir()
        .process(process_name)
        .expect("historical process should exist");
    let process_program = Program {
        language: linked.artifact.ir().language.clone(),
        declarations: linked.artifact.ir().declarations.clone(),
        main: process.body.clone(),
        private_bindings: Default::default(),
        spans: Default::default(),
    };
    let mut historical_context = crate::artifact::CompiledModuleContext::from(&linked.artifact);
    historical_context.module_ref = historical_module_ref(historical_module_hash);
    historical_context.process_refs.insert(
        process_name.to_string(),
        crate::ProcessRef::new(
            crate::ContentHash::new(historical_process_component),
            historical_process_position,
        ),
    );
    let (chunk, compile_stats) = Compiler::compile_linked_process_program(
        &process_program,
        Default::default(),
        historical_context,
        crate::tracking::LashlangExecutionContext::process(process_name),
    );
    let historical = CompiledProgram {
        chunk,
        compile_stats,
    };
    (current, historical)
}

fn historical_module_ref(hash: &str) -> crate::ModuleRef {
    crate::ModuleRef::new(&crate::ContentHash::new(hash))
}
