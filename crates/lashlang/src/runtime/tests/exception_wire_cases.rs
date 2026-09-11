use super::*;

// Structural validation of the durable handler and finally stacks. The VM
// trusts their ordering absolutely — `throw_value` picks the catch target by
// position and derives the cleanup set from it — so an authored blob whose
// ordering is impossible has to be refused rather than executed.

/// A blob whose exception stacks are structurally impossible must be refused,
/// whether the shape is visible to decode-time validation or only once the
/// compiled program's scope extents are in hand.
fn assert_exception_wire_refused(
    program: &CompiledProgram,
    continuation: VmContinuation,
    expected: &str,
) {
    let bytes = serde_json::to_vec(&continuation).expect("authored continuation encodes");
    match serde_json::from_slice::<VmContinuation>(&bytes) {
        Err(error) => assert!(
            error.to_string().contains(expected),
            "decode refused for the wrong reason: {error}"
        ),
        Ok(decoded) => {
            let host = Host;
            let error = Vm::resume_from(decoded, program, &host)
                .err()
                .unwrap_or_else(|| panic!("the authored continuation must be refused"));
            assert!(
                error.to_string().contains(expected),
                "resume refused for the wrong reason: {error}"
            );
        }
    }
}

/// A caller with its own catch calls a function whose catch is live. Inverting
/// the two records puts an inner frame's handler below an outer frame's, which
/// no execution can produce.
#[tokio::test(flavor = "current_thread")]
async fn a_non_monotonic_handler_stack_is_refused() {
    let inner = exception_function(
        exception_try(
            Expr::Throw(Box::new(Expr::String("boom".into()))),
            Some(("error", Expr::String("inner".into()))),
            None,
        ),
        &[],
    );
    let program = compile_program(&Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("f".into()),
            expr: Box::new(inner),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Call {
                function: Box::new(Expr::Variable("f".into())),
                args: Vec::new(),
            },
            Some(("error", Expr::String("outer".into()))),
            None,
        ))),
    ]));
    let function = &program.chunk.functions[0];
    let push_handler = |range: std::ops::Range<usize>| {
        program.chunk.code[range]
            .iter()
            .find_map(|instruction| match instruction {
                Instruction::PushHandler {
                    handler,
                    finally,
                    catches,
                } => Some((*handler, *finally, *catches)),
                _ => None,
            })
            .expect("a push_handler instruction")
    };
    let outer = push_handler(0..program.chunk.root_code_len);
    let inner = push_handler(function.entry_ip..function.end_ip);
    let call_ip = program.chunk.code[..program.chunk.root_code_len]
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Call { .. }))
        .expect("root call instruction");
    let throw_ip = program.chunk.code[function.entry_ip..function.end_ip]
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Throw))
        .map(|offset| function.entry_ip + offset)
        .expect("inner throw instruction");

    let handler = |(handler_instruction_pointer, finally_instruction_pointer, catches),
                   frame_depth,
                   frame_function| VmHandlerContinuation {
        handler_instruction_pointer,
        finally_instruction_pointer,
        catches,
        frame_depth,
        frame_function,
        operand_stack_depth: 0,
        iterator_stack_depth: 0,
    };
    let authored = VmContinuation {
        pending_tools: Vec::new(),
        execution_nonce: 0,
        format_version: VM_CONTINUATION_FORMAT_VERSION,
        reference_semantics: false,
        instruction_pointer: throw_ip,
        active_function: Some(0),
        operand_stack: vec![Value::String("boom".into())],
        last_value: None,
        slots: vec![None; function.slot_names.len()],
        projected_slots: vec![false; function.slot_names.len()],
        globals: Record::new(),
        iterator_stack: Vec::new(),
        frame_stack: vec![VmFrameContinuation {
            return_instruction_pointer: call_ip + 1,
            function: None,
            operand_stack_base: 0,
            slots: vec![None; program.chunk.slot_names.len()],
            projected_slots: vec![false; program.chunk.slot_names.len()],
            globals: Record::new(),
            iterator_stack: Vec::new(),
            return_target: VmFrameReturnContinuation::Direct,
        }],
        // Deliberately impossible: the inner frame's handler sits below the
        // outer frame's. Each record is independently in range.
        handler_stack: vec![handler(inner, 1, Some(0)), handler(outer, 0, None)],
        finally_stack: Vec::new(),
        occurrence_counters: Default::default(),
        mode: ExecutionMode::Foreground,
        profile: None,
        pending_error_span: None,
        instructions_executed: 0,
        active_execution_elapsed: std::time::Duration::ZERO,
        heap: VmHeapContinuation::default(),
    };
    assert_exception_wire_refused(&program, authored, "not nested inside");
}

