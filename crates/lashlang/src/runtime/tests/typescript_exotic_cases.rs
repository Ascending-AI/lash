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
async fn map_and_set_for_each_use_a_durable_cloned_snapshot() {
    fn seen_name(variable: &str) -> Expr {
        Expr::JavaScriptBinary {
            left: Box::new(Expr::String("seen-".into())),
            op: crate::JavaScriptBinaryOp::Add,
            right: Box::new(Expr::Variable(variable.into())),
        }
    }

    let map_callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["value".into(), "key".into(), "receiver".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::Variable("value".into()))),
            private_builtin(
                "__typescript_stdlib",
                vec![
                    Expr::String("set".into()),
                    Expr::Variable("receiver".into()),
                    seen_name("key"),
                    Expr::Bool(true),
                ],
            ),
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
            ["seen-a", "seen-b", "seen-c"]
                .into_iter()
                .map(|key| heap_method("has", "map", vec![Expr::String(key.into())]))
                .collect(),
        ))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(map_program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Bool(true), Value::Bool(true), Value::Bool(false)].into()
        ))
    );

    let set_callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["value".into(), "duplicate".into(), "receiver".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::Variable("value".into()))),
            private_builtin(
                "__typescript_stdlib",
                vec![
                    Expr::String("add".into()),
                    Expr::Variable("receiver".into()),
                    seen_name("value"),
                ],
            ),
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
            ["seen-a", "seen-b", "seen-c"]
                .into_iter()
                .map(|value| heap_method("has", "set", vec![Expr::String(value.into())]))
                .collect(),
        ))),
    ]);
    assert_eq!(
        run_typescript_ast_across_every_effect(set_program).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Bool(true), Value::Bool(true), Value::Bool(false)].into()
        ))
    );
}
