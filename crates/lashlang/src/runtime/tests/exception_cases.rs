fn exception_try(body: Expr, catch: Option<(&str, Expr)>, finally: Option<Expr>) -> Expr {
    Expr::Try(Box::new(crate::TryExpr {
        body: Box::new(body),
        catch: catch.map(|(binding, body)| crate::CatchClause {
            binding: binding.into(),
            body: Box::new(body),
        }),
        finally: finally.map(Box::new),
    }))
}

fn exception_finish(value: Expr) -> Program {
    Program::block(vec![Expr::Finish(Box::new(value))])
}

async fn run_exception_program<H: ExecutionHost>(
    program: Program,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    let compiled = compile_program(&program);
    let mut state = State::new();
    execute_compiled(&compiled, &mut state, host).await
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_throw_transfers_the_original_value_to_catch() {
    let program = exception_finish(exception_try(
        Expr::Throw(Box::new(Expr::String("boom".into()))),
        Some(("error", Expr::Variable("error".into()))),
        None,
    ));
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(Value::String("boom".into())))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_errors_are_caught_as_heap_backed_error_records() {
    let program = exception_finish(exception_try(
        Expr::BuiltinCall {
            name: "len".into(),
            args: vec![Expr::Number(1.0)],
        },
        Some((
            "error",
            Expr::Record(vec![
                (
                    "name".into(),
                    Expr::Field {
                        target: Box::new(Expr::Variable("error".into())),
                        field: "name".into(),
                    },
                ),
                (
                    "code".into(),
                    Expr::Field {
                        target: Box::new(Expr::Variable("error".into())),
                        field: "code".into(),
                    },
                ),
            ]),
        )),
        None,
    ));
    let ExecutionOutcome::Finished(Value::Record(error)) = run_exception_program(program, &Host)
        .await
        .expect("catch should finish")
    else {
        panic!("catch should return an error record")
    };
    assert_eq!(error["name"], Value::String("RuntimeError".into()));
    assert_eq!(error["code"], Value::String("LenUnsupported".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn effect_failure_is_a_throw_with_structured_operation_metadata() {
    let failure = Expr::ResultUnwrap(Box::new(Expr::ReceiverCall {
        receiver: Box::new(Expr::ResourceRef(crate::ResourceRefExpr::resolved(
            vec!["tools".into()],
            "Tools",
            "tools",
        ))),
        operation: "err".into(),
        args: Vec::new(),
    }));
    let operation = Expr::Field {
        target: Box::new(Expr::Field {
            target: Box::new(Expr::Variable("error".into())),
            field: "details".into(),
        }),
        field: "operation".into(),
    };
    let program = exception_finish(exception_try(failure, Some(("error", operation)), None));
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(Value::String("err".into())))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finally_runs_on_normal_and_exceptional_paths_and_a_new_throw_replaces_the_old_one() {
    let replacement = exception_try(
        exception_try(
            Expr::Throw(Box::new(Expr::String("old".into()))),
            None,
            Some(Expr::Throw(Box::new(Expr::String("new".into())))),
        ),
        Some(("error", Expr::Variable("error".into()))),
        None,
    );
    assert_eq!(
        run_exception_program(exception_finish(replacement), &Host).await,
        Ok(ExecutionOutcome::Finished(Value::String("new".into())))
    );

    let normal = exception_try(
        Expr::Number(7.0),
        None,
        Some(Expr::BuiltinCall {
            name: "len".into(),
            args: vec![Expr::String("ran".into())],
        }),
    );
    assert_eq!(
        run_exception_program(exception_finish(normal), &Host).await,
        Ok(ExecutionOutcome::Finished(Value::Number(7.0)))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn throw_unwinds_function_frames_to_the_callers_handler() {
    let function = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: Vec::new(),
        captures: Vec::new(),
        body: Box::new(Expr::Throw(Box::new(Expr::String("from callee".into())))),
    }));
    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("f".into()),
            expr: Box::new(function),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Call {
                function: Box::new(Expr::Variable("f".into())),
                args: Vec::new(),
            },
            Some(("error", Expr::Variable("error".into()))),
            None,
        ))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(Value::String(
            "from callee".into()
        )))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_round_trips_inside_try_and_finally() {
    let try_program = compile_program(&exception_finish(exception_try(
        Expr::Block(vec![
            Expr::BuiltinCall {
                name: "len".into(),
                args: vec![Expr::String("work".into())],
            },
            Expr::Number(11.0),
        ]),
        Some(("error", Expr::Number(-1.0))),
        None,
    )));
    let expected = uninterrupted_continuation_result(&try_program).await;
    let inside_try = find_instruction_continuation(&try_program, |continuation| {
        !continuation.handler_stack.is_empty() && continuation.finally_stack.is_empty()
    })
    .await;
    assert_eq!(
        round_trip_and_resume(&try_program, inside_try).await,
        expected
    );

    let finally_program = compile_program(&exception_finish(exception_try(
        Expr::Number(12.0),
        None,
        Some(Expr::Block(vec![
            Expr::BuiltinCall {
                name: "len".into(),
                args: vec![Expr::String("cleanup".into())],
            },
            Expr::Null,
        ])),
    )));
    let expected = uninterrupted_continuation_result(&finally_program).await;
    let inside_finally = find_instruction_continuation(&finally_program, |continuation| {
        !continuation.finally_stack.is_empty()
    })
    .await;
    assert_eq!(
        round_trip_and_resume(&finally_program, inside_finally).await,
        expected
    );
}

struct CancelledExceptionHost;

impl ExecutionHost for CancelledExceptionHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Host.perform(op).await
    }

    fn is_cancelled(&self) -> bool {
        true
    }
}