/// The sharper shape: both handlers live in the *same* frame, so every
/// per-record invariant survives the swap. Only the scope extents the compiler
/// emitted can tell the honest order from the reordered one, and executing the
/// reordered one silently skips a mandatory cleanup effect.
#[tokio::test(flavor = "current_thread")]
async fn a_same_frame_handler_swap_is_refused() {
    let program = compile_program(&exception_finish(exception_try(
        exception_try(
            Expr::Throw(Box::new(Expr::String("boom".into()))),
            None,
            Some(exception_resource_call(
                "echo",
                Expr::String("cleanup".into()),
            )),
        ),
        Some(("error", Expr::Variable("error".into()))),
        None,
    )));

    let honest_host = ExceptionRecordingHost::default();
    let mut state = State::new();
    execute_compiled(&program, &mut state, &honest_host)
        .await
        .expect("the honest run finishes");
    assert_eq!(
        honest_host.operations.lock_recover().len(),
        1,
        "the honest run performs the cleanup effect once"
    );

    let base = find_instruction_continuation(&program, |continuation| {
        continuation.handler_stack.len() == 2
    })
    .await;
    let mut swapped = base;
    swapped.handler_stack.swap(0, 1);
    assert_exception_wire_refused(&program, swapped, "not nested inside");
}

/// Frame return sites are already checked against the call that produced them.
/// Handler targets get the same treatment: an authored handler pointing one
/// instruction past its catch entry names no scope the compiler emitted.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_target_that_is_not_a_scope_entry_is_refused() {
    let program = compile_program(&exception_finish(exception_try(
        Expr::Throw(Box::new(Expr::String("boom".into()))),
        Some(("error", Expr::Variable("error".into()))),
        None,
    )));
    let base = find_instruction_continuation(&program, |continuation| {
        continuation.handler_stack.len() == 1
    })
    .await;
    let mut skewed = base;
    skewed.handler_stack[0].handler_instruction_pointer += 1;
    assert_exception_wire_refused(&program, skewed, "names no exception scope");
}

/// A handler whose recorded finally target does not belong to the scope its
/// handler target names is equally unrepresentable.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_finally_target_from_another_scope_is_refused() {
    let program = compile_program(&exception_finish(exception_try(
        Expr::Throw(Box::new(Expr::String("boom".into()))),
        Some(("error", Expr::Variable("error".into()))),
        None,
    )));
    let base = find_instruction_continuation(&program, |continuation| {
        continuation.handler_stack.len() == 1
    })
    .await;
    let mut forged = base;
    forged.handler_stack[0].finally_instruction_pointer = Some(0);
    assert_exception_wire_refused(&program, forged, "names no exception scope");
}

/// The finally stack is a nesting structure too: its handler depths and frame
/// depths grow with the chain, so a decreasing pair is impossible state.
#[tokio::test(flavor = "current_thread")]
async fn a_non_monotonic_finally_stack_is_refused() {
    let program = compile_program(&exception_finish(exception_try(
        exception_try(
            exception_try(
                Expr::Throw(Box::new(Expr::String("boom".into()))),
                None,
                Some(exception_resource_call("echo", Expr::String("b".into()))),
            ),
            None,
            Some(exception_resource_call("echo", Expr::String("a".into()))),
        ),
        Some(("error", Expr::Variable("error".into()))),
        None,
    )));
    let base = find_instruction_continuation(&program, |continuation| {
        continuation.finally_stack.len() == 1
    })
    .await;
    let mut authored = base;
    let mut deeper = authored.finally_stack[0].clone();
    deeper.handler_stack_depth = authored.finally_stack[0]
        .handler_stack_depth
        .saturating_sub(1);
    // Appended after the record it claims to be nested inside, with a smaller
    // handler depth: the chain would unwind outwards then inwards.
    authored.finally_stack.push(deeper);
    assert_exception_wire_refused(&program, authored, "not nested inside");
}

