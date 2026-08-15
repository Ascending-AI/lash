fn private_builtin(name: &str, args: Vec<Expr>) -> Expr {
    Expr::BuiltinCall {
        name: name.into(),
        args,
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