#[tokio::test(flavor = "current_thread")]
async fn execution_terminals_bypass_a_surrounding_catch() {
    let caught = |body| {
        exception_finish(exception_try(
            body,
            Some(("error", Expr::Number(999.0))),
            None,
        ))
    };

    let cancelled = run_exception_program(caught(Expr::Number(1.0)), &CancelledExceptionHost).await;
    assert!(matches!(cancelled, Err(RuntimeError::HostCancelled)));

    let loop_body = Expr::While {
        condition: Box::new(Expr::Bool(true)),
        body: Box::new(Expr::Null),
    };
    let instruction_env =
        ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
            ExecutionBound::instructions(8),
            ExecutionBound::Unbounded,
            ExecutionBound::Unbounded,
        ));
    assert!(matches!(
        run_exception_program(caught(loop_body.clone()), &instruction_env).await,
        Err(RuntimeError::InstructionBudgetExceeded { .. })
    ));

    // The terminal bypasses both guest handlers: the catch cannot swallow it,
    // and the finally cannot replace it with its own arbitrary completion.
    let catch_and_finally = exception_finish(exception_try(
        loop_body.clone(),
        Some(("error", Expr::Number(999.0))),
        Some(Expr::Throw(Box::new(Expr::String("finally ran".into())))),
    ));
    assert!(matches!(
        run_exception_program(catch_and_finally, &instruction_env).await,
        Err(RuntimeError::InstructionBudgetExceeded { .. })
    ));

    let deadline_env =
        ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
            ExecutionBound::Unbounded,
            ExecutionBound::Bounded(std::time::Duration::from_nanos(1)),
            ExecutionBound::Unbounded,
        ));
    assert!(matches!(
        run_exception_program(caught(loop_body), &deadline_env).await,
        Err(RuntimeError::ExecutionDeadlineExceeded { .. })
    ));

    let memory_env = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
        ExecutionBound::logical_bytes(1),
    ));
    assert!(matches!(
        run_exception_program(
            caught(Expr::List(vec![Expr::String("too large".into())])),
            &memory_env,
        )
        .await,
        Err(RuntimeError::MemoryLimitExceeded { .. })
    ));

    let recursive = Expr::Function(Box::new(crate::FunctionExpr {
        name: Some("f".into()),
        params: Vec::new(),
        captures: Vec::new(),
        body: Box::new(Expr::Call {
            function: Box::new(Expr::Variable("f".into())),
            args: Vec::new(),
        }),
    }));
    let frame_program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("f".into()),
            expr: Box::new(recursive),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Call {
                function: Box::new(Expr::Variable("f".into())),
                args: Vec::new(),
            },
            Some(("error", Expr::Number(999.0))),
            None,
        ))),
    ]);
    let frame_env = ExecutionEnvironment::new(&Host).with_execution_bounds(
        ExecutionBounds::unbounded()
            .with_max_frame_depth(std::num::NonZeroU64::new(1).expect("nonzero frame limit")),
    );
    assert!(matches!(
        run_exception_program(frame_program, &frame_env).await,
        Err(RuntimeError::FrameDepthExceeded { .. })
    ));
}