/// Nesting is not the only thing a durable handler can lie about. A single
/// handler on the stack is unanchored unless its scope is tied to a code
/// position: it need only name *some* scope the compiler emitted in the right
/// function. Two sibling cleanup-only scopes make that concrete — renaming the
/// live handler to its sibling runs the wrong cleanup and skips the mandatory
/// one, which is the same harm the nesting rule was written to prevent.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_naming_a_sibling_scope_is_refused() {
    let program = compile_program(&Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("a".into()),
            expr: Box::new(exception_try(
                Expr::Number(1.0),
                None,
                Some(exception_resource_call("echo", Expr::String("A".into()))),
            )),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::BuiltinCall {
                name: "len".into(),
                args: vec![Expr::Number(1.0)],
            },
            None,
            Some(exception_resource_call("echo", Expr::String("B".into()))),
        ))),
    ]));
    let scopes = program
        .chunk
        .code
        .iter()
        .filter_map(|instruction| match instruction {
            Instruction::PushHandler {
                handler,
                finally,
                catches,
            } => Some((*handler, *finally, *catches)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(scopes.len(), 2, "two sibling cleanup-only scopes");

    let base = find_instruction_continuation(&program, |continuation| {
        continuation.handler_stack.len() == 1
            && continuation.handler_stack[0].handler_instruction_pointer == scopes[1].0
    })
    .await;
    let mut forged = base;
    forged.handler_stack[0].handler_instruction_pointer = scopes[0].0;
    forged.handler_stack[0].finally_instruction_pointer = scopes[0].1;
    forged.handler_stack[0].catches = scopes[0].2;
    assert_exception_wire_refused(&program, forged, "is not live at");
}

/// The same substitution with catch handlers: the resumed VM must not be able
/// to enter an unrelated catch body.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_naming_an_unrelated_catch_scope_is_refused() {
    let program = compile_program(&Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("first".into()),
            expr: Box::new(exception_try(
                Expr::Throw(Box::new(Expr::String("one".into()))),
                Some(("first_error", Expr::String("first".into()))),
                None,
            )),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Throw(Box::new(Expr::String("two".into()))),
            Some(("second_error", Expr::String("second".into()))),
            None,
        ))),
    ]));
    let scopes = program
        .chunk
        .code
        .iter()
        .filter_map(|instruction| match instruction {
            Instruction::PushHandler {
                handler,
                finally,
                catches,
            } => Some((*handler, *finally, *catches)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(scopes.len(), 2, "two independent try scopes");

    let base = find_instruction_continuation(&program, |continuation| {
        continuation.handler_stack.len() == 1
            && continuation.handler_stack[0].handler_instruction_pointer == scopes[0].0
    })
    .await;
    let mut forged = base;
    forged.handler_stack[0].handler_instruction_pointer = scopes[1].0;
    forged.handler_stack[0].finally_instruction_pointer = scopes[1].1;
    forged.handler_stack[0].catches = scopes[1].2;
    assert_exception_wire_refused(&program, forged, "is not live at");
}

/// `InvalidExceptionState` is the VM's own signal that the bytecode violated
/// the handler/finally discipline. Routing it back through the handler stack
/// would hand an internal invariant violation to the very structure that is
/// already suspect, so it must be an uncatchable terminal.
#[tokio::test(flavor = "current_thread")]
async fn an_invalid_exception_state_bypasses_a_surrounding_catch() {
    assert_eq!(
        RuntimeError::InvalidExceptionState {
            reason: "handler stack underflow".into()
        }
        .taxonomy(),
        ErrorTaxonomy::UncatchableTerminal
    );

    // A hand-built chunk whose `PopHandler` has no handler to pop, wrapped in a
    // catch that must never observe the failure.
    let mut program = compile_program(&exception_finish(exception_try(
        Expr::Number(1.0),
        Some(("error", Expr::String("caught".into()))),
        None,
    )));
    let body = program
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::PushNumber(_)))
        .expect("the try body pushes its constant");
    program.chunk.code[body] = Instruction::PopHandler;
    let outcome = execute_compiled(&program, &mut State::new(), &Host).await;
    assert!(
        matches!(outcome, Err(RuntimeError::InvalidExceptionState { .. })),
        "the surrounding catch must not observe it: {outcome:?}"
    );
}

