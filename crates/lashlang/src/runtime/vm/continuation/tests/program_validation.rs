use super::*;
use crate::ast::{AssignTarget, Expr, FunctionExpr, Program};
use crate::runtime::entry_points::compile_program_internal;
use crate::{AbilityOp, AbilityResult, ExecutionHostError};

struct TestHost;

impl ExecutionHost for TestHost {
    async fn perform(&self, _op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Err(ExecutionHostError::new("test host performs no effects"))
    }
}

struct CallbackHost;

impl ExecutionHost for CallbackHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Print(_) | AbilityOp::Finish(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected callback test effect")),
        }
    }
}

fn private_builtin(name: &str, args: Vec<Expr>) -> Expr {
    Expr::BuiltinCall {
        name: name.into(),
        args,
    }
}

fn callback_program() -> CompiledProgram {
    let callback = Expr::Function(Box::new(FunctionExpr {
        name: None,
        params: vec!["value".into(), "key".into(), "receiver".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::Variable("value".into()))),
            Expr::Return(Box::new(Expr::Variable("value".into()))),
        ])),
    }));
    crate::runtime::entry_points::compile_ast_with_dialect(
        &Program::block(vec![
            Expr::Assign {
                target: AssignTarget::variable("callback".into()),
                expr: Box::new(callback),
            },
            Expr::If {
                condition: Box::new(Expr::Bool(false)),
                then_block: Box::new(Expr::Map {
                    items: Box::new(Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)])),
                    function: Box::new(Expr::Variable("callback".into())),
                }),
                else_block: Box::new(Expr::Undefined),
            },
            Expr::If {
                condition: Box::new(Expr::Bool(false)),
                then_block: Box::new(private_builtin(
                    "__typescript_async_map",
                    vec![
                        Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)]),
                        Expr::Variable("callback".into()),
                    ],
                )),
                else_block: Box::new(Expr::Undefined),
            },
            private_builtin(
                "__typescript_stdlib",
                vec![
                    Expr::String("forEach".into()),
                    private_builtin(
                        "__typescript_heap_new",
                        vec![
                            Expr::String("Set".into()),
                            Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)]),
                        ],
                    ),
                    Expr::Variable("callback".into()),
                ],
            ),
            Expr::Finish(Box::new(Expr::Null)),
        ]),
        crate::CompilationDialect::Typescript,
    )
    .expect("compile callback driver program")
}

fn dynamic_call_program() -> CompiledProgram {
    let callback = Expr::Function(Box::new(FunctionExpr {
        name: None,
        params: vec!["value".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Block(vec![
            Expr::Print(Box::new(Expr::Variable("value".into()))),
            Expr::Return(Box::new(Expr::Variable("value".into()))),
        ])),
    }));
    crate::runtime::entry_points::compile_ast_with_dialect(
        &Program::block(vec![
            Expr::Assign {
                target: AssignTarget::variable("callback".into()),
                expr: Box::new(callback),
            },
            Expr::Finish(Box::new(private_builtin(
                "__typescript_call_dynamic",
                vec![
                    Expr::Variable("callback".into()),
                    Expr::List(vec![Expr::Number(1.0)]),
                ],
            ))),
        ]),
        crate::CompilationDialect::Typescript,
    )
    .expect("compile dynamic call program")
}

fn suspend_inside_first_callback(program: &CompiledProgram) -> VmContinuation {
    let mut state = State::new();
    let mut vm = Vm::from_state(program, &mut state, &CallbackHost).expect("start callback VM");
    assert_eq!(
        futures_executor::block_on(vm.run_process_until_effect()).expect("run to callback effect"),
        VmRunOutcome::EffectCompleted
    );
    vm.suspend().expect("suspend callback frame")
}

fn one_capture_program() -> CompiledProgram {
    compile_program_internal(&Program::block(vec![
        Expr::Assign {
            target: AssignTarget::variable("captured".into()),
            expr: Box::new(Expr::Number(1.0)),
        },
        Expr::Assign {
            target: AssignTarget::variable("f".into()),
            expr: Box::new(Expr::Function(Box::new(FunctionExpr {
                name: None,
                params: Vec::new(),
                captures: vec!["captured".into()],
                body: Box::new(Expr::Variable("captured".into())),
            }))),
        },
        Expr::Finish(Box::new(Expr::Call {
            function: Box::new(Expr::Variable("f".into())),
            args: Vec::new(),
        })),
    ]))
}

