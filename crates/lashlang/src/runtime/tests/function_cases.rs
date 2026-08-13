use crate::ast::{AssignTarget, BinaryOp, FunctionExpr};
use crate::runtime::entry_points::compile_program_internal;

fn variable(name: &str) -> Expr {
    Expr::Variable(name.into())
}

fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

fn call(function: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call {
        function: Box::new(function),
        args,
    }
}

fn function(name: Option<&str>, params: &[&str], captures: &[&str], body: Expr) -> Expr {
    Expr::Function(Box::new(FunctionExpr {
        name: name.map(Into::into),
        params: params.iter().map(|name| (*name).into()).collect(),
        captures: captures.iter().map(|name| (*name).into()).collect(),
        body: Box::new(body),
    }))
}

fn format_call(template: &str, args: Vec<Expr>) -> Expr {
    let mut all_args = vec![Expr::String(template.into())];
    all_args.extend(args);
    Expr::BuiltinCall {
        name: "format".into(),
        args: all_args,
    }
}

fn factorial_program(n: f64) -> Program {
    let recursive = call(
        variable("factorial"),
        vec![Expr::Binary {
            left: Box::new(variable("n")),
            op: BinaryOp::Subtract,
            right: Box::new(Expr::Number(1.0)),
        }],
    );
    let body = Expr::If {
        condition: Box::new(Expr::Binary {
            left: Box::new(variable("n")),
            op: BinaryOp::LessEqual,
            right: Box::new(Expr::Number(1.0)),
        }),
        then_block: Box::new(Expr::Number(1.0)),
        else_block: Box::new(Expr::Binary {
            left: Box::new(variable("n")),
            op: BinaryOp::Multiply,
            right: Box::new(recursive),
        }),
    };
    Program::block(vec![
        assign("factorial", function(Some("factorial"), &["n"], &[], body)),
        Expr::Finish(Box::new(call(variable("factorial"), vec![Expr::Number(n)]))),
    ])
}