// The shapes the scope extents cannot see. A handler's extent is a region, so
// every record in the next two blobs sits inside the scope it names and passes
// every per-record and nesting rule above. What they get wrong is which
// handlers are installed *at the instruction the frame is sitting at*, which is
// flow-sensitive — so the chain the lowerer recorded is what refuses them.

/// A mandatory cleanup omitted from a nested chain. Both scopes are live at the
/// suspension; dropping the outer record from the blob leaves a chain that is
/// still perfectly nested, and resuming it would skip the outer cleanup that
/// the honest run performs.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_omitted_from_a_nested_chain_is_refused() {
    let program = compile_program(&exception_finish(exception_try(
        exception_try(
            exception_resource_call("echo", Expr::String("body".into())),
            None,
            Some(exception_resource_call(
                "echo",
                Expr::String("inner".into()),
            )),
        ),
        None,
        Some(exception_resource_call(
            "echo",
            Expr::String("outer".into()),
        )),
    )));

    let honest_host = ExceptionRecordingHost::default();
    let mut state = State::new();
    execute_compiled(&program, &mut state, &honest_host)
        .await
        .expect("the honest run finishes");
    assert_eq!(
        honest_host
            .operations
            .lock_recover()
            .iter()
            .map(|(_, value, _)| value.clone())
            .collect::<Vec<_>>(),
        vec![
            Value::String("body".into()),
            Value::String("inner".into()),
            Value::String("outer".into()),
        ],
        "the honest run performs both cleanups"
    );

    let base = find_instruction_continuation(&program, |continuation| {
        continuation.handler_stack.len() == 2 && continuation.finally_stack.is_empty()
    })
    .await;
    let mut omitted = base;
    omitted.handler_stack.remove(0);
    assert_exception_wire_refused(&program, omitted, "is not the chain the compiled program");
}

/// A scope re-installed while its own cleanup runs. The finally body lies
/// inside the scope's extent, so the record is live by every region test, but
/// no execution can reach that instruction with the handler installed: the
/// cleanup it names is already running, and resuming would run it a second
/// time.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_reinstalled_during_its_own_cleanup_is_refused() {
    let program = compile_program(&exception_finish(exception_try(
        Expr::Number(1.0),
        None,
        Some(exception_resource_call(
            "echo",
            Expr::String("cleanup".into()),
        )),
    )));
    let scope = program
        .chunk
        .code
        .iter()
        .find_map(|instruction| match instruction {
            Instruction::PushHandler {
                handler,
                finally,
                catches,
            } => Some((*handler, *finally, *catches)),
            _ => None,
        })
        .expect("the cleanup-only scope");

    let base = find_instruction_continuation(&program, |continuation| {
        continuation.finally_stack.len() == 1 && continuation.handler_stack.is_empty()
    })
    .await;
    let mut reinstalled = base;
    reinstalled.handler_stack.push(VmHandlerContinuation {
        handler_instruction_pointer: scope.0,
        finally_instruction_pointer: scope.1,
        catches: scope.2,
        frame_depth: reinstalled.frame_stack.len(),
        frame_function: reinstalled.active_function,
        operand_stack_depth: 0,
        iterator_stack_depth: 0,
    });
    assert_exception_wire_refused(
        &program,
        reinstalled,
        "is not the chain the compiled program",
    );
}

