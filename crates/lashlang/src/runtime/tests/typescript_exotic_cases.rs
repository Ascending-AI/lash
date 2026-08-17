fn private_builtin(name: &str, args: Vec<Expr>) -> Expr {
    Expr::BuiltinCall {
        name: name.into(),
        args,
    }
}

fn ts_assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: crate::AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

fn heap_new(kind: &str, args: Vec<Expr>) -> Expr {
    private_builtin(
        "__typescript_heap_new",
        std::iter::once(Expr::String(kind.into()))
            .chain(args)
            .collect(),
    )
}

fn heap_method(method: &str, receiver: &str, args: Vec<Expr>) -> Expr {
    private_builtin(
        "__typescript_stdlib",
        std::iter::once(Expr::String(method.into()))
            .chain(std::iter::once(Expr::Variable(receiver.into())))
            .chain(args)
            .collect(),
    )
}

fn field(target: &str, name: &str) -> Expr {
    Expr::Field {
        target: Box::new(Expr::Variable(target.into())),
        field: name.into(),
    }
}

async fn run_typescript_ast_across_every_effect(program: Program) -> ExecutionOutcome {
    let compiled = compile_ast_with_dialect(&program, CompilationDialect::Typescript)
        .expect("compile TypeScript substrate AST");
    let mut state = State::new();
    let mut vm = Vm::from_state(&compiled, &mut state, &Host).expect("install VM state");
    loop {
        match vm
            .run_process_until_effect()
            .await
            .expect("run TypeScript substrate AST")
        {
            VmRunOutcome::EffectCompleted => {
                let continuation = vm.suspend().expect("suspend at effect");
                let encoded = serde_json::to_vec(&continuation).expect("encode continuation");
                let decoded = serde_json::from_slice(&encoded).expect("decode continuation");
                vm = Vm::resume_from(decoded, &compiled, &Host).expect("resume continuation");
            }
            VmRunOutcome::Complete(outcome) => return outcome,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn regexp_last_index_mutation_survives_park_and_restore() {
    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("regexp".into()),
            expr: Box::new(private_builtin(
                "__typescript_heap_new",
                vec![
                    Expr::String("RegExp".into()),
                    Expr::String("a+".into()),
                    Expr::String("ig".into()),
                ],
            )),
        },
        Expr::Assign {
            target: crate::AssignTarget {
                root: "regexp".into(),
                steps: vec![crate::AssignPathStep::Field("lastIndex".into())],
            },
            expr: Box::new(Expr::Number(9.0)),
        },
        Expr::Print(Box::new(Expr::String("park".into()))),
        Expr::Finish(Box::new(Expr::Field {
            target: Box::new(Expr::Variable("regexp".into())),
            field: "lastIndex".into(),
        })),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::Number(9.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn url_search_params_live_link_and_order_survive_park_and_restore() {
    let program = Program::block(vec![
        ts_assign(
            "url",
            heap_new(
                "URL",
                vec![Expr::String("https://example.test/?b=2&a=1&a=2".into())],
            ),
        ),
        ts_assign("params", field("url", "searchParams")),
        heap_method(
            "append",
            "params",
            vec![Expr::String("a".into()), Expr::String("3".into())],
        ),
        Expr::Print(Box::new(Expr::String("park linked URL state".into()))),
        Expr::Finish(Box::new(Expr::List(vec![
            field("url", "href"),
            heap_method("toString", "params", Vec::new()),
            Expr::JavaScriptBinary {
                left: Box::new(Expr::Variable("params".into())),
                op: crate::JavaScriptBinaryOp::StrictEqual,
                right: Box::new(field("url", "searchParams")),
            },
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::String("https://example.test/?b=2&a=1&a=2&a=3".into()),
                Value::String("b=2&a=1&a=2&a=3".into()),
                Value::Bool(true),
            ]
            .into(),
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn url_search_params_live_link_survives_state_snapshot_round_trip() {
    let setup = Program::block(vec![
        ts_assign(
            "url",
            heap_new(
                "URL",
                vec![Expr::String("https://example.test/?a=1&a=2".into())],
            ),
        ),
        ts_assign("params", field("url", "searchParams")),
        heap_method(
            "append",
            "params",
            vec![Expr::String("b".into()), Expr::String("3".into())],
        ),
        Expr::Finish(Box::new(Expr::Null)),
    ]);
    let setup = compile_ast_with_dialect(&setup, CompilationDialect::Typescript)
        .expect("compile URL snapshot setup");
    let mut state = State::new();
    execute(&setup, &mut state, &Host)
        .await
        .expect("persist URL globals");
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("encode URL snapshot");
    let snapshot = Snapshot::from_canonical_bytes(&bytes).expect("decode URL snapshot");
    let mut state = State::from_snapshot(snapshot);
    let query = Program::block(vec![Expr::Finish(Box::new(Expr::List(vec![
        field("url", "href"),
        heap_method("toString", "params", Vec::new()),
        Expr::JavaScriptBinary {
            left: Box::new(Expr::Variable("params".into())),
            op: crate::JavaScriptBinaryOp::StrictEqual,
            right: Box::new(field("url", "searchParams")),
        },
    ])))]);
    let query = compile_ast_with_dialect(&query, CompilationDialect::Typescript)
        .expect("compile URL snapshot query");
    assert_eq!(
        execute(&query, &mut state, &Host)
            .await
            .expect("query restored URL globals"),
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::String("https://example.test/?a=1&a=2&b=3".into()),
                Value::String("a=1&a=2&b=3".into()),
                Value::Bool(true),
            ]
            .into(),
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn regexp_last_index_uses_heap_aware_to_number_and_to_length() {
    let mut expressions = Vec::new();
    for (suffix, value) in [
        ("list", Expr::List(vec![Expr::Number(1.0)])),
        ("empty", Expr::List(Vec::new())),
        (
            "many",
            Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)]),
        ),
        ("record", Expr::Record(Vec::new())),
        ("map", heap_new("Map", Vec::new())),
        ("date", heap_new("Date", vec![Expr::Number(42.0)])),
        ("infinity", Expr::Number(f64::INFINITY)),
    ] {
        let regexp = format!("regexp_{suffix}");
        expressions.push(ts_assign(
            &regexp,
            heap_new(
                "RegExp",
                vec![Expr::String("a+".into()), Expr::String("g".into())],
            ),
        ));
        expressions.push(Expr::Assign {
            target: crate::AssignTarget {
                root: regexp.clone().into(),
                steps: vec![crate::AssignPathStep::Field("lastIndex".into())],
            },
            expr: Box::new(value),
        });
    }
    expressions.push(Expr::Finish(Box::new(Expr::List(
        ["list", "empty", "many", "record", "map", "date", "infinity"]
            .into_iter()
            .map(|suffix| field(&format!("regexp_{suffix}"), "lastIndex"))
            .collect(),
    ))));
    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(expressions)).await,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::Number(1.0),
                Value::Number(0.0),
                Value::Number(0.0),
                Value::Number(0.0),
                Value::Number(0.0),
                Value::Number(42.0),
                Value::Number(crate::runtime::heap::MAX_JAVASCRIPT_LENGTH as f64),
            ]
            .into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reference_index_keys_use_heap_aware_string_coercion() {
    let program = Program::block(vec![
        ts_assign(
            "record",
            Expr::Record(vec![
                ("a".into(), Expr::Number(1.0)),
                ("1,2,3".into(), Expr::Number(12.0)),
            ]),
        ),
        ts_assign("object_key", Expr::Record(Vec::new())),
        ts_assign("array", Expr::List(vec![Expr::Number(1.0)])),
        ts_assign("empty_list_key", Expr::List(Vec::new())),
        ts_assign("map", heap_new("Map", Vec::new())),
        ts_assign(
            "nested_list_key",
            Expr::List(vec![
                Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)]),
                Expr::Number(3.0),
            ]),
        ),
        Expr::Finish(Box::new(Expr::List(vec![
            Expr::Index {
                target: Box::new(Expr::Variable("record".into())),
                index: Box::new(Expr::Variable("object_key".into())),
            },
            Expr::Index {
                target: Box::new(Expr::Variable("array".into())),
                index: Box::new(Expr::Variable("empty_list_key".into())),
            },
            Expr::Index {
                target: Box::new(Expr::Variable("map".into())),
                index: Box::new(Expr::Variable("object_key".into())),
            },
            Expr::Index {
                target: Box::new(Expr::Variable("record".into())),
                index: Box::new(Expr::Variable("nested_list_key".into())),
            },
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::Undefined,
                Value::Undefined,
                Value::Undefined,
                Value::Number(12.0),
            ]
            .into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reference_array_index_keys_are_symmetric_for_reads_and_writes() {
    let program = Program::block(vec![
        ts_assign(
            "array",
            Expr::List(vec![Expr::Record(vec![("x".into(), Expr::Number(1.0))])]),
        ),
        ts_assign("key", Expr::List(vec![Expr::Number(0.0)])),
        ts_assign(
            "before",
            Expr::Field {
                target: Box::new(Expr::Index {
                    target: Box::new(Expr::Variable("array".into())),
                    index: Box::new(Expr::Variable("key".into())),
                }),
                field: "x".into(),
            },
        ),
        Expr::Assign {
            target: crate::AssignTarget {
                root: "array".into(),
                steps: vec![
                    crate::AssignPathStep::Index(Expr::Variable("key".into())),
                    crate::AssignPathStep::Field("x".into()),
                ],
            },
            expr: Box::new(Expr::Number(2.0)),
        },
        ts_assign(
            "after_nested_write",
            Expr::Field {
                target: Box::new(Expr::Index {
                    target: Box::new(Expr::Variable("array".into())),
                    index: Box::new(Expr::Variable("key".into())),
                }),
                field: "x".into(),
            },
        ),
        Expr::Assign {
            target: crate::AssignTarget {
                root: "array".into(),
                steps: vec![crate::AssignPathStep::Index(Expr::Variable("key".into()))],
            },
            expr: Box::new(Expr::Record(vec![("x".into(), Expr::Number(3.0))])),
        },
        Expr::Finish(Box::new(Expr::List(vec![
            Expr::Variable("before".into()),
            Expr::Variable("after_nested_write".into()),
            Expr::Field {
                target: Box::new(Expr::Index {
                    target: Box::new(Expr::Variable("array".into())),
                    index: Box::new(Expr::Variable("key".into())),
                }),
                field: "x".into(),
            },
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0),].into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reference_object_key_nested_array_write_returns_a_deterministic_error() {
    let program = Program::block(vec![
        ts_assign(
            "array",
            Expr::List(vec![Expr::Record(vec![("x".into(), Expr::Number(1.0))])]),
        ),
        ts_assign("key", Expr::Record(Vec::new())),
        Expr::Assign {
            target: crate::AssignTarget {
                root: "array".into(),
                steps: vec![
                    crate::AssignPathStep::Index(Expr::Variable("key".into())),
                    crate::AssignPathStep::Field("x".into()),
                ],
            },
            expr: Box::new(Expr::Number(2.0)),
        },
    ]);
    let compiled = compile_ast_with_dialect(&program, CompilationDialect::Typescript)
        .expect("compile object index-key assignment regression");
    assert_eq!(
        execute(&compiled, &mut State::new(), &Host)
            .await
            .expect_err("object-key nested array assignment must fail"),
        RuntimeError::TypeScriptArrayNonIndexPropertyUnsupported {
            key: "[object Object]".into(),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn date_reference_index_key_uses_the_pending_string_coercion_error() {
    let program = Program::block(vec![
        ts_assign("record", Expr::Record(Vec::new())),
        ts_assign("date_key", heap_new("Date", vec![Expr::Number(42.0)])),
        Expr::Finish(Box::new(Expr::Index {
            target: Box::new(Expr::Variable("record".into())),
            index: Box::new(Expr::Variable("date_key".into())),
        })),
    ]);
    let compiled = compile_ast_with_dialect(&program, CompilationDialect::Typescript)
        .expect("compile Date index-key coercion regression");
    let error = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect_err("Date index-key coercion remains a loud deviation");
    assert!(
        error
            .to_string()
            .contains("TS_DATE_STRING_COERCION_PENDING"),
        "{error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn regexp_string_coercion_matches_node() {
    let program = Program::block(vec![
        ts_assign(
            "regexp",
            heap_new(
                "RegExp",
                vec![Expr::String("a+".into()), Expr::String("g".into())],
            ),
        ),
        ts_assign(
            "empty_regexp",
            heap_new(
                "RegExp",
                vec![
                    Expr::String(String::new().into()),
                    Expr::String(String::new().into()),
                ],
            ),
        ),
        Expr::Finish(Box::new(Expr::List(vec![
            Expr::JavaScriptBinary {
                left: Box::new(Expr::String(String::new().into())),
                op: crate::JavaScriptBinaryOp::Add,
                right: Box::new(Expr::Variable("regexp".into())),
            },
            heap_method("toString", "regexp", Vec::new()),
            heap_method("toString", "empty_regexp", Vec::new()),
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::String("/a+/g".into()),
                Value::String("/a+/g".into()),
                Value::String("/(?:)/".into()),
            ]
            .into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn date_to_string_uses_the_pending_string_coercion_error() {
    let program = Program::block(vec![
        ts_assign("date", heap_new("Date", vec![Expr::Number(42.0)])),
        Expr::Finish(Box::new(heap_method("toString", "date", Vec::new()))),
    ]);
    let compiled = compile_ast_with_dialect(&program, CompilationDialect::Typescript)
        .expect("compile Date toString regression");
    let error = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect_err("Date toString remains a loud deviation");
    assert!(
        error
            .to_string()
            .contains("TS_DATE_STRING_COERCION_PENDING"),
        "{error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn javascript_unary_plus_and_minus_use_exact_reference_to_number() {
    let mut expressions = Vec::new();
    for (name, value) in [
        ("one", Expr::List(vec![Expr::Number(1.0)])),
        ("empty", Expr::List(Vec::new())),
        (
            "many",
            Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)]),
        ),
        ("record", Expr::Record(Vec::new())),
        ("map", heap_new("Map", Vec::new())),
        ("date", heap_new("Date", vec![Expr::Number(42.0)])),
    ] {
        expressions.push(ts_assign(name, value));
    }
    expressions.push(Expr::Finish(Box::new(Expr::List(
        [
            "one", "empty", "many", "record", "map", "date", "one", "empty", "many", "record",
            "map", "date",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, name)| Expr::JavaScriptUnary {
            op: if index < 6 {
                crate::JavaScriptUnaryOp::Plus
            } else {
                crate::JavaScriptUnaryOp::Negate
            },
            expr: Box::new(Expr::Variable(name.into())),
        })
        .collect(),
    ))));

    let ExecutionOutcome::Finished(Value::List(values)) =
        run_typescript_ast_across_every_effect(Program::block(expressions)).await
    else {
        panic!("unary reference coercions should finish as a list")
    };
    assert_eq!(values[0], Value::Number(1.0));
    assert_eq!(values[1], Value::Number(0.0));
    assert!(matches!(values[2], Value::Number(value) if value.is_nan()));
    assert!(matches!(values[3], Value::Number(value) if value.is_nan()));
    assert!(matches!(values[4], Value::Number(value) if value.is_nan()));
    assert_eq!(values[5], Value::Number(42.0));
    assert_eq!(values[6], Value::Number(-1.0));
    assert!(matches!(values[7], Value::Number(value) if value.to_bits() == (-0.0_f64).to_bits()));
    assert!(matches!(values[8], Value::Number(value) if value.is_nan()));
    assert!(matches!(values[9], Value::Number(value) if value.is_nan()));
    assert!(matches!(values[10], Value::Number(value) if value.is_nan()));
    assert_eq!(values[11], Value::Number(-42.0));
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "heap references must be exported before truthiness")]
fn scalar_truthiness_asserts_on_unexported_references() {
    is_truthy(&Value::Ref(HeapId::from_counter(1)));
}

#[tokio::test(flavor = "current_thread")]
async fn nested_regexp_last_index_write_survives_park_and_restore() {
    let program = Program::block(vec![
        ts_assign(
            "holder",
            Expr::Record(vec![(
                "re".into(),
                heap_new(
                    "RegExp",
                    vec![Expr::String("a+".into()), Expr::String("g".into())],
                ),
            )]),
        ),
        Expr::Assign {
            target: crate::AssignTarget {
                root: "holder".into(),
                steps: vec![
                    crate::AssignPathStep::Field("re".into()),
                    crate::AssignPathStep::Field("lastIndex".into()),
                ],
            },
            expr: Box::new(Expr::Number(5.0)),
        },
        Expr::Print(Box::new(Expr::String("park".into()))),
        Expr::Finish(Box::new(Expr::Field {
            target: Box::new(Expr::Field {
                target: Box::new(Expr::Variable("holder".into())),
                field: "re".into(),
            }),
            field: "lastIndex".into(),
        })),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::Number(5.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn exotic_references_work_as_discarded_truthy_unary_iterable_and_binary_operands() {
    let map_setup = || ts_assign("map", heap_new("Map", Vec::new()));
    let set_setup = || ts_assign("set", heap_new("Set", Vec::new()));

    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(vec![
            map_setup(),
            heap_method(
                "set",
                "map",
                vec![Expr::String("k".into()), Expr::Number(5.0)],
            ),
            Expr::Finish(Box::new(field("map", "size"))),
        ]))
        .await,
        ExecutionOutcome::Finished(Value::Number(1.0))
    );
    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(vec![
            map_setup(),
            ts_assign(
                "bound",
                heap_method(
                    "set",
                    "map",
                    vec![Expr::String("k".into()), Expr::Number(5.0)],
                ),
            ),
            Expr::Finish(Box::new(Expr::JavaScriptBinary {
                left: Box::new(Expr::Variable("bound".into())),
                op: crate::JavaScriptBinaryOp::StrictEqual,
                right: Box::new(Expr::Variable("map".into())),
            })),
        ]))
        .await,
        ExecutionOutcome::Finished(Value::Bool(true))
    );
    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(vec![
            map_setup(),
            Expr::If {
                condition: Box::new(Expr::Variable("map".into())),
                then_block: Box::new(Expr::Finish(Box::new(Expr::Bool(true)))),
                else_block: Box::new(Expr::Finish(Box::new(Expr::Bool(false)))),
            },
        ]))
        .await,
        ExecutionOutcome::Finished(Value::Bool(true))
    );
    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(vec![
            set_setup(),
            Expr::Finish(Box::new(Expr::JavaScriptUnary {
                op: crate::JavaScriptUnaryOp::Not,
                expr: Box::new(Expr::Variable("set".into())),
            })),
        ]))
        .await,
        ExecutionOutcome::Finished(Value::Bool(false))
    );

    let iterable = heap_new(
        "Set",
        vec![Expr::List(vec![Expr::Number(2.0), Expr::Number(3.0)])],
    );
    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(vec![
            ts_assign("set", iterable),
            ts_assign("total", Expr::Number(0.0)),
            Expr::For {
                binding: "value".into(),
                iterable: Box::new(Expr::Variable("set".into())),
                body: Box::new(ts_assign(
                    "total",
                    Expr::JavaScriptBinary {
                        left: Box::new(Expr::Variable("total".into())),
                        op: crate::JavaScriptBinaryOp::Add,
                        right: Box::new(Expr::Variable("value".into())),
                    },
                )),
            },
            Expr::Finish(Box::new(Expr::Variable("total".into()))),
        ]))
        .await,
        ExecutionOutcome::Finished(Value::Number(5.0))
    );

    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(vec![
            ts_assign(
                "map",
                heap_new(
                    "Map",
                    vec![Expr::List(vec![
                        Expr::List(vec![Expr::String("a".into()), Expr::Number(2.0)]),
                        Expr::List(vec![Expr::String("b".into()), Expr::Number(3.0)]),
                    ])],
                ),
            ),
            ts_assign("total", Expr::Number(0.0)),
            Expr::For {
                binding: "entry".into(),
                iterable: Box::new(Expr::Variable("map".into())),
                body: Box::new(ts_assign(
                    "total",
                    Expr::JavaScriptBinary {
                        left: Box::new(Expr::Variable("total".into())),
                        op: crate::JavaScriptBinaryOp::Add,
                        right: Box::new(Expr::Index {
                            target: Box::new(Expr::Variable("entry".into())),
                            index: Box::new(Expr::Number(1.0)),
                        }),
                    },
                )),
            },
            Expr::Finish(Box::new(Expr::Variable("total".into()))),
        ]))
        .await,
        ExecutionOutcome::Finished(Value::Number(5.0))
    );

    assert_eq!(
        run_typescript_ast_across_every_effect(Program::block(vec![
            ts_assign("left", heap_new("Date", vec![Expr::Number(9.0)])),
            ts_assign("right", heap_new("Date", vec![Expr::Number(4.0)])),
            Expr::Finish(Box::new(Expr::JavaScriptBinary {
                left: Box::new(Expr::Variable("left".into())),
                op: crate::JavaScriptBinaryOp::Subtract,
                right: Box::new(Expr::Variable("right".into())),
            })),
        ]))
        .await,
        ExecutionOutcome::Finished(Value::Number(5.0))
    );

    let unsupported_date_add = Program::block(vec![
        ts_assign("date", heap_new("Date", vec![Expr::Number(9.0)])),
        Expr::Finish(Box::new(Expr::JavaScriptBinary {
            left: Box::new(Expr::Variable("date".into())),
            op: crate::JavaScriptBinaryOp::Add,
            right: Box::new(Expr::String("x".into())),
        })),
    ]);
    let compiled = compile_ast_with_dialect(&unsupported_date_add, CompilationDialect::Typescript)
        .expect("compile Date addition regression");
    let error = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect_err("Date addition needs pending string semantics");
    assert!(
        error
            .to_string()
            .contains("TS_DATE_STRING_COERCION_PENDING"),
        "{error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn set_normalizes_negative_zero_before_iteration() {
    let program = Program::block(vec![
        ts_assign("set", heap_new("Set", Vec::new())),
        heap_method("add", "set", vec![Expr::Number(-0.0)]),
        ts_assign("reciprocal", Expr::Undefined),
        Expr::For {
            binding: "value".into(),
            iterable: Box::new(Expr::Variable("set".into())),
            body: Box::new(ts_assign(
                "reciprocal",
                Expr::JavaScriptBinary {
                    left: Box::new(Expr::Number(1.0)),
                    op: crate::JavaScriptBinaryOp::Divide,
                    right: Box::new(Expr::Variable("value".into())),
                },
            )),
        },
        Expr::Finish(Box::new(Expr::Variable("reciprocal".into()))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::Number(f64::INFINITY))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn lashlang_dialect_cannot_execute_javascript_heap_constructor_intrinsic() {
    let program = Program::block(vec![Expr::Finish(Box::new(heap_new("Map", Vec::new())))]);
    let compiled = compile_ast_with_dialect(&program, CompilationDialect::Lashlang)
        .expect("private intrinsic compiles for gate regression");
    let error = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect_err("Lashlang must not mint a JavaScript exotic");
    assert!(
        error
            .to_string()
            .contains("TYPESCRIPT_REFERENCE_SEMANTICS_REQUIRED"),
        "{error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn map_for_each_callback_parks_and_resumes_through_the_shared_driver() {
    let callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["value".into(), "key".into(), "map".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::Variable("value".into()))),
            Expr::Return(Box::new(Expr::Undefined)),
        ])),
    }));
    let entries = Expr::List(vec![
        Expr::List(vec![Expr::String("a".into()), Expr::Number(1.0)]),
        Expr::List(vec![Expr::String("b".into()), Expr::Number(2.0)]),
    ]);
    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("map".into()),
            expr: Box::new(private_builtin(
                "__typescript_heap_new",
                vec![Expr::String("Map".into()), entries],
            )),
        },
        Expr::Assign {
            target: crate::AssignTarget::variable("callback".into()),
            expr: Box::new(callback),
        },
        private_builtin(
            "__typescript_stdlib",
            vec![
                Expr::String("forEach".into()),
                Expr::Variable("map".into()),
                Expr::Variable("callback".into()),
            ],
        ),
        Expr::Finish(Box::new(Expr::Field {
            target: Box::new(Expr::Variable("map".into())),
            field: "size".into(),
        })),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::Number(2.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn error_family_observables_are_node_shaped_and_durable() {
    let program = Program::block(vec![
        ts_assign(
            "error",
            heap_new(
                "TypeError",
                vec![
                    Expr::String("bad".into()),
                    Expr::Record(vec![("cause".into(), Expr::List(vec![Expr::Number(7.0)]))]),
                ],
            ),
        ),
        Expr::Print(Box::new(Expr::String("park with the Error rooted".into()))),
        Expr::Finish(Box::new(Expr::List(vec![
            field("error", "name"),
            field("error", "message"),
            Expr::Index {
                target: Box::new(field("error", "cause")),
                index: Box::new(Expr::Number(0.0)),
            },
            private_builtin(
                "__typescript_heap_instanceof",
                vec![
                    Expr::Variable("error".into()),
                    Expr::String("TypeError".into()),
                ],
            ),
            private_builtin(
                "__typescript_heap_instanceof",
                vec![Expr::Variable("error".into()), Expr::String("Error".into())],
            ),
            private_builtin(
                "__typescript_heap_instanceof",
                vec![
                    Expr::Variable("error".into()),
                    Expr::String("RangeError".into()),
                ],
            ),
            heap_method("toString", "error", Vec::new()),
            Expr::JavaScriptUnary {
                op: crate::JavaScriptUnaryOp::Plus,
                expr: Box::new(Expr::Variable("error".into())),
            },
            private_builtin(
                "__typescript_stdlib",
                vec![
                    Expr::String("Object.keys".into()),
                    Expr::Variable("error".into()),
                ],
            ),
            private_builtin(
                "__typescript_stdlib",
                vec![
                    Expr::String("JSON.stringify".into()),
                    Expr::Record(vec![("error".into(), Expr::Variable("error".into()))]),
                ],
            ),
            private_builtin(
                "__typescript_stdlib",
                vec![
                    Expr::String("JSON.stringify".into()),
                    Expr::Variable("error".into()),
                ],
            ),
        ]))),
    ]);
    let ExecutionOutcome::Finished(Value::List(values)) =
        run_typescript_ast_across_every_effect(program).await
    else {
        panic!("Error observables should finish as a list")
    };
    assert_eq!(values[0], Value::String("TypeError".into()));
    assert_eq!(values[1], Value::String("bad".into()));
    assert_eq!(values[2], Value::Number(7.0));
    assert_eq!(
        &values[3..6],
        &[Value::Bool(true), Value::Bool(true), Value::Bool(false)]
    );
    assert_eq!(values[6], Value::String("TypeError: bad".into()));
    assert!(matches!(values[7], Value::Number(value) if value.is_nan()));
    assert_eq!(values[8], Value::List(Vec::new().into()));
    assert_eq!(values[9], Value::String("{\"error\":{}}".into()));
    assert_eq!(values[10], Value::String("{}".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn every_error_brand_has_error_ancestry_and_each_ecma_kind_its_own() {
    let kinds = [
        "Error",
        "TypeError",
        "RangeError",
        "SyntaxError",
        "ReferenceError",
        "URIError",
        "EvalError",
        "AggregateError",
    ];
    let mut expressions = Vec::new();
    let mut checks = Vec::new();
    for (index, kind) in kinds.into_iter().enumerate() {
        let name = format!("error_{index}");
        let args = if kind == "AggregateError" {
            vec![
                Expr::List(vec![
                    Expr::String("first".into()),
                    Expr::String("second".into()),
                ]),
                Expr::String("many".into()),
            ]
        } else {
            vec![Expr::String("one".into())]
        };
        expressions.push(ts_assign(&name, heap_new(kind, args)));
        checks.push(private_builtin(
            "__typescript_heap_instanceof",
            vec![
                Expr::Variable(name.as_str().into()),
                Expr::String(kind.into()),
            ],
        ));
        checks.push(private_builtin(
            "__typescript_heap_instanceof",
            vec![
                Expr::Variable(name.as_str().into()),
                Expr::String("Error".into()),
            ],
        ));
        if kind == "AggregateError" {
            checks.push(Expr::Field {
                target: Box::new(field(&name, "errors")),
                field: "length".into(),
            });
        }
    }
    // The two brands the substrate mints for delivered rejections have no
    // constructor in the dialect, so `Error` is the only ancestry they can be
    // asked about — and nothing narrower may answer true.
    for (index, brand) in ["EffectError", "RuntimeError"].into_iter().enumerate() {
        let name = format!("brand_{index}");
        expressions.push(ts_assign(
            &name,
            heap_new(brand, vec![Expr::String("one".into())]),
        ));
        for constructor in ["Error", "TypeError"] {
            checks.push(private_builtin(
                "__typescript_heap_instanceof",
                vec![
                    Expr::Variable(name.as_str().into()),
                    Expr::String(constructor.into()),
                ],
            ));
        }
    }
    expressions.push(Expr::Finish(Box::new(Expr::List(checks))));
    let ExecutionOutcome::Finished(Value::List(values)) =
        run_typescript_ast_across_every_effect(Program::block(expressions)).await
    else {
        panic!("Error ancestry should finish as a list")
    };
    assert!(values[..16].iter().all(|value| *value == Value::Bool(true)));
    assert_eq!(values[16], Value::Number(2.0));
    assert_eq!(
        &values[17..21],
        &[
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false)
        ],
        "a minted brand answers `instanceof Error` and nothing narrower"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn instanceof_hook_covers_every_javascript_heap_kind() {
    let cases = [
        (
            "RegExp",
            heap_new(
                "RegExp",
                vec![Expr::String("a".into()), Expr::String("".into())],
            ),
        ),
        ("Map", heap_new("Map", Vec::new())),
        ("Set", heap_new("Set", Vec::new())),
        ("Date", heap_new("Date", vec![Expr::Number(0.0)])),
    ];
    for (kind, value) in cases {
        let program = Program::block(vec![Expr::Finish(Box::new(private_builtin(
            "__typescript_heap_instanceof",
            vec![value, Expr::String(kind.into())],
        )))]);
        assert_eq!(
            run_typescript_ast_across_every_effect(program).await,
            ExecutionOutcome::Finished(Value::Bool(true)),
            "{kind} instanceof {kind}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn thrown_error_keeps_identity_and_internal_exotic_assignment_throws_type_error() {
    let identity = Program::block(vec![
        ts_assign("error", heap_new("Error", vec![Expr::String("x".into())])),
        Expr::Finish(Box::new(Expr::Try(Box::new(crate::TryExpr {
            body: Box::new(Expr::Throw(Box::new(Expr::Variable("error".into())))),
            catch: Some(crate::CatchClause {
                binding: "caught".into(),
                body: Box::new(Expr::Block(vec![
                    Expr::Print(Box::new(Expr::String("park in catch".into()))),
                    Expr::JavaScriptBinary {
                        left: Box::new(Expr::Variable("caught".into())),
                        op: crate::JavaScriptBinaryOp::StrictEqual,
                        right: Box::new(Expr::Variable("error".into())),
                    },
                ])),
            }),
            finally: None,
        })))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(identity).await,
        ExecutionOutcome::Finished(Value::Bool(true))
    );

    let internal = Program::block(vec![
        ts_assign(
            "regexp",
            heap_new(
                "RegExp",
                vec![Expr::String("a".into()), Expr::String("".into())],
            ),
        ),
        Expr::Finish(Box::new(Expr::Try(Box::new(crate::TryExpr {
            body: Box::new(Expr::Assign {
                target: crate::AssignTarget {
                    root: "regexp".into(),
                    steps: vec![crate::AssignPathStep::Field("source".into())],
                },
                expr: Box::new(Expr::String("b".into())),
            }),
            catch: Some(crate::CatchClause {
                binding: "caught".into(),
                body: Box::new(private_builtin(
                    "__typescript_heap_instanceof",
                    vec![
                        Expr::Variable("caught".into()),
                        Expr::String("TypeError".into()),
                    ],
                )),
            }),
            finally: None,
        })))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(internal).await,
        ExecutionOutcome::Finished(Value::Bool(true))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_calls_apply_ecma_arguments_and_rest_then_resume_inside_the_callee() {
    let plain = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["first".into(), "second".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::String("callee park".into()))),
            Expr::Return(Box::new(Expr::JavaScriptBinary {
                left: Box::new(Expr::Variable("second".into())),
                op: crate::JavaScriptBinaryOp::StrictEqual,
                right: Box::new(Expr::Undefined),
            })),
        ])),
    }));
    let rest = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["first".into(), "rest".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Return(Box::new(Expr::Field {
            target: Box::new(Expr::Variable("rest".into())),
            field: "length".into(),
        }))),
    }));
    let program = Program::block(vec![
        ts_assign("plain", plain),
        ts_assign(
            "rest",
            private_builtin(
                "__typescript_closure",
                vec![rest, Expr::Number(1.0), Expr::Bool(true)],
            ),
        ),
        Expr::Finish(Box::new(Expr::List(vec![
            private_builtin(
                "__typescript_call_dynamic",
                vec![
                    Expr::Variable("plain".into()),
                    Expr::List(vec![Expr::Number(1.0)]),
                ],
            ),
            private_builtin(
                "__typescript_call_dynamic",
                vec![
                    Expr::Variable("rest".into()),
                    Expr::List(vec![
                        Expr::Number(1.0),
                        Expr::Number(2.0),
                        Expr::Number(3.0),
                    ]),
                ],
            ),
            Expr::Call {
                function: Box::new(Expr::Variable("plain".into())),
                args: Vec::new(),
            },
            Expr::Call {
                function: Box::new(Expr::Variable("plain".into())),
                args: vec![Expr::Number(1.0), Expr::Number(2.0), Expr::Number(3.0)],
            },
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::Bool(true),
                Value::Number(2.0),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn async_map_callbacks_park_before_and_after_work_and_replay_deterministically() {
    let callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["value".into(), "index".into(), "array".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::String("before await".into()))),
            Expr::Print(Box::new(Expr::String("after await".into()))),
            Expr::Return(Box::new(Expr::JavaScriptBinary {
                left: Box::new(Expr::Variable("value".into())),
                op: crate::JavaScriptBinaryOp::Multiply,
                right: Box::new(Expr::Number(2.0)),
            })),
        ])),
    }));
    let program = Program::block(vec![Expr::Finish(Box::new(private_builtin(
        "__typescript_async_map",
        vec![
            Expr::List(vec![Expr::Number(2.0), Expr::Number(4.0)]),
            callback,
        ],
    )))]);
    let expected = ExecutionOutcome::Finished(Value::List(
        vec![Value::Number(4.0), Value::Number(8.0)].into(),
    ));
    assert_eq!(
        run_typescript_ast_across_every_effect(program.clone()).await,
        expected
    );
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        expected
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stored_per_iteration_closures_stay_inside_the_vm_across_calls_and_parks() {
    let callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: Vec::new(),
        captures: vec!["i".into()],
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::String("park inside stored closure".into()))),
            Expr::Return(Box::new(Expr::Variable("i".into()))),
        ])),
    }));
    let stored_call = |index| Expr::Call {
        function: Box::new(Expr::Index {
            target: Box::new(Expr::Variable("callbacks".into())),
            index: Box::new(Expr::Number(index)),
        }),
        args: Vec::new(),
    };
    let program = Program::block(vec![
        ts_assign("callbacks", Expr::List(Vec::new())),
        Expr::For {
            binding: "i".into(),
            iterable: Box::new(private_builtin(
                "range",
                vec![Expr::Number(0.0), Expr::Number(2.0)],
            )),
            body: Box::new(Expr::Block(vec![ts_assign(
                "callbacks",
                private_builtin("push", vec![Expr::Variable("callbacks".into()), callback]),
            )])),
        },
        Expr::Finish(Box::new(Expr::List(vec![
            stored_call(0.0),
            stored_call(1.0),
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(0.0), Value::Number(1.0)].into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn discarded_and_boolean_tested_closures_are_vm_internal_values() {
    let closure = || {
        Expr::Function(Box::new(crate::FunctionExpr {
            name: None,
            params: Vec::new(),
            captures: Vec::new(),
            body: Box::new(Expr::Return(Box::new(Expr::Number(1.0)))),
        }))
    };
    let program = Program::block(vec![
        closure(),
        Expr::Finish(Box::new(Expr::If {
            condition: Box::new(closure()),
            then_block: Box::new(Expr::Bool(true)),
            else_block: Box::new(Expr::Bool(false)),
        })),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::Bool(true))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn async_map_all_settled_wrapper_collects_throws_in_input_order_across_parks() {
    let callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["value".into(), "index".into(), "array".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Return(Box::new(Expr::Try(Box::new(
            crate::TryExpr {
                body: Box::new(Expr::Block(vec![
                    Expr::Print(Box::new(Expr::String("park before settlement".into()))),
                    Expr::If {
                        condition: Box::new(Expr::JavaScriptBinary {
                            left: Box::new(Expr::Variable("value".into())),
                            op: crate::JavaScriptBinaryOp::StrictEqual,
                            right: Box::new(Expr::Number(2.0)),
                        }),
                        then_block: Box::new(Expr::Throw(Box::new(Expr::String("boom".into())))),
                        else_block: Box::new(Expr::Record(vec![
                            ("status".into(), Expr::String("fulfilled".into())),
                            ("value".into(), Expr::Variable("value".into())),
                        ])),
                    },
                ])),
                catch: Some(crate::CatchClause {
                    binding: "reason".into(),
                    body: Box::new(Expr::Record(vec![
                        ("status".into(), Expr::String("rejected".into())),
                        ("reason".into(), Expr::Variable("reason".into())),
                    ])),
                }),
                finally: None,
            },
        ))))),
    }));
    let program = Program::block(vec![Expr::Finish(Box::new(private_builtin(
        "__typescript_async_map",
        vec![
            Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)]),
            callback,
        ],
    )))]);
    let mut fulfilled = crate::Record::new();
    fulfilled.insert("status".into(), Value::String("fulfilled".into()));
    fulfilled.insert("value".into(), Value::Number(1.0));
    let mut rejected = crate::Record::new();
    rejected.insert("status".into(), Value::String("rejected".into()));
    rejected.insert("reason".into(), Value::String("boom".into()));
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::Record(fulfilled.into()),
                Value::Record(rejected.into())
            ]
            .into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn global_delete_and_presence_preserve_absent_vs_undefined_across_restart() {
    let program = Program::block(vec![
        ts_assign("kept", Expr::Undefined),
        ts_assign("removed", Expr::Number(1.0)),
        private_builtin(
            "__typescript_global_delete",
            vec![Expr::String("removed".into())],
        ),
        Expr::Print(Box::new(Expr::String("park after deletion".into()))),
        Expr::Finish(Box::new(Expr::List(vec![
            private_builtin("__typescript_global_has", vec![Expr::String("kept".into())]),
            private_builtin(
                "__typescript_global_has",
                vec![Expr::String("removed".into())],
            ),
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Bool(true), Value::Bool(false)].into()
        ))
    );

    let setup = Program::block(vec![
        ts_assign("kept", Expr::Undefined),
        ts_assign("removed", Expr::Number(1.0)),
        Expr::Finish(Box::new(Expr::Null)),
    ]);
    let setup = compile_ast_with_dialect(&setup, CompilationDialect::Typescript)
        .expect("compile global setup");
    let mut state = State::new();
    execute(&setup, &mut state, &Host)
        .await
        .expect("persist globals before deletion");
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("encode pre-deletion snapshot");
    let snapshot = Snapshot::from_canonical_bytes(&bytes).expect("decode pre-deletion snapshot");
    let mut state = State::from_snapshot(snapshot);
    let deletion = Program::block(vec![
        private_builtin(
            "__typescript_global_delete",
            vec![Expr::String("removed".into())],
        ),
        Expr::Finish(Box::new(Expr::Null)),
    ]);
    let deletion = compile_ast_with_dialect(&deletion, CompilationDialect::Typescript)
        .expect("compile persisted-global deletion");
    execute(&deletion, &mut state, &Host)
        .await
        .expect("delete previously persisted global");
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("encode post-deletion snapshot");
    let snapshot = Snapshot::from_canonical_bytes(&bytes).expect("decode post-deletion snapshot");
    let mut state = State::from_snapshot(snapshot);
    let query = Program::block(vec![Expr::Finish(Box::new(Expr::List(vec![
        private_builtin("__typescript_global_has", vec![Expr::String("kept".into())]),
        private_builtin(
            "__typescript_global_has",
            vec![Expr::String("removed".into())],
        ),
    ])))]);
    let query = compile_ast_with_dialect(&query, CompilationDialect::Typescript)
        .expect("compile rehydration query");
    assert_eq!(
        execute(&query, &mut state, &Host)
            .await
            .expect("query rehydrated globals"),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Bool(true), Value::Bool(false)].into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_global_set_is_durable_across_function_park_and_state_restore() {
    let setter = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: Vec::new(),
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::String("park before global set".into()))),
            Expr::Return(Box::new(private_builtin(
                "__typescript_global_set",
                vec![
                    Expr::String("answer".into()),
                    Expr::Record(vec![("value".into(), Expr::Number(42.0))]),
                ],
            ))),
        ])),
    }));
    let setup = Program::block(vec![
        Expr::Call {
            function: Box::new(setter),
            args: Vec::new(),
        },
        Expr::Finish(Box::new(field("answer", "value"))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(setup.clone()).await,
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
    let setup = compile_ast_with_dialect(&setup, CompilationDialect::Typescript)
        .expect("compile nested global setter");
    let mut state = State::new();
    assert_eq!(
        execute(&setup, &mut state, &Host)
            .await
            .expect("execute nested global setter"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("encode global-set snapshot");
    let snapshot = Snapshot::from_canonical_bytes(&bytes).expect("decode global-set snapshot");
    let mut restored = State::from_snapshot(snapshot);
    let query = Program::block(vec![Expr::Finish(Box::new(field("answer", "value")))]);
    let query = compile_ast_with_dialect(&query, CompilationDialect::Typescript)
        .expect("compile global-set query");
    assert_eq!(
        execute(&query, &mut restored, &Host)
            .await
            .expect("query restored global"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn global_set_does_not_weaken_closure_session_persistence_policy() {
    let closure = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: Vec::new(),
        captures: Vec::new(),
        body: Box::new(Expr::Return(Box::new(Expr::Null))),
    }));
    let program = Program::block(vec![
        private_builtin(
            "__typescript_global_set",
            vec![Expr::String("stored_function".into()), closure],
        ),
        Expr::Finish(Box::new(Expr::Null)),
    ]);
    let compiled = compile_ast_with_dialect(&program, CompilationDialect::Typescript)
        .expect("compile closure global-set policy probe");
    let mut state = State::new();
    assert_eq!(
        execute(&compiled, &mut state, &Host)
            .await
            .expect("closure-valued session globals retain the existing omission policy"),
        ExecutionOutcome::Finished(Value::Null)
    );
    assert!(
        state.snapshot().globals().get("stored_function").is_none(),
        "closure-valued globals remain absent from the host-visible snapshot"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn heap_member_delete_preserves_aliases_and_survives_continuation_round_trip() {
    let program = Program::block(vec![
        ts_assign(
            "object",
            Expr::Record(vec![
                (
                    "removed".into(),
                    Expr::Record(vec![("child".into(), Expr::Number(1.0))]),
                ),
                ("kept".into(), Expr::Number(2.0)),
            ]),
        ),
        ts_assign("alias", Expr::Variable("object".into())),
        ts_assign(
            "deleted",
            private_builtin(
                "__typescript_heap_delete_member",
                vec![
                    Expr::Variable("object".into()),
                    Expr::String("removed".into()),
                ],
            ),
        ),
        Expr::Print(Box::new(Expr::String("park after member deletion".into()))),
        Expr::Finish(Box::new(Expr::List(vec![
            Expr::Variable("deleted".into()),
            private_builtin(
                "__typescript_stdlib",
                vec![
                    Expr::String("Object.hasOwn".into()),
                    Expr::Variable("alias".into()),
                    Expr::String("removed".into()),
                ],
            ),
            field("alias", "kept"),
        ]))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Bool(true), Value::Bool(false), Value::Number(2.0)].into()
        ))
    );

    let array_delete = Program::block(vec![
        ts_assign("array", Expr::List(vec![Expr::Number(1.0)])),
        Expr::Finish(Box::new(private_builtin(
            "__typescript_heap_delete_member",
            vec![Expr::Variable("array".into()), Expr::Number(0.0)],
        ))),
    ]);
    let compiled = compile_ast_with_dialect(&array_delete, CompilationDialect::Typescript)
        .expect("compile dense-array deletion probe");
    let error = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect_err("deleting a present dense-array index must reject");
    assert!(
        error
            .to_string()
            .contains("TS_DELETE_ARRAY_INDEX_UNSUPPORTED")
            && error.to_string().contains("splice"),
        "{error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_global_names_are_rejected_by_all_root_intrinsics() {
    for intrinsic in ["__typescript_global_delete", "__typescript_global_has"] {
        for name in ["undefined", "NaN", "Infinity"] {
            let program = Program::block(vec![Expr::Finish(Box::new(private_builtin(
                intrinsic,
                vec![Expr::String(name.into())],
            )))]);
            let compiled = compile_ast_with_dialect(&program, CompilationDialect::Typescript)
                .expect("compile reserved-name probe");
            let error = execute(&compiled, &mut State::new(), &Host)
                .await
                .expect_err("reserved global name must reject");
            assert!(
                error.to_string().contains("TS_RESERVED_GLOBAL_NAME"),
                "{error}"
            );
        }
    }
    for name in ["undefined", "NaN", "Infinity"] {
        let program = Program::block(vec![Expr::Finish(Box::new(private_builtin(
            "__typescript_global_set",
            vec![Expr::String(name.into()), Expr::Number(1.0)],
        )))]);
        let compiled = compile_ast_with_dialect(&program, CompilationDialect::Typescript)
            .expect("compile reserved global set probe");
        let error = execute(&compiled, &mut State::new(), &Host)
            .await
            .expect_err("reserved global set must reject");
        assert!(
            error.to_string().contains("TS_RESERVED_GLOBAL_NAME"),
            "{error}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn uri_codec_intrinsics_match_node_and_throw_real_uri_errors() {
    let caught_uri_error = |intrinsic: &str, input: &str| {
        Expr::Try(Box::new(crate::TryExpr {
            body: Box::new(private_builtin(intrinsic, vec![Expr::String(input.into())])),
            catch: Some(crate::CatchClause {
                binding: "caught".into(),
                body: Box::new(Expr::List(vec![
                    field("caught", "name"),
                    field("caught", "message"),
                    private_builtin(
                        "__typescript_heap_instanceof",
                        vec![
                            Expr::Variable("caught".into()),
                            Expr::String("URIError".into()),
                        ],
                    ),
                ])),
            }),
            finally: None,
        }))
    };
    let program = Program::block(vec![Expr::Finish(Box::new(Expr::List(vec![
        private_builtin(
            "__typescript_encode_uri_component",
            vec![Expr::String("A Z;/?:@&=+$,#-_.!~*'()é😀".into())],
        ),
        private_builtin(
            "__typescript_encode_uri",
            vec![Expr::String("https://a.test/a b?x=é&y=#z".into())],
        ),
        private_builtin(
            "__typescript_decode_uri_component",
            vec![Expr::String(
                "A%20Z%3B%2F%3F%3A%40%26%3D%2B%24%2C%23%C3%A9%F0%9F%98%80".into(),
            )],
        ),
        private_builtin(
            "__typescript_decode_uri",
            vec![Expr::String("https://a.test/a%20b?x=%C3%A9&y=%23z".into())],
        ),
        caught_uri_error("__typescript_decode_uri_component", "%C0%AF"),
        caught_uri_error("__typescript_decode_uri", "%E0%A4%A"),
    ])))]);
    let uri_error = || {
        Value::List(
            vec![
                Value::String("URIError".into()),
                Value::String("URI malformed".into()),
                Value::Bool(true),
            ]
            .into(),
        )
    };
    assert_eq!(
        run_typescript_ast_across_every_effect(program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::String(
                    "A%20Z%3B%2F%3F%3A%40%26%3D%2B%24%2C%23-_.!~*'()%C3%A9%F0%9F%98%80".into(),
                ),
                Value::String("https://a.test/a%20b?x=%C3%A9&y=#z".into()),
                Value::String("A Z;/?:@&=+$,#é😀".into()),
                Value::String("https://a.test/a b?x=é&y=%23z".into()),
                uri_error(),
                uri_error(),
            ]
            .into(),
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn map_and_set_for_each_use_a_live_durable_cursor() {
    let map_callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["value".into(), "key".into(), "receiver".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::Variable("value".into()))),
            Expr::If {
                condition: Box::new(Expr::JavaScriptBinary {
                    left: Box::new(Expr::Variable("key".into())),
                    op: crate::JavaScriptBinaryOp::StrictEqual,
                    right: Box::new(Expr::String("a".into())),
                }),
                then_block: Box::new(Expr::Block(vec![
                    private_builtin(
                        "__typescript_stdlib",
                        vec![
                            Expr::String("delete".into()),
                            Expr::Variable("receiver".into()),
                            Expr::String("b".into()),
                        ],
                    ),
                    private_builtin(
                        "__typescript_stdlib",
                        vec![
                            Expr::String("set".into()),
                            Expr::Variable("receiver".into()),
                            Expr::String("c".into()),
                            Expr::Number(3.0),
                        ],
                    ),
                ])),
                else_block: Box::new(Expr::Undefined),
            },
            Expr::Return(Box::new(Expr::Undefined)),
        ])),
    }));
    let map_program = Program::block(vec![
        ts_assign(
            "map",
            heap_new(
                "Map",
                vec![Expr::List(vec![
                    Expr::List(vec![Expr::String("a".into()), Expr::Number(1.0)]),
                    Expr::List(vec![Expr::String("b".into()), Expr::Number(2.0)]),
                ])],
            ),
        ),
        ts_assign("callback", map_callback),
        heap_method("forEach", "map", vec![Expr::Variable("callback".into())]),
        Expr::Finish(Box::new(Expr::List(
            ["a", "b", "c"]
                .into_iter()
                .map(|key| heap_method("has", "map", vec![Expr::String(key.into())]))
                .collect(),
        ))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(map_program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)].into()
        ))
    );

    let set_callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["value".into(), "duplicate".into(), "receiver".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::Variable("value".into()))),
            Expr::If {
                condition: Box::new(Expr::JavaScriptBinary {
                    left: Box::new(Expr::Variable("value".into())),
                    op: crate::JavaScriptBinaryOp::StrictEqual,
                    right: Box::new(Expr::String("a".into())),
                }),
                then_block: Box::new(Expr::Block(vec![
                    private_builtin(
                        "__typescript_stdlib",
                        vec![
                            Expr::String("delete".into()),
                            Expr::Variable("receiver".into()),
                            Expr::String("b".into()),
                        ],
                    ),
                    private_builtin(
                        "__typescript_stdlib",
                        vec![
                            Expr::String("add".into()),
                            Expr::Variable("receiver".into()),
                            Expr::String("c".into()),
                        ],
                    ),
                ])),
                else_block: Box::new(Expr::Undefined),
            },
            Expr::Return(Box::new(Expr::Undefined)),
        ])),
    }));
    let set_program = Program::block(vec![
        ts_assign(
            "set",
            heap_new(
                "Set",
                vec![Expr::List(vec![
                    Expr::String("a".into()),
                    Expr::String("b".into()),
                ])],
            ),
        ),
        ts_assign("callback", set_callback),
        heap_method("forEach", "set", vec![Expr::Variable("callback".into())]),
        Expr::Finish(Box::new(Expr::List(
            ["a", "b", "c"]
                .into_iter()
                .map(|value| heap_method("has", "set", vec![Expr::String(value.into())]))
                .collect(),
        ))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(set_program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)].into()
        ))
    );
}