#[tokio::test(flavor = "current_thread")]
async fn user_function_closure_capture_is_deep_by_value_and_recursion_is_stackless() {
    let closure_program = Program::block(vec![
        assign("captured", Expr::List(vec![Expr::Number(10.0)])),
        assign(
            "add_capture",
            function(
                None,
                &["value"],
                &["captured"],
                Expr::Binary {
                    left: Box::new(Expr::Index {
                        target: Box::new(variable("captured")),
                        index: Box::new(Expr::Number(0.0)),
                    }),
                    op: BinaryOp::Add,
                    right: Box::new(variable("value")),
                },
            ),
        ),
        assign("captured", Expr::List(vec![Expr::Number(99.0)])),
        Expr::Finish(Box::new(call(
            variable("add_capture"),
            vec![Expr::Number(5.0)],
        ))),
    ]);
    let closure = compile_program_internal(&closure_program);
    assert_eq!(
        execute_compiled(&closure, &mut State::new(), &Host)
            .await
            .expect("closure program"),
        ExecutionOutcome::Finished(Value::Number(15.0))
    );

    let factorial = compile_program_internal(&factorial_program(8.0));
    assert_eq!(
        execute_compiled(&factorial, &mut State::new(), &Host)
            .await
            .expect("recursive program"),
        ExecutionOutcome::Finished(Value::Number(40_320.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_map_reenters_the_flat_vm_and_rejects_effectful_callbacks() {
    let pure = Program::block(vec![
        assign(
            "double",
            function(
                None,
                &["value"],
                &[],
                Expr::Binary {
                    left: Box::new(variable("value")),
                    op: BinaryOp::Multiply,
                    right: Box::new(Expr::Number(2.0)),
                },
            ),
        ),
        Expr::Finish(Box::new(Expr::Map {
            items: Box::new(Expr::List(vec![
                Expr::Number(1.0),
                Expr::Number(2.0),
                Expr::Number(3.0),
            ])),
            function: Box::new(variable("double")),
        })),
    ]);
    assert_eq!(
        execute(&pure, &mut State::new(), &Host)
            .await
            .expect("pure callback"),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(2.0), Value::Number(4.0), Value::Number(6.0)].into()
        ))
    );

    let effectful = Program::block(vec![
        assign(
            "observe",
            function(
                None,
                &["value"],
                &[],
                Expr::Block(vec![
                    Expr::Print(Box::new(variable("value"))),
                    variable("value"),
                ]),
            ),
        ),
        Expr::Map {
            items: Box::new(Expr::List(vec![Expr::Number(1.0)])),
            function: Box::new(variable("observe")),
        },
    ]);
    assert!(matches!(
        execute(&effectful, &mut State::new(), &Host).await,
        Err(RuntimeError::EffectInBuiltinCallback)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn effect_suspension_inside_a_user_function_round_trips_the_frame_stack() {
    let program = compile_program_internal(&Program::block(vec![
        assign(
            "observe",
            function(
                None,
                &["value"],
                &[],
                Expr::Block(vec![
                    Expr::Print(Box::new(variable("value"))),
                    variable("value"),
                ]),
            ),
        ),
        Expr::Finish(Box::new(call(
            variable("observe"),
            vec![Expr::Number(17.0)],
        ))),
    ]));
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode()
            .await
            .expect("suspend after function effect"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("capture function continuation");
    assert_eq!(continuation.frame_stack.len(), 1);
    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        ExecutionOutcome::Finished(Value::Number(17.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn effect_suspension_inside_a_user_function_with_heap_argument_round_trips() {
    let program = compile_program_internal(&Program::block(vec![
        assign(
            "f",
            function(
                None,
                &["n"],
                &[],
                Expr::Block(vec![
                    Expr::Print(Box::new(Expr::Number(1.0))),
                    variable("n"),
                ]),
            ),
        ),
        Expr::Finish(Box::new(call(
            variable("f"),
            vec![Expr::List(vec![Expr::Number(7.0), Expr::Number(8.0)])],
        ))),
    ]));
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode()
            .await
            .expect("suspend after function effect"),
        ExecutionOutcome::Continued
    );
    let continuation = vm
        .suspend()
        .expect("capture function continuation with heap argument");
    assert_eq!(continuation.frame_stack.len(), 1);
    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(7.0), Value::Number(8.0)].into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn suspended_caller_and_callee_preserve_heap_arguments_and_locals() {
    let callee_body = Expr::Block(vec![
        assign("local_list", Expr::List(vec![variable("list_arg")])),
        assign(
            "local_record",
            Expr::Record(vec![("record".into(), variable("record_arg"))]),
        ),
        assign("local_tuple", Expr::Tuple(vec![variable("tuple_arg")])),
        Expr::Print(Box::new(Expr::Number(1.0))),
        Expr::Tuple(vec![
            variable("local_list"),
            variable("local_record"),
            variable("local_tuple"),
        ]),
    ]);
    let caller_body = Expr::Block(vec![
        assign(
            "caller_local",
            Expr::Record(vec![("payload".into(), variable("payload"))]),
        ),
        call(
            variable("callee"),
            vec![
                Expr::List(vec![Expr::Number(1.0)]),
                Expr::Record(vec![("value".into(), Expr::Number(2.0))]),
                Expr::Tuple(vec![Expr::Number(3.0)]),
            ],
        ),
    ]);
    let program = compile_program_internal(&Program::block(vec![
        assign(
            "callee",
            function(
                None,
                &["list_arg", "record_arg", "tuple_arg"],
                &[],
                callee_body,
            ),
        ),
        assign(
            "caller",
            function(None, &["payload"], &["callee"], caller_body),
        ),
        Expr::Finish(Box::new(call(
            variable("caller"),
            vec![Expr::List(vec![Expr::Number(9.0)])],
        ))),
    ]));
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("suspend in callee"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("capture caller and callee heaps");
    assert_eq!(continuation.frame_stack.len(), 2);
    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        ExecutionOutcome::Finished(Value::Tuple(
            vec![
                Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into()),
                Value::Record(std::sync::Arc::new({
                    let mut record = Record::new();
                    record.insert(
                        "record".to_string(),
                        Value::Record(std::sync::Arc::new({
                            let mut nested = Record::new();
                            nested.insert("value".to_string(), Value::Number(2.0));
                            nested
                        })),
                    );
                    record
                })),
                Value::Tuple(vec![Value::Tuple(vec![Value::Number(3.0)].into())].into()),
            ]
            .into()
        ))
    );
}

fn assert_resume_rejects_program_counter(program: &CompiledProgram, continuation: VmContinuation) {
    assert!(matches!(
        Vm::resume_from(continuation, program, &Host),
        Err(ContinuationError::InstructionPointerOutsideCodeRange { .. })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn resume_rejects_cross_function_active_and_return_instruction_pointers() {
    let program = compile_program_internal(&Program::block(vec![
        assign(
            "leaf",
            function(
                None,
                &["n"],
                &[],
                Expr::Block(vec![Expr::Print(Box::new(variable("n"))), variable("n")]),
            ),
        ),
        assign(
            "caller",
            function(
                None,
                &["n"],
                &["leaf"],
                call(variable("leaf"), vec![variable("n")]),
            ),
        ),
        assign("sibling", function(None, &["n"], &[], variable("n"))),
        Expr::Finish(Box::new(call(variable("caller"), vec![Expr::Number(1.0)]))),
    ]));
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("suspend inside leaf"),
        ExecutionOutcome::Continued
    );
    let nested = vm.suspend().expect("capture nested call");
    assert_eq!(nested.frame_stack.len(), 2);
    let leaf = nested.active_function.expect("leaf is active") as usize;
    let caller = nested.frame_stack[1]
        .function
        .expect("caller frame records its function") as usize;
    let sibling = (0..program.chunk.functions.len())
        .find(|index| *index != leaf && *index != caller)
        .expect("sibling function");
    let function_end = |index: usize| {
        program
            .chunk
            .functions
            .get(index + 1)
            .map_or(program.chunk.code.len(), |function| function.entry_ip)
    };

    let mut active_function_to_root = nested.clone();
    active_function_to_root.instruction_pointer = 0;
    assert_resume_rejects_program_counter(&program, active_function_to_root);

    let mut active_sibling = nested.clone();
    active_sibling.instruction_pointer = program.chunk.functions[sibling].entry_ip;
    assert_resume_rejects_program_counter(&program, active_sibling);

    let mut active_exact_end = nested.clone();
    active_exact_end.instruction_pointer = function_end(leaf);
    assert_resume_rejects_program_counter(&program, active_exact_end);

    let mut root_to_function = nested.clone();
    root_to_function.frame_stack.clear();
    root_to_function.active_function = None;
    root_to_function.instruction_pointer = program.chunk.functions[leaf].entry_ip;
    root_to_function.slots = nested.frame_stack[0].slots.clone();
    root_to_function.projected_slots = nested.frame_stack[0].projected_slots.clone();
    root_to_function.globals = nested.frame_stack[0].globals.clone();
    root_to_function.iterator_stack = nested.frame_stack[0].iterator_stack.clone();
    assert_resume_rejects_program_counter(&program, root_to_function);

    let mut root_exact_end = nested.clone();
    root_exact_end.frame_stack.clear();
    root_exact_end.active_function = None;
    root_exact_end.instruction_pointer = program.chunk.root_code_len;
    root_exact_end.slots = nested.frame_stack[0].slots.clone();
    root_exact_end.projected_slots = nested.frame_stack[0].projected_slots.clone();
    root_exact_end.globals = nested.frame_stack[0].globals.clone();
    root_exact_end.iterator_stack = nested.frame_stack[0].iterator_stack.clone();
    assert_resume_rejects_program_counter(&program, root_exact_end);

    let mut return_root_to_function = nested.clone();
    return_root_to_function.frame_stack[0].return_instruction_pointer =
        program.chunk.functions[leaf].entry_ip;
    assert_resume_rejects_program_counter(&program, return_root_to_function);

    let mut return_root_exact_end = nested.clone();
    return_root_exact_end.frame_stack[0].return_instruction_pointer = program.chunk.root_code_len;
    assert_resume_rejects_program_counter(&program, return_root_exact_end);

    let mut return_function_to_root = nested.clone();
    return_function_to_root.frame_stack[1].return_instruction_pointer = 0;
    assert_resume_rejects_program_counter(&program, return_function_to_root);

    let mut return_sibling = nested.clone();
    return_sibling.frame_stack[1].return_instruction_pointer =
        program.chunk.functions[sibling].entry_ip;
    assert_resume_rejects_program_counter(&program, return_sibling);

    let mut return_exact_end = nested.clone();
    return_exact_end.frame_stack[1].return_instruction_pointer = function_end(caller);
    assert_resume_rejects_program_counter(&program, return_exact_end);

    let mut non_call_return = nested;
    non_call_return.frame_stack[0].return_instruction_pointer = (1..program.chunk.root_code_len)
        .find(|return_ip| {
            !matches!(
                program.chunk.code[return_ip - 1],
                Instruction::Call { .. } | Instruction::Map
            )
        })
        .expect("root has a non-call return site");
    assert!(matches!(
        Vm::resume_from(non_call_return, &program, &host),
        Err(ContinuationError::InvalidReturnSite { .. })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn caller_and_callee_iterators_round_trip_and_corrupt_frames_fail_closed() {
    let program = compile_program_internal(&Program::block(vec![
        assign(
            "callee",
            function(
                None,
                &["values"],
                &[],
                Expr::Block(vec![
                    Expr::For {
                        binding: "inner".into(),
                        iterable: Box::new(variable("values")),
                        body: Box::new(Expr::Print(Box::new(variable("inner")))),
                    },
                    variable("values"),
                ]),
            ),
        ),
        assign(
            "caller",
            function(
                None,
                &["values"],
                &["callee"],
                Expr::Block(vec![
                    Expr::For {
                        binding: "outer".into(),
                        iterable: Box::new(variable("values")),
                        body: Box::new(call(variable("callee"), vec![variable("values")])),
                    },
                    variable("values"),
                ]),
            ),
        ),
        Expr::Finish(Box::new(call(
            variable("caller"),
            vec![Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)])],
        ))),
    ]));
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("suspend in nested loop"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("capture both iterators");
    assert_eq!(continuation.iterator_stack.len(), 1);
    let caller_frame = continuation
        .frame_stack
        .iter()
        .position(|frame| !frame.iterator_stack.is_empty())
        .expect("caller iterator is parked");
    assert_eq!(
        round_trip_and_resume(&program, continuation.clone()).await,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(1.0), Value::Number(2.0)].into()
        ))
    );

    let mut invalid_binding = continuation.clone();
    invalid_binding.frame_stack[caller_frame].iterator_stack[0].binding_slot =
        invalid_binding.frame_stack[caller_frame].slots.len();
    let invalid_binding_bytes =
        serde_json::to_vec(&invalid_binding).expect("serialize corrupt frame binding");
    assert!(
        serde_json::from_slice::<VmContinuation>(&invalid_binding_bytes)
            .expect_err("decode must reject corrupt frame binding")
            .to_string()
            .contains("binds slot")
    );
    assert!(matches!(
        Vm::resume_from(invalid_binding, &program, &host),
        Err(ContinuationError::FrameIteratorBindingOutOfBounds { .. })
    ));

    let mut zero_step = continuation;
    zero_step.frame_stack[caller_frame].iterator_stack[0].cursor = VmIteratorCursor::Range {
        next: 0,
        end: 2,
        step: 0,
    };
    let zero_step_bytes = serde_json::to_vec(&zero_step).expect("serialize zero-step frame");
    assert!(
        serde_json::from_slice::<VmContinuation>(&zero_step_bytes)
            .expect_err("decode must reject zero-step frame")
            .to_string()
            .contains("zero range step")
    );
    assert!(matches!(
        Vm::resume_from(zero_step, &program, &host),
        Err(ContinuationError::FrameZeroRangeStep { .. })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn vm_into_globals_omits_top_level_and_nested_closures_without_panicking() {
    let program = compile_program_internal(&Program::block(vec![
        assign("ordinary", Expr::Number(7.0)),
        assign("closure", function(None, &[], &[], Expr::Null)),
        assign("nested", Expr::List(vec![variable("closure")])),
    ]));
    let host = Host;
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("install fresh state");
    vm.run_for_mode().await.expect("populate globals");
    let globals = vm.into_globals().expect("materialize ordinary globals");
    assert_eq!(globals.get("ordinary"), Some(&Value::Number(7.0)));
    assert_eq!(globals.get("closure"), None);
    assert_eq!(globals.get("nested"), None);
}

#[tokio::test(flavor = "current_thread")]
async fn compiled_format_uses_template_arity_for_scalar_heap_and_closure_arguments() {
    let scalar_program = compile_program_internal(&Program::block(vec![
        assign(
            "render",
            function(
                None,
                &["left", "right"],
                &[],
                format_call("{}:{}", vec![variable("left"), variable("right")]),
            ),
        ),
        Expr::Finish(Box::new(call(
            variable("render"),
            vec![Expr::Number(1.0), Expr::Number(2.0)],
        ))),
    ]));
    assert!(scalar_program.chunk.code.iter().any(|instruction| matches!(
        instruction,
        Instruction::Intrinsic(IntrinsicOp::FormatCompiled(index))
            if *index < scalar_program.chunk.format_templates[*index].argc
    )));
    assert_eq!(
        execute_compiled(&scalar_program, &mut State::new(), &Host)
            .await
            .expect("format scalar arguments"),
        ExecutionOutcome::Finished(Value::String("1:2".into()))
    );

    let heap_program = compile_program_internal(&Program::block(vec![
        assign(
            "filler",
            function(
                None,
                &["value"],
                &[],
                format_call("{}", vec![Expr::List(vec![variable("value")])]),
            ),
        ),
        assign(
            "render",
            function(
                None,
                &["value"],
                &[],
                format_call("{}", vec![Expr::List(vec![variable("value")])]),
            ),
        ),
        Expr::Finish(Box::new(call(
            variable("render"),
            vec![Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)])],
        ))),
    ]));
    assert!(heap_program.chunk.code.iter().any(|instruction| matches!(
        instruction,
        Instruction::Intrinsic(IntrinsicOp::FormatCompiled(index))
            if *index == heap_program.chunk.format_templates[*index].argc
    )));
    assert_eq!(
        execute_compiled(&heap_program, &mut State::new(), &Host)
            .await
            .expect("format heap argument"),
        ExecutionOutcome::Finished(Value::String("[[1,2]]".into()))
    );

    let closure_program = compile_program_internal(&Program::block(vec![
        assign(
            "filler_one",
            function(
                None,
                &["value"],
                &[],
                format_call("{}", vec![Expr::List(vec![variable("value")])]),
            ),
        ),
        assign(
            "filler_two",
            function(
                None,
                &["value"],
                &[],
                format_call("{}", vec![variable("value")]),
            ),
        ),
        assign(
            "render",
            function(
                None,
                &["value"],
                &[],
                format_call("{}", vec![Expr::List(vec![variable("value")])]),
            ),
        ),
        assign("value", function(None, &[], &[], Expr::Null)),
        Expr::Finish(Box::new(call(variable("render"), vec![variable("value")]))),
    ]));
    assert!(
        closure_program
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(
                instruction,
                Instruction::Intrinsic(IntrinsicOp::FormatCompiled(index))
                    if *index > closure_program.chunk.format_templates[*index].argc
            ))
    );
    assert!(matches!(
        execute_compiled(&closure_program, &mut State::new(), &Host).await,
        Err(RuntimeError::FunctionValueAtHostBoundary)
    ));
}

struct FunctionBoundsHost {
    bounds: ExecutionBounds,
    collect_every_allocation: bool,
}

impl ExecutionHost for FunctionBoundsHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected effect")),
        }
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        self.bounds
    }

    fn collect_heap_every_allocation(&self) -> bool {
        self.collect_every_allocation
    }
}

#[tokio::test(flavor = "current_thread")]
async fn frame_depth_is_a_typed_execution_bound_and_gc_stress_preserves_closures() {
    let bounded = FunctionBoundsHost {
        bounds: ExecutionBounds::unbounded()
            .with_max_frame_depth(std::num::NonZeroU64::new(4).expect("nonzero")),
        collect_every_allocation: false,
    };
    assert!(matches!(
        execute(&factorial_program(10.0), &mut State::new(), &bounded).await,
        Err(RuntimeError::FrameDepthExceeded { limit: 4 })
    ));

    let stress = FunctionBoundsHost {
        bounds: ExecutionBounds::unbounded(),
        collect_every_allocation: true,
    };
    assert_eq!(
        execute(&factorial_program(7.0), &mut State::new(), &stress)
            .await
            .expect("GC stress recursion"),
        ExecutionOutcome::Finished(Value::Number(5_040.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn default_frame_depth_rejects_fifteen_hundred_recursive_calls() {
    let host = FunctionBoundsHost {
        bounds: ExecutionBounds::unbounded(),
        collect_every_allocation: false,
    };

    assert!(matches!(
        execute(&factorial_program(1_500.0), &mut State::new(), &host).await,
        Err(RuntimeError::FrameDepthExceeded { limit })
            if limit == DEFAULT_MAX_VM_FRAME_DEPTH.get()
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn closures_obey_the_complete_host_boundary_matrix() {
    let closure = || function(None, &[], &[], Expr::Null);
    let foreground_boundaries = [
        ("effect argument", Expr::Print(Box::new(variable("f")))),
        ("finish", Expr::Finish(Box::new(variable("f")))),
        (
            "JSON",
            Expr::BuiltinCall {
                name: "json_parse".into(),
                args: vec![variable("f")],
            },
        ),
        ("format", format_call("{}", vec![variable("f")])),
        (
            "projection",
            Expr::BuiltinCall {
                name: "to_string".into(),
                args: vec![variable("f")],
            },
        ),
        (
            "schema",
            Expr::BuiltinCall {
                name: "validate".into(),
                args: vec![
                    variable("f"),
                    Expr::TypeLiteral(Box::new(crate::ast::TypeExpr::Any)),
                ],
            },
        ),
    ];
    for (boundary, expr) in foreground_boundaries {
        let program = compile_program_internal(&Program::block(vec![assign("f", closure()), expr]));
        assert!(
            matches!(
                execute_compiled(&program, &mut State::new(), &Host).await,
                Err(RuntimeError::FunctionValueAtHostBoundary)
            ),
            "{boundary} must reject a closure before it reaches the host"
        );
    }

    for (boundary, expr) in [
        ("yield", Expr::Yield(Box::new(variable("f")))),
        ("wake", Expr::Wake(Box::new(variable("f")))),
    ] {
        let program = compile_program_internal(&Program::block(vec![assign("f", closure()), expr]));
        assert!(
            matches!(
                execute_compiled_process(
                    &program,
                    &mut State::new(),
                    &RecordingProcessHost::default()
                )
                .await,
                Err(RuntimeError::FunctionValueAtHostBoundary)
            ),
            "{boundary} must reject a closure before it reaches the host"
        );
    }

    let checkpoint_program = compile_program_internal(&Program::block(vec![
        assign("f", closure()),
        Expr::Print(Box::new(Expr::Number(1.0))),
        Expr::Finish(Box::new(Expr::Number(2.0))),
    ]));
    let checkpoint = find_instruction_continuation(&checkpoint_program, |continuation| {
        continuation
            .slots
            .iter()
            .flatten()
            .any(|value| matches!(value, Value::Ref(_)))
    })
    .await;
    let encoded = serde_json::to_vec(&checkpoint).expect("checkpoint with closure must encode");
    let decoded: VmContinuation =
        serde_json::from_slice(&encoded).expect("checkpoint with closure must decode");
    let mut vm = Vm::resume_from(decoded, &checkpoint_program, &Host)
        .expect("checkpoint with closure must resume");
    assert_eq!(
        vm.run_for_mode()
            .await
            .expect("resumed checkpoint must run"),
        ExecutionOutcome::Finished(Value::Number(2.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn closure_heap_objects_round_trip_through_state_snapshot_v3() {
    let program = Program::block(vec![Expr::If {
        condition: Box::new(variable("initialize")),
        then_block: Box::new(Expr::Block(vec![
            assign("captured", Expr::List(vec![Expr::Number(7.0)])),
            assign(
                "closure",
                function(None, &[], &["captured"], variable("captured")),
            ),
            assign("initialize", Expr::Bool(false)),
        ])),
        else_block: Box::new(Expr::Finish(Box::new(call(
            variable("closure"),
            Vec::new(),
        )))),
    }]);
    let compiled = compile_program_internal(&program);
    let mut state = State::new();
    state
        .insert_global("initialize", Value::Bool(true))
        .expect("seed initialization flag");
    assert_eq!(
        execute_compiled(&compiled, &mut state, &Host)
            .await
            .expect("store closure"),
        ExecutionOutcome::Continued
    );
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("serialize closure heap");
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("restore closure heap");
    assert_eq!(
        restored.to_canonical_bytes().expect("redump closure heap"),
        bytes
    );
    let mut restored_state = State::from_snapshot(restored);
    assert_eq!(
        execute_compiled(&compiled, &mut restored_state, &Host)
            .await
            .expect("call restored closure"),
        ExecutionOutcome::Finished(Value::List(vec![Value::Number(7.0)].into()))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_round_trip_mid_recursion_preserves_frames_heap_and_meter() {
    let program = compile_program_internal(&factorial_program(9.0));
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation.frame_stack.len() >= 3 && continuation.instructions_executed > 8
    })
    .await;
    let old_instructions = continuation.instructions_executed;
    let old_allocations = continuation.heap.allocation_counter();
    let bytes = serde_json::to_vec(&continuation).expect("serialize recursive continuation");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("deserialize recursive continuation");
    let host = Host;
    let mut vm = Vm::resume_from(restored, &program, &host).expect("resume recursive continuation");
    assert_eq!(
        vm.run_for_mode().await.expect("finish resumed recursion"),
        ExecutionOutcome::Finished(Value::Number(362_880.0))
    );
    let finished = vm.suspend().expect("capture finished meter");
    assert!(finished.instructions_executed > old_instructions);
    assert!(finished.heap.allocation_counter() >= old_allocations);
}

#[tokio::test(flavor = "current_thread")]
async fn recursive_calls_keep_occurrence_counters_stable_across_resume() {
    let linked = crate::LinkedModule::link(factorial_program(8.0), runtime_test_environment())
        .expect("AST-only function program links");
    let program = crate::compile_linked(&linked);
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation.frame_stack.len() >= 3 && !continuation.occurrence_counters.is_empty()
    })
    .await;
    let counters = continuation.occurrence_counters.clone();
    assert!(counters.values().any(|count| *count > 1));
    let restored: VmContinuation = serde_json::from_slice(
        &serde_json::to_vec(&continuation).expect("serialize occurrence continuation"),
    )
    .expect("restore occurrence continuation");
    assert_eq!(restored.occurrence_counters, counters);
    assert_eq!(
        round_trip_and_resume(&program, restored).await,
        ExecutionOutcome::Finished(Value::Number(40_320.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_callback_continuation_preserves_reentry_and_occurrence_counters() {
    let linked = crate::LinkedModule::link(
        Program::block(vec![
            assign(
                "increment",
                function(
                    None,
                    &["value"],
                    &[],
                    Expr::Binary {
                        left: Box::new(variable("value")),
                        op: BinaryOp::Add,
                        right: Box::new(Expr::Number(1.0)),
                    },
                ),
            ),
            assign(
                "callback",
                function(
                    None,
                    &["value"],
                    &["increment"],
                    call(variable("increment"), vec![variable("value")]),
                ),
            ),
            Expr::Finish(Box::new(Expr::Map {
                items: Box::new(Expr::List(vec![
                    Expr::Number(2.0),
                    Expr::Number(4.0),
                    Expr::Number(6.0),
                ])),
                function: Box::new(variable("callback")),
            })),
        ]),
        runtime_test_environment(),
    )
    .expect("AST-only callback program links");
    let program = crate::compile_linked(&linked);
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation
            .frame_stack
            .iter()
            .any(|frame| matches!(frame.return_target, VmFrameReturnContinuation::Map { .. }))
    })
    .await;
    let counters = continuation.occurrence_counters.clone();
    let restored: VmContinuation = serde_json::from_slice(
        &serde_json::to_vec(&continuation).expect("serialize callback continuation"),
    )
    .expect("restore callback continuation");
    assert_eq!(restored.occurrence_counters, counters);
    let host = Host;
    let mut vm = Vm::resume_from(restored, &program, &host).expect("resume callback continuation");
    assert_eq!(
        vm.run_for_mode().await.expect("finish resumed callbacks"),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(3.0), Value::Number(5.0), Value::Number(7.0)].into()
        ))
    );
    let finished = vm.suspend().expect("capture callback occurrence counters");
    assert!(
        finished
            .occurrence_counters
            .values()
            .any(|count| *count > 1)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn function_recursion_determinism_process_probe() {
    if std::env::var_os("LASHLANG_FUNCTION_DETERMINISM_PROBE").is_none() {
        return;
    }
    let program = compile_program_internal(&factorial_program(9.0));
    let mut continuation =
        find_instruction_continuation(&program, |continuation| continuation.frame_stack.len() >= 3)
            .await;
    continuation.active_execution_elapsed = std::time::Duration::ZERO;
    let bytes = serde_json::to_vec(&continuation).expect("serialize recursive continuation");
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("FUNCTION_DETERMINISM_DUMP={hex}");
}

#[test]
fn independent_os_processes_dump_identical_mid_recursion_continuations() {
    let executable = std::env::current_exe().expect("current test executable");
    let run_probe = || {
        let output = std::process::Command::new(&executable)
            .args([
                "--exact",
                "runtime::tests::function_recursion_determinism_process_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LASHLANG_FUNCTION_DETERMINISM_PROBE", "1")
            .output()
            .expect("spawn function determinism probe");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("probe output UTF-8")
            .lines()
            .find_map(|line| {
                line.find("FUNCTION_DETERMINISM_DUMP=")
                    .map(|index| line[index + "FUNCTION_DETERMINISM_DUMP=".len()..].to_string())
            })
            .expect("function determinism dump")
    };
    assert_eq!(run_probe(), run_probe());
}