fn exception_resource_call(operation: &str, value: Expr) -> Expr {
    Expr::ResultUnwrap(Box::new(Expr::ReceiverCall {
        receiver: Box::new(Expr::ResourceRef(crate::ResourceRefExpr::resolved(
            vec!["tools".into()],
            "Tools",
            "tools",
        ))),
        operation: operation.into(),
        args: vec![Expr::Record(vec![("value".into(), value)])],
    }))
}

fn exception_function(body: Expr, captures: &[&str]) -> Expr {
    Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: Vec::new(),
        captures: captures.iter().map(|name| (*name).into()).collect(),
        body: Box::new(body),
    }))
}

#[derive(Default)]
struct ExceptionRecordingHost {
    operations: Mutex<Vec<(String, Value, Option<u64>)>>,
}

impl ExecutionHost for ExceptionRecordingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        if let AbilityOp::ResourceOperation(operation) = op {
            let value = operation
                .args
                .first()
                .and_then(Value::as_record)
                .and_then(|record| record.get("value"))
                .cloned()
                .unwrap_or(Value::Null);
            self.operations.lock_recover().push((
                operation.operation.clone(),
                value.clone(),
                operation.call_site.as_ref().map(|site| site.occurrence),
            ));
            if operation.operation == "missing" {
                return if self.operations.lock_recover().len() == 1 {
                    Err(ExecutionHostError::new("transient failure"))
                } else {
                    Ok(AbilityResult::Value(value))
                };
            }
            return Host.perform(AbilityOp::ResourceOperation(operation)).await;
        }
        Host.perform(op).await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn exception_unwind_crosses_frames_finally_chains_and_iterators() {
    let inner = exception_try(
        Expr::Throw(Box::new(Expr::String("boom".into()))),
        None,
        Some(exception_resource_call(
            "echo",
            Expr::String("inner".into()),
        )),
    );
    let outer = exception_try(
        Expr::Call {
            function: Box::new(Expr::Variable("inner".into())),
            args: Vec::new(),
        },
        None,
        Some(exception_resource_call(
            "echo",
            Expr::String("outer".into()),
        )),
    );
    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("inner".into()),
            expr: Box::new(exception_function(inner, &[])),
        },
        Expr::Assign {
            target: crate::AssignTarget::variable("outer".into()),
            expr: Box::new(exception_function(outer, &["inner"])),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Call {
                function: Box::new(Expr::Variable("outer".into())),
                args: Vec::new(),
            },
            Some(("error", Expr::Variable("error".into()))),
            None,
        ))),
    ]);
    let host = ExceptionRecordingHost::default();
    assert_eq!(
        run_exception_program(program, &host).await,
        Ok(ExecutionOutcome::Finished(Value::String("boom".into())))
    );
    assert_eq!(
        host.operations
            .lock_recover()
            .iter()
            .map(|(_, value, _)| value.clone())
            .collect::<Vec<_>>(),
        [Value::String("inner".into()), Value::String("outer".into())]
    );

    let iterator_program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("item".into()),
            expr: Box::new(Expr::String("before".into())),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::For {
                binding: "item".into(),
                iterable: Box::new(Expr::List(vec![Expr::Number(1.0)])),
                body: Box::new(Expr::Throw(Box::new(Expr::String("stop".into())))),
            },
            Some(("error", Expr::Variable("item".into()))),
            None,
        ))),
    ]);
    assert_eq!(
        run_exception_program(iterator_program, &Host).await,
        Ok(ExecutionOutcome::Finished(Value::String("before".into())))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn effect_failure_catch_retry_is_a_new_occurrence() {
    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("attempt".into()),
            expr: Box::new(Expr::Number(0.0)),
        },
        Expr::Assign {
            target: crate::AssignTarget::variable("result".into()),
            expr: Box::new(Expr::Null),
        },
        Expr::While {
            condition: Box::new(Expr::Binary {
                left: Box::new(Expr::Variable("attempt".into())),
                op: crate::BinaryOp::Less,
                right: Box::new(Expr::Number(2.0)),
            }),
            body: Box::new(Expr::Block(vec![
                Expr::Assign {
                    target: crate::AssignTarget::variable("attempt".into()),
                    expr: Box::new(Expr::Binary {
                        left: Box::new(Expr::Variable("attempt".into())),
                        op: crate::BinaryOp::Add,
                        right: Box::new(Expr::Number(1.0)),
                    }),
                },
                Expr::Assign {
                    target: crate::AssignTarget::variable("result".into()),
                    expr: Box::new(exception_try(
                        exception_resource_call("missing", Expr::Variable("attempt".into())),
                        Some(("error", Expr::Null)),
                        None,
                    )),
                },
            ])),
        },
        Expr::Finish(Box::new(Expr::Variable("result".into()))),
    ]);
    let linked = crate::LinkedModule::link(program, runtime_test_environment())
        .expect("exception program links");
    let compiled = crate::compile_linked(&linked);
    let host = ExceptionRecordingHost::default();
    assert_eq!(
        execute_compiled(&compiled, &mut State::new(), &host).await,
        Ok(ExecutionOutcome::Finished(Value::Number(2.0)))
    );
    let calls = host.operations.lock_recover().clone();
    assert_eq!(
        calls
            .iter()
            .map(|(operation, _, _)| operation.as_str())
            .collect::<Vec<_>>(),
        ["missing", "missing"]
    );
    assert_eq!(calls[0].2, Some(1));
    assert_eq!(calls[1].2, Some(2));
}