fn root_continuation(program: &CompiledProgram, heap: Heap, root: Option<Value>) -> VmContinuation {
    let mut continuation = empty_continuation(heap);
    continuation.slots = vec![None; program.chunk.slot_names.len()];
    continuation.projected_slots = vec![false; program.chunk.slot_names.len()];
    if let Some(root) = root {
        continuation.slots[0] = Some(root);
    }
    continuation
}

fn expect_capture_count_error(program: &CompiledProgram, continuation: VmContinuation) {
    assert!(matches!(
        Vm::resume_from(continuation, program, &TestHost),
        Err(ContinuationError::ClosureCaptureCountMismatch {
            index: 0,
            expected: 1,
            ..
        })
    ));
}

#[test]
fn resume_validates_closures_in_active_frames_globals_and_nested_containers() {
    let program = one_capture_program();

    for captures in [Vec::new(), vec![Value::Null, Value::Bool(true)]] {
        let mut heap = Heap::default();
        let closure = heap
            .allocate(HeapObject::Closure {
                function: 0,
                captures,
            })
            .expect("allocate malformed closure");
        expect_capture_count_error(&program, root_continuation(&program, heap, Some(closure)));
    }

    let mut global_heap = Heap::default();
    let global_closure = global_heap
        .allocate(HeapObject::Closure {
            function: 0,
            captures: Vec::new(),
        })
        .expect("allocate global closure");
    let mut global = root_continuation(&program, global_heap, None);
    global.globals.insert("f".to_string(), global_closure);
    expect_capture_count_error(&program, global);

    let mut nested_heap = Heap::default();
    let nested_closure = nested_heap
        .allocate(HeapObject::Closure {
            function: 0,
            captures: Vec::new(),
        })
        .expect("allocate nested closure");
    let nested_record = nested_heap
        .allocate(HeapObject::Record(Box::new({
            let mut record = Record::new();
            record.insert("closure".to_string(), nested_closure);
            record
        })))
        .expect("allocate nested record");
    let nested_list = nested_heap
        .allocate(HeapObject::List(vec![nested_record]))
        .expect("allocate nested list");
    expect_capture_count_error(
        &program,
        root_continuation(&program, nested_heap, Some(nested_list)),
    );

    let mut frame_heap = Heap::default();
    let frame_closure = frame_heap
        .allocate(HeapObject::Closure {
            function: 0,
            captures: Vec::new(),
        })
        .expect("allocate frame closure");
    let function = &program.chunk.functions[0];
    let call_ip = program
        .chunk
        .code
        .iter()
        .take(program.chunk.root_code_len)
        .position(|instruction| matches!(instruction, Instruction::Call { .. }))
        .expect("root call instruction");
    let mut frame = root_continuation(&program, frame_heap, None);
    frame.active_function = Some(0);
    frame.instruction_pointer = function.entry_ip;
    frame.slots = vec![None; function.slot_names.len()];
    frame.projected_slots = vec![false; function.slot_names.len()];
    let mut caller_slots = vec![None; program.chunk.slot_names.len()];
    caller_slots[0] = Some(frame_closure);
    frame.frame_stack.push(VmFrameContinuation {
        return_instruction_pointer: call_ip + 1,
        function: None,
        operand_stack_base: 0,
        slots: caller_slots,
        projected_slots: vec![false; program.chunk.slot_names.len()],
        globals: Record::new(),
        iterator_stack: Vec::new(),
        return_target: VmFrameReturnContinuation::Direct,
    });
    expect_capture_count_error(&program, frame);
}

#[test]
fn resume_reports_unknown_closure_function_indices_by_name() {
    let program = one_capture_program();
    let mut heap = Heap::default();
    let closure = heap
        .allocate(HeapObject::Closure {
            function: 99,
            captures: Vec::new(),
        })
        .expect("allocate unknown closure");
    assert!(matches!(
        Vm::resume_from(
            root_continuation(&program, heap, Some(closure)),
            &program,
            &TestHost,
        ),
        Err(ContinuationError::UnknownFunction { index: 99 })
    ));
}