/// The other half of the chain rule: it must reject nothing an execution can
/// actually produce. Exception scopes are left by more than the normal edge —
/// `break` and `return` unwind them one instruction at a time, and a `finally`
/// body runs with its own handler off the stack — so the recorded chain is
/// swept against every honest instruction boundary of a program that uses all
/// of them, each captured continuation encoded, decoded and resumed.
#[tokio::test(flavor = "current_thread")]
async fn every_honest_exception_boundary_resumes() {
    let returning = exception_function(
        exception_try(
            Expr::Block(vec![
                exception_resource_call("echo", Expr::String("in-call".into())),
                Expr::Return(Box::new(Expr::Number(7.0))),
            ]),
            None,
            Some(exception_resource_call(
                "echo",
                Expr::String("return-cleanup".into()),
            )),
        ),
        &[],
    );
    let program = compile_program(&Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("f".into()),
            expr: Box::new(returning),
        },
        Expr::Assign {
            target: crate::AssignTarget::variable("n".into()),
            expr: Box::new(Expr::Number(0.0)),
        },
        // A loop whose `break` leaves a nested cleanup scope from inside the
        // body, wrapped in a cleanup of its own.
        exception_try(
            Expr::While {
                condition: Box::new(Expr::Bool(true)),
                body: Box::new(exception_try(
                    Expr::Block(vec![
                        Expr::Assign {
                            target: crate::AssignTarget::variable("n".into()),
                            expr: Box::new(Expr::Binary {
                                left: Box::new(Expr::Variable("n".into())),
                                op: crate::ast::BinaryOp::Add,
                                right: Box::new(Expr::Number(1.0)),
                            }),
                        },
                        Expr::If {
                            condition: Box::new(Expr::Binary {
                                left: Box::new(Expr::Variable("n".into())),
                                op: crate::ast::BinaryOp::Greater,
                                right: Box::new(Expr::Number(2.0)),
                            }),
                            then_block: Box::new(Expr::Break),
                            else_block: Box::new(Expr::If {
                                condition: Box::new(Expr::Binary {
                                    left: Box::new(Expr::Variable("n".into())),
                                    op: crate::ast::BinaryOp::Equal,
                                    right: Box::new(Expr::Number(1.0)),
                                }),
                                then_block: Box::new(Expr::Continue),
                                else_block: Box::new(exception_resource_call(
                                    "echo",
                                    Expr::String("spin".into()),
                                )),
                            }),
                        },
                    ]),
                    None,
                    Some(exception_resource_call(
                        "echo",
                        Expr::String("loop-cleanup".into()),
                    )),
                )),
            },
            None,
            Some(exception_resource_call(
                "echo",
                Expr::String("after-loop".into()),
            )),
        ),
        // A catch body with its own cleanup scope, entered by a throw.
        Expr::Assign {
            target: crate::AssignTarget::variable("caught".into()),
            expr: Box::new(exception_try(
                Expr::Throw(Box::new(Expr::String("boom".into()))),
                Some((
                    "error",
                    exception_resource_call("echo", Expr::Variable("error".into())),
                )),
                Some(exception_resource_call(
                    "echo",
                    Expr::String("outer-cleanup".into()),
                )),
            )),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Call {
                function: Box::new(Expr::Variable("f".into())),
                args: Vec::new(),
            },
            None,
            Some(exception_resource_call(
                "echo",
                Expr::String("call-cleanup".into()),
            )),
        ))),
    ]));

    let expected = uninterrupted_continuation_result(&program).await;
    let host = Host;
    let mut resumed = 0usize;
    let mut caller_frame_handlers = 0usize;
    let mut inside_cleanup = 0usize;
    for budget in 1..=program.chunk.code.len() * 4 {
        let mut vm = continuation_test_vm(&program, &host);
        vm.suspend_after_instructions(budget);
        if !matches!(vm.run_for_mode().await, Ok(ExecutionOutcome::Continued)) {
            continue;
        }
        let continuation = vm
            .suspend()
            .unwrap_or_else(|error| panic!("boundary {budget} must be capturable: {error}"));
        if continuation
            .handler_stack
            .iter()
            .any(|handler| handler.frame_depth < continuation.frame_stack.len())
        {
            caller_frame_handlers += 1;
        }
        if !continuation.finally_stack.is_empty() {
            inside_cleanup += 1;
        }
        let bytes = serde_json::to_vec(&continuation).expect("encode");
        let decoded = serde_json::from_slice::<VmContinuation>(&bytes).expect("decode");
        let mut restored = Vm::resume_from(decoded, &program, &host)
            .unwrap_or_else(|error| panic!("boundary {budget} must resume: {error}"));
        assert_eq!(
            restored
                .run_for_mode()
                .await
                .unwrap_or_else(|error| panic!("boundary {budget} must finish: {error}")),
            expected,
            "boundary {budget} finished differently"
        );
        resumed += 1;
    }
    assert!(resumed > 20, "the sweep resumed only {resumed} boundaries");
    // The two anchors the rule uses, both covered by honest state: a frame
    // holding a handler while a call it made is suspended, and a frame sitting
    // inside a cleanup body with that scope's handler already gone.
    assert!(
        caller_frame_handlers > 0,
        "no boundary suspended inside a call made from a protected region"
    );
    assert!(
        inside_cleanup > 0,
        "no boundary suspended inside a cleanup body"
    );
}
