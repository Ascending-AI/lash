// Loop control flow (`break` / `continue`) crossing structured exception
// scopes. Every case here pins ECMA-262 completion semantics: leaving a
// protected region by a jump pops its handler and runs the pending `finally`
// blocks, innermost first, up to the target loop.

fn control_flow_append(list: &str, item: &str) -> Expr {
    Expr::Assign {
        target: crate::AssignTarget::variable(list.into()),
        expr: Box::new(Expr::Binary {
            left: Box::new(Expr::Variable(list.into())),
            op: crate::ast::BinaryOp::Add,
            right: Box::new(Expr::List(vec![Expr::String(item.into())])),
        }),
    }
}

fn control_flow_empty_list(list: &str) -> Expr {
    Expr::Assign {
        target: crate::AssignTarget::variable(list.into()),
        expr: Box::new(Expr::List(Vec::new())),
    }
}

fn control_flow_strings(items: &[&str]) -> Value {
    Value::List(
        items
            .iter()
            .map(|item| Value::String((*item).into()))
            .collect::<Vec<_>>()
            .into(),
    )
}

fn control_flow_for(binding: &str, count: usize, body: Expr) -> Expr {
    Expr::For {
        binding: binding.into(),
        iterable: Box::new(Expr::List(
            (0..count).map(|index| Expr::Number(index as f64)).collect(),
        )),
        body: Box::new(body),
    }
}