struct StressExceptionHost;

impl ExecutionHost for StressExceptionHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Host.perform(op).await
    }

    fn collect_heap_every_allocation(&self) -> bool {
        true
    }
}

async fn suspend_in_exceptional_finally<H: ExecutionHost>(
    program: &CompiledProgram,
    host: &H,
) -> VmContinuation {
    let slots = SlotState::from_globals(
        Record::new(),
        &program.chunk.slot_names,
        &ProjectedBindings::new(),
    );
    let mut vm = Vm::new_with_mode(&program.chunk, slots, host, ExecutionMode::Foreground);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode()
            .await
            .expect("finally effect should suspend"),
        ExecutionOutcome::Continued
    );
    let mut continuation = vm.suspend().expect("finally continuation");
    continuation.active_execution_elapsed = std::time::Duration::ZERO;
    assert!(matches!(
        continuation.finally_stack.as_slice(),
        [VmFinallyContinuation {
            completion: VmFinallyCompletionContinuation::Throw { .. },
            ..
        }]
    ));
    continuation
}

#[tokio::test(flavor = "current_thread")]
async fn effects_suspend_inside_finally_with_pending_errors_and_gc_stress() {
    let pending = Expr::Record(vec![
        ("name".into(), Expr::String("PendingError".into())),
        ("payload".into(), Expr::List(vec![Expr::Number(7.0)])),
    ]);
    let program = compile_program(&exception_finish(exception_try(
        exception_try(
            Expr::Throw(Box::new(pending.clone())),
            None,
            Some(exception_resource_call(
                "echo",
                Expr::String("cleanup".into()),
            )),
        ),
        Some(("error", Expr::Variable("error".into()))),
        None,
    )));
    let normal = suspend_in_exceptional_finally(&program, &Host).await;
    let stress = suspend_in_exceptional_finally(&program, &StressExceptionHost).await;
    assert_eq!(
        serde_json::to_vec(&stress).expect("stress continuation encodes"),
        serde_json::to_vec(&normal).expect("normal continuation encodes")
    );
    assert_eq!(
        round_trip_and_resume(&program, normal).await,
        ExecutionOutcome::Finished(Value::Record(Arc::new(Record::from_iter([
            ("name".to_string(), Value::String("PendingError".into())),
            (
                "payload".to_string(),
                Value::List(vec![Value::Number(7.0)].into())
            ),
        ]))))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_exception_continuations_fail_closed() {
    let function = exception_function(Expr::Null, &[]);
    let program = compile_program(&Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("f".into()),
            expr: Box::new(function),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Number(1.0),
            Some(("error", Expr::Variable("error".into()))),
            Some(Expr::Null),
        ))),
    ]));
    let base = find_instruction_continuation(&program, |continuation| {
        continuation.active_function.is_none() && continuation.handler_stack.len() == 1
    })
    .await;
    let host = Host;

    let mut out_of_range = base.clone();
    out_of_range.handler_stack[0].handler_instruction_pointer = usize::MAX;
    assert!(matches!(
        Vm::resume_from(out_of_range, &program, &host),
        Err(ContinuationError::InstructionPointerOutsideCodeRange { .. })
    ));

    let mut other_function = base.clone();
    other_function.handler_stack[0].handler_instruction_pointer =
        program.chunk.functions[0].entry_ip;
    assert!(matches!(
        Vm::resume_from(other_function, &program, &host),
        Err(ContinuationError::InstructionPointerOutsideCodeRange { .. })
    ));

    let mut bad_stack = base.clone();
    bad_stack.handler_stack[0].operand_stack_depth = bad_stack.operand_stack.len() + 1;
    assert!(matches!(
        Vm::resume_from(bad_stack, &program, &host),
        Err(ContinuationError::HandlerStackDepthOutOfBounds { .. })
    ));

    let mut bad_finally = base.clone();
    bad_finally.handler_stack[0].finally_instruction_pointer = Some(usize::MAX);
    assert!(matches!(
        Vm::resume_from(bad_finally, &program, &host),
        Err(ContinuationError::InstructionPointerOutsideCodeRange { .. })
    ));

    let mut wrong_frame = base;
    wrong_frame.handler_stack[0].frame_function = Some(0);
    assert!(matches!(
        Vm::resume_from(wrong_frame, &program, &host),
        Err(ContinuationError::HandlerFrameIdentityMismatch { .. })
    ));
}