#[test]
fn callback_continuations_validate_cursor_effect_policy_and_return_site_mode() {
    let program = callback_program();
    let foreach = suspend_inside_first_callback(&program);
    Vm::resume_from(foreach.clone(), &program, &CallbackHost)
        .expect("authentic forEach callback continuation resumes");

    let map_return_ip = program
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Map))
        .expect("program contains a Map return site")
        + 1;
    let mut map = foreach.clone();
    map.frame_stack[0].return_instruction_pointer = map_return_ip;
    let VmFrameReturnContinuation::Callback {
        completion,
        allow_effects,
        ..
    } = &mut map.frame_stack[0].return_target
    else {
        panic!("driver must use callback return target")
    };
    *completion = VmCallbackCompletion::Collect;
    *allow_effects = false;
    Vm::resume_from(map.clone(), &program, &CallbackHost)
        .expect("authentic Map callback continuation resumes");

    let async_map_return_ip = program
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::AsyncMap))
        .expect("program contains an AsyncMap return site")
        + 1;
    let mut async_map = foreach.clone();
    async_map.frame_stack[0].return_instruction_pointer = async_map_return_ip;
    let VmFrameReturnContinuation::Callback {
        completion,
        allow_effects,
        ..
    } = &mut async_map.frame_stack[0].return_target
    else {
        panic!("async map must use callback return target")
    };
    *completion = VmCallbackCompletion::Collect;
    *allow_effects = true;
    Vm::resume_from(async_map.clone(), &program, &CallbackHost)
        .expect("authentic AsyncMap callback continuation resumes");

    let mut async_map_sync_policy = async_map;
    let VmFrameReturnContinuation::Callback { allow_effects, .. } =
        &mut async_map_sync_policy.frame_stack[0].return_target
    else {
        panic!("async map must use callback return target")
    };
    *allow_effects = false;
    assert!(matches!(
        Vm::resume_from(async_map_sync_policy, &program, &CallbackHost),
        Err(ContinuationError::InvalidReturnSite { .. })
    ));

    let mut map_effects = map.clone();
    let VmFrameReturnContinuation::Callback { allow_effects, .. } =
        &mut map_effects.frame_stack[0].return_target
    else {
        panic!("Map must use callback return target")
    };
    *allow_effects = true;
    assert!(matches!(
        Vm::resume_from(map_effects, &program, &CallbackHost),
        Err(ContinuationError::InvalidReturnSite { .. })
    ));

    let mut map_mode = map;
    let VmFrameReturnContinuation::Callback { completion, .. } =
        &mut map_mode.frame_stack[0].return_target
    else {
        panic!("Map must use callback return target")
    };
    *completion = VmCallbackCompletion::Discard;
    assert!(matches!(
        Vm::resume_from(map_mode, &program, &CallbackHost),
        Err(ContinuationError::InvalidReturnSite { .. })
    ));

    let mut zero_cursor = foreach.clone();
    let VmFrameReturnContinuation::Callback { next_index, .. } =
        &mut zero_cursor.frame_stack[0].return_target
    else {
        panic!("forEach must use callback return target")
    };
    *next_index = 0;
    assert!(
        validate_continuation(&zero_cursor)
            .expect_err("Discard callback cannot replay calls[0]")
            .to_string()
            .contains("invalid callback cursor")
    );

    let mut foreach_effects = foreach.clone();
    let VmFrameReturnContinuation::Callback { allow_effects, .. } =
        &mut foreach_effects.frame_stack[0].return_target
    else {
        panic!("forEach must use callback return target")
    };
    *allow_effects = false;
    assert!(matches!(
        Vm::resume_from(foreach_effects, &program, &CallbackHost),
        Err(ContinuationError::InvalidReturnSite { .. })
    ));

    let mut foreach_mode = foreach;
    let VmFrameReturnContinuation::Callback { completion, .. } =
        &mut foreach_mode.frame_stack[0].return_target
    else {
        panic!("forEach must use callback return target")
    };
    *completion = VmCallbackCompletion::Collect;
    assert!(matches!(
        Vm::resume_from(foreach_mode, &program, &CallbackHost),
        Err(ContinuationError::InvalidReturnSite { .. })
    ));
}

#[test]
fn dynamic_call_continuation_accepts_only_a_dynamic_or_static_direct_return_site() {
    let program = dynamic_call_program();
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &CallbackHost).expect("start dynamic call");
    assert_eq!(
        futures_executor::block_on(vm.run_process_until_effect()).expect("park in dynamic callee"),
        VmRunOutcome::EffectCompleted
    );
    let continuation = vm.suspend().expect("suspend dynamic callee");
    Vm::resume_from(continuation.clone(), &program, &CallbackHost)
        .expect("authentic dynamic call frame resumes");

    let forged_return_ip = program
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::MakeClosure { .. }))
        .expect("program creates a closure")
        + 1;
    let mut forged = continuation;
    forged.frame_stack[0].return_instruction_pointer = forged_return_ip;
    assert!(matches!(
        Vm::resume_from(forged, &program, &CallbackHost),
        Err(ContinuationError::InvalidReturnSite { .. })
    ));
}