/// A `while` loop that runs its body exactly once, used where a `for` loop's
/// iterator depth would mask a leaked handler.
fn control_flow_while_once(counter: &str, body: Vec<Expr>) -> Vec<Expr> {
    let mut block = vec![Expr::Assign {
        target: crate::AssignTarget::variable(counter.into()),
        expr: Box::new(Expr::Number(1.0)),
    }];
    block.extend(body);
    vec![
        Expr::Assign {
            target: crate::AssignTarget::variable(counter.into()),
            expr: Box::new(Expr::Number(0.0)),
        },
        Expr::While {
            condition: Box::new(Expr::Binary {
                left: Box::new(Expr::Variable(counter.into())),
                op: crate::ast::BinaryOp::Less,
                right: Box::new(Expr::Number(1.0)),
            }),
            body: Box::new(Expr::Block(block)),
        },
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn break_out_of_a_try_runs_the_pending_finally() {
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            2,
            exception_try(
                Expr::Break,
                None,
                Some(control_flow_append("log", "cleanup")),
            ),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "cleanup"
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_handler_left_by_break_must_not_capture_a_later_unrelated_throw() {
    let mut block = control_flow_while_once(
        "n",
        vec![exception_try(
            Expr::Break,
            Some((
                "e",
                Expr::Finish(Box::new(Expr::String("stale-handler-fired".into()))),
            )),
            None,
        )],
    );
    block.push(Expr::Finish(Box::new(Expr::Throw(Box::new(Expr::String(
        "escaped".into(),
    ))))));
    let outcome = run_exception_program(Program::block(block), &Host).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::UncaughtException { value }) if *value == Value::String("escaped".into())),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_cleanup_handler_left_by_break_must_not_rerun_its_finally() {
    let mut block = vec![Expr::Assign {
        target: crate::AssignTarget::variable("runs".into()),
        expr: Box::new(Expr::Number(0.0)),
    }];
    block.extend(control_flow_while_once(
        "n",
        vec![exception_try(
            Expr::Break,
            None,
            Some(Expr::Assign {
                target: crate::AssignTarget::variable("runs".into()),
                expr: Box::new(Expr::Binary {
                    left: Box::new(Expr::Variable("runs".into())),
                    op: crate::ast::BinaryOp::Add,
                    right: Box::new(Expr::Number(1.0)),
                }),
            }),
        )],
    ));
    block.push(Expr::Finish(Box::new(Expr::Throw(Box::new(Expr::String(
        "escaped".into(),
    ))))));
    let outcome = run_exception_program(Program::block(block), &Host).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::UncaughtException { value }) if *value == Value::String("escaped".into())),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_out_of_a_for_loop_try_does_not_leak_the_handler() {
    let program = Program::block(vec![
        control_flow_for(
            "i",
            1,
            exception_try(
                Expr::Break,
                Some(("e", Expr::String("stale-handler-fired".into()))),
                None,
            ),
        ),
        Expr::Finish(Box::new(Expr::Throw(Box::new(Expr::String(
            "escaped".into(),
        ))))),
    ]);
    let outcome = run_exception_program(program, &Host).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::UncaughtException { value }) if *value == Value::String("escaped".into())),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn continue_out_of_a_try_does_not_leak_handlers() {
    let program = Program::block(vec![
        control_flow_for(
            "i",
            3,
            exception_try(
                Expr::Continue,
                Some(("e", Expr::String("stale-handler-fired".into()))),
                None,
            ),
        ),
        Expr::Finish(Box::new(Expr::Throw(Box::new(Expr::String(
            "escaped".into(),
        ))))),
    ]);
    let outcome = run_exception_program(program, &Host).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::UncaughtException { value }) if *value == Value::String("escaped".into())),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn continue_out_of_a_try_runs_the_pending_finally_on_every_iteration() {
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            3,
            exception_try(
                Expr::Continue,
                None,
                Some(control_flow_append("log", "cleanup")),
            ),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "cleanup", "cleanup", "cleanup"
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_out_of_a_catch_body_runs_the_pending_finally() {
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            1,
            exception_try(
                Expr::Throw(Box::new(Expr::String("x".into()))),
                Some(("e", Expr::Break)),
                Some(control_flow_append("log", "cleanup")),
            ),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "cleanup"
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_through_nested_trys_runs_every_finally_inner_to_outer() {
    let inner = exception_try(
        exception_try(
            exception_try(Expr::Break, None, Some(control_flow_append("log", "inner"))),
            None,
            Some(control_flow_append("log", "middle")),
        ),
        Some(("e", Expr::Null)),
        Some(control_flow_append("log", "outer")),
    );
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for("i", 2, inner),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "inner", "middle", "outer"
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_in_nested_loops_unwinds_only_to_its_own_loop() {
    let inner_loop = control_flow_for(
        "j",
        3,
        exception_try(Expr::Break, None, Some(control_flow_append("log", "inner"))),
    );
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            2,
            exception_try(inner_loop, None, Some(control_flow_append("log", "outer"))),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    // Each outer iteration enters the outer try, breaks out of the inner try
    // (running only `inner`), leaves the inner loop, and then exits the outer
    // try normally (running `outer`).
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "inner", "outer", "inner", "outer"
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_inside_a_finally_replaces_the_pending_normal_completion() {
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            3,
            Expr::Block(vec![
                exception_try(
                    Expr::Null,
                    None,
                    Some(Expr::Block(vec![
                        control_flow_append("log", "cleanup"),
                        Expr::Break,
                    ])),
                ),
                control_flow_append("log", "unreachable"),
            ]),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "cleanup"
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_inside_a_finally_discards_the_pending_throw() {
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            3,
            exception_try(
                Expr::Throw(Box::new(Expr::String("boom".into()))),
                None,
                Some(Expr::Block(vec![
                    control_flow_append("log", "cleanup"),
                    Expr::Break,
                ])),
            ),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "cleanup"
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn continue_inside_a_finally_replaces_the_pending_completion_each_iteration() {
    let program = Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            2,
            Expr::Block(vec![
                exception_try(
                    Expr::Throw(Box::new(Expr::String("boom".into()))),
                    None,
                    Some(Expr::Block(vec![
                        control_flow_append("log", "cleanup"),
                        Expr::Continue,
                    ])),
                ),
                control_flow_append("log", "unreachable"),
            ]),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(control_flow_strings(&[
            "cleanup", "cleanup"
        ])))
    );
}

/// Captures the last durable continuation before the program finishes. A jump
/// edge that leaked exception state shows up here as a handler or finally
/// record that outlived its protected region.
async fn control_flow_final_continuation(program: &CompiledProgram) -> VmContinuation {
    let host = Host;
    let mut last = None;
    for budget in 1..=program.chunk.code.len() * 8 {
        let mut vm = continuation_test_vm(program, &host);
        vm.suspend_after_instructions(budget);
        if !matches!(vm.run_for_mode().await, Ok(ExecutionOutcome::Continued)) {
            continue;
        }
        let continuation = vm.suspend().expect("capture a continuation");
        let bytes = serde_json::to_vec(&continuation).expect("encode");
        last = Some(serde_json::from_slice::<VmContinuation>(&bytes).expect("decode"));
    }
    last.expect("at least one continuation must be captured")
}

fn assert_no_pending_exception_state(continuation: &VmContinuation) {
    assert!(
        continuation.handler_stack.is_empty(),
        "handler stack outlived its protected region at ip {}: {:?}",
        continuation.instruction_pointer,
        continuation.handler_stack
    );
    assert!(
        continuation.finally_stack.is_empty(),
        "finally stack outlived its protected region at ip {}: {:?}",
        continuation.instruction_pointer,
        continuation.finally_stack
    );
}

/// A `break` edge must leave nothing behind in the durable continuation.
#[tokio::test(flavor = "current_thread")]
async fn break_leaves_no_stale_handler_in_the_durable_continuation() {
    let mut block = control_flow_while_once(
        "n",
        vec![exception_try(
            Expr::Break,
            Some(("e", Expr::Number(7.0))),
            None,
        )],
    );
    block.push(Expr::Finish(Box::new(Expr::Number(0.0))));
    let program = compile_program(&Program::block(block));
    assert_no_pending_exception_state(&control_flow_final_continuation(&program).await);
}

/// A `break` out of a `finally` body must not leave the abandoned completion
/// on the finally stack.
#[tokio::test(flavor = "current_thread")]
async fn break_inside_a_finally_leaves_no_pending_completion() {
    let mut block = control_flow_while_once(
        "n",
        vec![exception_try(
            Expr::Throw(Box::new(Expr::String("boom".into()))),
            None,
            Some(Expr::Block(vec![Expr::Number(1.0), Expr::Break])),
        )],
    );
    block.push(Expr::Finish(Box::new(Expr::Number(0.0))));
    let program = compile_program(&Program::block(block));
    assert_no_pending_exception_state(&control_flow_final_continuation(&program).await);
}

/// Suspending inside a `finally` that is running because of a `break` must
/// resume to the same completion: the pending jump is bytecode, not VM state,
/// so the continuation carries an ordinary `Normal` completion.
#[tokio::test(flavor = "current_thread")]
async fn suspension_inside_a_finally_entered_by_break_resumes_to_the_break() {
    let program = compile_program(&Program::block(vec![
        control_flow_empty_list("log"),
        control_flow_for(
            "i",
            3,
            exception_try(
                Expr::Break,
                None,
                Some(Expr::Block(vec![
                    exception_resource_call("echo", Expr::String("cleanup".into())),
                    control_flow_append("log", "cleanup"),
                ])),
            ),
        ),
        Expr::Finish(Box::new(Expr::Variable("log".into()))),
    ]));

    let stressed = {
        let host = StressExceptionHost;
        let slots = SlotState::from_globals(
            Record::new(),
            &program.chunk.slot_names,
            &ProjectedBindings::new(),
        );
        let mut vm = Vm::new_with_mode(&program.chunk, slots, &host, ExecutionMode::Foreground);
        vm.suspend_after_effects(1);
        assert_eq!(
            vm.run_for_mode().await.expect("cleanup effect suspends"),
            ExecutionOutcome::Continued
        );
        let mut continuation = vm.suspend().expect("cleanup continuation");
        continuation.active_execution_elapsed = std::time::Duration::ZERO;
        continuation
    };
    assert_eq!(
        stressed.finally_stack.len(),
        1,
        "the finally is pending: {:?}",
        stressed.finally_stack
    );
    assert!(
        matches!(
            stressed.finally_stack[0].completion,
            VmFinallyCompletionContinuation::Normal { .. }
        ),
        "a break-driven finally carries a normal completion: {:?}",
        stressed.finally_stack[0].completion
    );
    assert_eq!(
        round_trip_and_resume(&program, stressed).await,
        ExecutionOutcome::Finished(control_flow_strings(&["cleanup"]))
    );
}

// ---------------------------------------------------------------------------
// Error identity through a cleanup-only unwind.
// ---------------------------------------------------------------------------

/// Wrapping an expression in `try { … } finally { … }` must not change which
/// error the host sees. `UncaughtException` is reserved for values thrown by an
/// explicit `throw`; a runtime failure that nothing catches keeps its variant.
#[tokio::test(flavor = "current_thread")]
async fn a_cleanup_only_scope_preserves_the_runtime_error_identity() {
    let failing = || Expr::BuiltinCall {
        name: "len".into(),
        args: vec![Expr::Number(1.0)],
    };
    let bare = run_exception_program(exception_finish(failing()), &Host)
        .await
        .expect_err("len(1) fails");
    let wrapped = run_exception_program(
        exception_finish(exception_try(failing(), None, Some(Expr::Number(0.0)))),
        &Host,
    )
    .await
    .expect_err("len(1) still fails inside a cleanup-only scope");
    assert_eq!(bare, wrapped, "bare={bare:?} wrapped={wrapped:?}");
    assert_eq!(bare, RuntimeError::LenUnsupported);
}

/// The same for an effect failure: host-side classification is variant-based,
/// so a cleanup-only scope must not reclassify a failed effect as a guest
/// exception.
#[tokio::test(flavor = "current_thread")]
async fn a_cleanup_only_scope_preserves_an_effect_failure_identity() {
    let failing = || exception_resource_call("err", Expr::String("x".into()));
    let bare = run_exception_program(exception_finish(failing()), &Host)
        .await
        .expect_err("the effect fails");
    let wrapped = run_exception_program(
        exception_finish(exception_try(failing(), None, Some(Expr::Number(0.0)))),
        &Host,
    )
    .await
    .expect_err("the effect still fails inside a cleanup-only scope");
    assert_eq!(bare, wrapped, "bare={bare:?} wrapped={wrapped:?}");
    assert!(
        matches!(bare, RuntimeError::UnwrappedModuleOperationFailed { .. }),
        "{bare:?}"
    );
}

/// The trap keeps pointing at the failing expression rather than at the
/// cleanup block that ran on the way out. Spans are attached directly to the
/// compiled instructions so the assertion is about attribution, not parsing.
#[tokio::test(flavor = "current_thread")]
async fn a_cleanup_only_scope_keeps_the_failing_expression_span() {
    let mut program = compile_program(&exception_finish(exception_try(
        Expr::BuiltinCall {
            name: "len".into(),
            args: vec![Expr::Number(1.0)],
        },
        None,
        Some(Expr::Number(0.0)),
    )));
    let failing_ip = program
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Intrinsic(IntrinsicOp::Len)))
        .expect("the program compiles a `len` intrinsic");
    let failing_span = Span { start: 11, end: 17 };
    for (ip, span) in program.chunk.spans.iter_mut().enumerate() {
        *span = Some(if ip == failing_ip {
            failing_span
        } else {
            Span { start: 90, end: 99 }
        });
    }
    let host = Host;
    let slots = SlotState::from_globals(
        Record::new(),
        &program.chunk.slot_names,
        &ProjectedBindings::new(),
    );
    let mut vm = Vm::new_with_mode(&program.chunk, slots, &host, ExecutionMode::Foreground);
    let failure = vm
        .run_traced_for_mode()
        .await
        .expect_err("the wrapped program still fails");
    assert_eq!(failure.error, RuntimeError::LenUnsupported);
    assert_eq!(failure.span, Some(failing_span));
}

/// A cleanup chain that suspends mid-`finally` must resume to the same
/// identity: the pending origin is durable, not VM-local.
#[tokio::test(flavor = "current_thread")]
async fn a_suspended_cleanup_chain_resumes_with_the_original_error() {
    let program = compile_program(&exception_finish(exception_try(
        Expr::BuiltinCall {
            name: "len".into(),
            args: vec![Expr::Number(1.0)],
        },
        None,
        Some(exception_resource_call(
            "echo",
            Expr::String("cleanup".into()),
        )),
    )));
    let host = Host;
    let slots = SlotState::from_globals(
        Record::new(),
        &program.chunk.slot_names,
        &ProjectedBindings::new(),
    );
    let mut vm = Vm::new_with_mode(&program.chunk, slots, &host, ExecutionMode::Foreground);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode()
            .await
            .expect("the cleanup effect suspends"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("cleanup continuation");
    let bytes = serde_json::to_vec(&continuation).expect("encode");
    let restored: VmContinuation = serde_json::from_slice(&bytes).expect("decode");
    let mut resumed = Vm::resume_from(restored, &program, &host).expect("resume");
    assert_eq!(
        resumed.run_for_mode().await,
        Err(RuntimeError::LenUnsupported)
    );
}

// ---------------------------------------------------------------------------
// Process terminals inside a protected region.
//
// `finish` and `fail` end the process; they are not function returns, and
// ECMA's analogue (`process.exit`) skips `finally` too. FIG-1303 defers the
// question deliberately rather than deciding process-terminal-vs-completion
// semantics in a layer with no `return` to test against. These tests pin the
// current behaviour so the FIG-1304/1305 constraint — a TypeScript `return`
// must lower to a real function return, never to `Expr::Finish` — trips a red
// test if it is ever violated, instead of silently dropping cleanups.
// ---------------------------------------------------------------------------

async fn control_flow_terminal_cleanups(terminal: Expr) -> (String, usize) {
    let host = ExceptionRecordingHost::default();
    let program = Program::block(vec![exception_try(
        terminal,
        None,
        Some(exception_resource_call(
            "echo",
            Expr::String("cleanup".into()),
        )),
    )]);
    let compiled = compile_program(&program);
    let slots = SlotState::from_globals(
        Record::new(),
        &compiled.chunk.slot_names,
        &ProjectedBindings::new(),
    );
    let mut vm = Vm::new_with_mode(&compiled.chunk, slots, &host, ExecutionMode::Process);
    let outcome = format!("{:?}", vm.run_for_mode().await);
    let cleanups = host.operations.lock_recover().len();
    (outcome, cleanups)
}

#[tokio::test(flavor = "current_thread")]
async fn finish_inside_a_try_does_not_run_the_finally() {
    let (outcome, cleanups) =
        control_flow_terminal_cleanups(Expr::Finish(Box::new(Expr::String("done".into())))).await;
    assert!(outcome.contains("Finished"), "{outcome}");
    assert_eq!(
        cleanups, 0,
        "DEFERRED (FIG-1303): `finish` is a process terminal and skips the finally. \
         If this is now 1, a lowering has started routing a return through `finish`; \
         decide the semantics before changing this number."
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fail_inside_a_try_does_not_run_the_finally() {
    let (outcome, cleanups) =
        control_flow_terminal_cleanups(Expr::Fail(Box::new(Expr::String("bad".into())))).await;
    assert!(outcome.contains("Failed"), "{outcome}");
    assert_eq!(
        cleanups, 0,
        "DEFERRED (FIG-1303): `fail` is a process terminal and skips the finally. \
         See `finish_inside_a_try_does_not_run_the_finally`."
    );
}