fn compile_linked_exception_program(program: Program) -> CompiledProgram {
    let linked = crate::LinkedModule::link(program, runtime_test_environment())
        .expect("determinism program links");
    crate::compile_linked(&linked)
}

async fn exception_effect_checkpoint(
    program: &CompiledProgram,
    host: &ExceptionRecordingHost,
) -> VmContinuation {
    let mut state = State::new();
    let mut vm = Vm::from_state(program, &mut state, host).expect("checkpoint VM");
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("effect checkpoint runs"),
        ExecutionOutcome::Continued
    );
    let mut continuation = vm.suspend().expect("effect checkpoint captures");
    continuation.active_execution_elapsed = std::time::Duration::ZERO;
    continuation
}

async fn exception_determinism_dump() -> Vec<u8> {
    let inside_try = compile_linked_exception_program(exception_finish(exception_try(
        Expr::Block(vec![
            exception_resource_call("echo", Expr::String("try".into())),
            Expr::Number(1.0),
        ]),
        Some(("error", Expr::Number(-1.0))),
        None,
    )));
    let try_continuation =
        exception_effect_checkpoint(&inside_try, &ExceptionRecordingHost::default()).await;
    assert!(!try_continuation.handler_stack.is_empty());

    let inside_finally = compile_linked_exception_program(exception_finish(exception_try(
        Expr::Number(2.0),
        None,
        Some(exception_resource_call(
            "echo",
            Expr::String("finally".into()),
        )),
    )));
    let finally_continuation =
        exception_effect_checkpoint(&inside_finally, &ExceptionRecordingHost::default()).await;
    assert!(!finally_continuation.finally_stack.is_empty());

    let after_caught_failure = compile_linked_exception_program(exception_finish(exception_try(
        exception_resource_call("err", Expr::String("reject".into())),
        Some((
            "error",
            exception_resource_call("echo", Expr::String("recovered".into())),
        )),
        None,
    )));
    let caught_continuation =
        exception_effect_checkpoint(&after_caught_failure, &ExceptionRecordingHost::default())
            .await;
    assert!(caught_continuation.occurrence_counters.len() >= 2);

    serde_json::to_vec(&[try_continuation, finally_continuation, caught_continuation])
        .expect("determinism continuations encode")
}

#[tokio::test(flavor = "current_thread")]
async fn exception_determinism_process_probe() {
    if std::env::var_os("LASHLANG_EXCEPTION_DETERMINISM_PROBE").is_none() {
        return;
    }
    let dump = exception_determinism_dump().await;
    println!(
        "EXCEPTION_DETERMINISM_DUMP={}",
        dump.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
}

#[test]
fn independent_processes_dump_identical_exception_continuations() {
    let executable = std::env::current_exe().expect("current test executable");
    let run_probe = || {
        let output = std::process::Command::new(&executable)
            .args([
                "--exact",
                "runtime::tests::exception_determinism_process_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LASHLANG_EXCEPTION_DETERMINISM_PROBE", "1")
            .output()
            .expect("spawn exception determinism probe");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("probe output is UTF-8")
            .lines()
            .find_map(|line| {
                line.find("EXCEPTION_DETERMINISM_DUMP=")
                    .map(|index| &line[index + "EXCEPTION_DETERMINISM_DUMP=".len()..])
            })
            .expect("probe dump line")
            .to_string()
    };
    assert_eq!(run_probe(), run_probe());
}
