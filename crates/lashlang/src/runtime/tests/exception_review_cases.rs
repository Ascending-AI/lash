// Cases folded in from the FIG-1303 adversarial review round. Each one covers
// an interaction the layer's own suite left open: unwinding across a builtin
// callback frame, the aliasing of a thrown durable slot, terminals raised from
// inside the exception machinery itself, and the renderer's refusal that keeps
// AST-only exception nodes out of the source-language projections.

/// Unwinding across a `ReturnTarget::Map` frame must reach the outer catch and
/// leave the abandoned callback machinery behind cleanly.
#[tokio::test(flavor = "current_thread")]
async fn a_throw_escapes_a_builtin_map_callback() {
    let callback = Expr::Function(Box::new(crate::FunctionExpr {
        name: None,
        params: vec!["item".into()],
        captures: Vec::new(),
        body: Box::new(Expr::Throw(Box::new(Expr::String("from map".into())))),
    }));
    let program = exception_finish(exception_try(
        Expr::Map {
            items: Box::new(Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)])),
            function: Box::new(callback),
        },
        Some(("e", Expr::Variable("e".into()))),
        None,
    ));
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(Value::String("from map".into())))
    );
}

/// `throw x` hands the catch binding the value the source slot still owns, so
/// every instruction boundary of the transfer has to stay capturable: a second
/// durable owner of one heap object would violate the persisted forest rule.
#[tokio::test(flavor = "current_thread")]
async fn every_boundary_of_a_caught_throw_stays_capturable() {
    let program = compile_program(&Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("x".into()),
            expr: Box::new(Expr::List(vec![Expr::Number(1.0), Expr::Number(2.0)])),
        },
        Expr::Assign {
            target: crate::AssignTarget::variable("caught".into()),
            expr: Box::new(exception_try(
                Expr::Throw(Box::new(Expr::Variable("x".into()))),
                Some(("e", Expr::Variable("e".into()))),
                None,
            )),
        },
        Expr::Finish(Box::new(Expr::Number(0.0))),
    ]));
    let host = Host;
    let mut captured = 0usize;
    for budget in 1..=program.chunk.code.len() * 4 {
        let mut vm = continuation_test_vm(&program, &host);
        vm.suspend_after_instructions(budget);
        if !matches!(vm.run_for_mode().await, Ok(ExecutionOutcome::Continued)) {
            continue;
        }
        vm.suspend()
            .unwrap_or_else(|error| panic!("boundary {budget} must be capturable: {error}"));
        captured += 1;
    }
    assert!(captured > 5, "the probe captured {captured} boundaries");
}

/// The catch binding holds a copy, so mutating it cannot reach back into the
/// slot the value was thrown from.
#[tokio::test(flavor = "current_thread")]
async fn mutating_the_catch_binding_leaves_the_thrown_slot_alone() {
    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("x".into()),
            expr: Box::new(Expr::List(vec![Expr::Number(1.0)])),
        },
        Expr::Assign {
            target: crate::AssignTarget::variable("ignored".into()),
            expr: Box::new(exception_try(
                Expr::Throw(Box::new(Expr::Variable("x".into()))),
                Some((
                    "e",
                    Expr::Assign {
                        target: crate::AssignTarget::variable("e".into()),
                        expr: Box::new(Expr::Binary {
                            left: Box::new(Expr::Variable("e".into())),
                            op: crate::ast::BinaryOp::Add,
                            right: Box::new(Expr::List(vec![Expr::Number(99.0)])),
                        }),
                    },
                )),
                None,
            )),
        },
        Expr::Finish(Box::new(Expr::Variable("x".into()))),
    ]);
    assert_eq!(
        run_exception_program(program, &Host).await,
        Ok(ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(1.0)].into()
        )))
    );
}

/// The memory limit stays terminal when it is the error-record import itself
/// that exhausts it: the taxonomy cannot be inverted by making the machinery
/// that builds the guest's error value fail.
#[tokio::test(flavor = "current_thread")]
async fn memory_exhaustion_while_importing_the_error_record_stays_terminal() {
    for limit in [1u64, 16, 64, 128] {
        let env = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
            ExecutionBound::Unbounded,
            ExecutionBound::Unbounded,
            ExecutionBound::logical_bytes(limit),
        ));
        let program = exception_finish(exception_try(
            Expr::BuiltinCall {
                name: "len".into(),
                args: vec![Expr::Number(1.0)],
            },
            Some(("e", Expr::Number(999.0))),
            None,
        ));
        let outcome = run_exception_program(program, &env).await;
        assert!(
            matches!(outcome, Err(RuntimeError::MemoryLimitExceeded { .. })),
            "limit {limit} produced {outcome:?}"
        );
    }
}

/// Frame-depth exhaustion raised by a call inside a catch body is a terminal,
/// so the enclosing catch must not see it.
#[tokio::test(flavor = "current_thread")]
async fn frame_depth_exhaustion_inside_a_catch_body_is_terminal() {
    let recursive = Expr::Function(Box::new(crate::FunctionExpr {
        name: Some("f".into()),
        params: Vec::new(),
        captures: Vec::new(),
        body: Box::new(Expr::Call {
            function: Box::new(Expr::Variable("f".into())),
            args: Vec::new(),
        }),
    }));
    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("f".into()),
            expr: Box::new(recursive),
        },
        Expr::Finish(Box::new(exception_try(
            exception_try(
                Expr::Throw(Box::new(Expr::String("outer".into()))),
                Some((
                    "inner",
                    Expr::Call {
                        function: Box::new(Expr::Variable("f".into())),
                        args: Vec::new(),
                    },
                )),
                None,
            ),
            Some(("e", Expr::Number(999.0))),
            None,
        ))),
    ]);
    let env = ExecutionEnvironment::new(&Host).with_execution_bounds(
        ExecutionBounds::unbounded()
            .with_max_frame_depth(std::num::NonZeroU64::new(2).expect("nonzero")),
    );
    let outcome = run_exception_program(program, &env).await;
    assert!(
        matches!(outcome, Err(RuntimeError::FrameDepthExceeded { .. })),
        "{outcome:?}"
    );
}

/// A host that starts reporting cancellation once the cleanup chain is already
/// running must still terminate the process rather than land in the catch.
struct CancelAfterFirstEffectHost {
    calls: AtomicUsize,
}

impl ExecutionHost for CancelAfterFirstEffectHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Host.perform(op).await
    }

    fn is_cancelled(&self) -> bool {
        self.calls.load(Ordering::SeqCst) >= 1
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_observed_mid_unwind_is_terminal() {
    let program = exception_finish(exception_try(
        exception_try(
            Expr::Throw(Box::new(Expr::String("boom".into()))),
            None,
            Some(exception_resource_call(
                "echo",
                Expr::String("cleanup".into()),
            )),
        ),
        Some(("e", Expr::Variable("e".into()))),
        None,
    ));
    let host = CancelAfterFirstEffectHost {
        calls: AtomicUsize::new(0),
    };
    let outcome = run_exception_program(program, &host).await;
    assert!(
        matches!(outcome, Err(RuntimeError::HostCancelled)),
        "{outcome:?}"
    );
}

/// Suspension inside a *catch* body, which the layer's own suite does not
/// cover: the caught record is live, and stress collection must not change the
/// bytes the continuation encodes to.
#[tokio::test(flavor = "current_thread")]
async fn suspension_inside_a_catch_body_is_byte_identical_under_gc_stress() {
    let thrown = Expr::Record(vec![
        ("name".into(), Expr::String("CaughtError".into())),
        ("payload".into(), Expr::List(vec![Expr::Number(3.0)])),
    ]);
    let program = compile_program(&exception_finish(exception_try(
        Expr::Throw(Box::new(thrown)),
        Some((
            "error",
            Expr::Block(vec![
                exception_resource_call("echo", Expr::String("in-catch".into())),
                Expr::Variable("error".into()),
            ]),
        )),
        None,
    )));

    async fn suspend_in_catch<H: ExecutionHost>(
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
            vm.run_for_mode().await.expect("the catch effect suspends"),
            ExecutionOutcome::Continued
        );
        let mut continuation = vm.suspend().expect("catch continuation");
        continuation.active_execution_elapsed = std::time::Duration::ZERO;
        continuation
    }

    let normal = suspend_in_catch(&program, &Host).await;
    let stress = suspend_in_catch(&program, &StressExceptionHost).await;
    assert_eq!(
        serde_json::to_vec(&stress).expect("stress encodes"),
        serde_json::to_vec(&normal).expect("normal encodes"),
        "GC stress must not change the catch-body continuation bytes"
    );
    assert_eq!(
        round_trip_and_resume(&program, normal).await,
        ExecutionOutcome::Finished(Value::Record(Arc::new(Record::from_iter([
            ("name".to_string(), Value::String("CaughtError".into())),
            (
                "payload".to_string(),
                Value::List(vec![Value::Number(3.0)].into())
            ),
        ]))))
    );
}

/// A cleanup chain that crosses a process boundary runs each cleanup exactly
/// once overall: suspend after the first cleanup effect, drop the VM and the
/// host, and resume from the bytes with a fresh recording host.
#[tokio::test(flavor = "current_thread")]
async fn a_cleanup_chain_is_exactly_once_across_a_process_boundary() {
    let thrown = Expr::Record(vec![("name".into(), Expr::String("E".into()))]);
    let callee = exception_function(
        exception_try(
            exception_try(
                Expr::Throw(Box::new(thrown)),
                None,
                Some(exception_resource_call("echo", Expr::String("B".into()))),
            ),
            None,
            Some(exception_resource_call("echo", Expr::String("A".into()))),
        ),
        &[],
    );
    let program = compile_program(&Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("f".into()),
            expr: Box::new(callee),
        },
        Expr::Finish(Box::new(exception_try(
            Expr::Call {
                function: Box::new(Expr::Variable("f".into())),
                args: Vec::new(),
            },
            Some(("error", Expr::Variable("error".into()))),
            None,
        ))),
    ]));

    let first = ExceptionRecordingHost::default();
    let bytes = {
        let slots = SlotState::from_globals(
            Record::new(),
            &program.chunk.slot_names,
            &ProjectedBindings::new(),
        );
        let mut vm = Vm::new_with_mode(&program.chunk, slots, &first, ExecutionMode::Foreground);
        vm.suspend_after_effects(1);
        assert_eq!(
            vm.run_for_mode().await.expect("the first leg suspends"),
            ExecutionOutcome::Continued
        );
        serde_json::to_vec(&vm.suspend().expect("mid-unwind continuation")).expect("encode")
    };
    let first_calls = first.operations.lock_recover().len();

    let second = ExceptionRecordingHost::default();
    let restored: VmContinuation = serde_json::from_slice(&bytes).expect("decode");
    let mut vm = Vm::resume_from(restored, &program, &second).expect("resume in a fresh process");
    let outcome = vm.run_for_mode().await;
    let second_calls = second.operations.lock_recover().len();

    assert_eq!(
        first_calls + second_calls,
        2,
        "each cleanup effect runs exactly once across the boundary"
    );
    assert_eq!(
        outcome,
        Ok(ExecutionOutcome::Finished(Value::Record(Arc::new(
            Record::from_iter([("name".to_string(), Value::String("E".into()))])
        ))))
    );
}

/// `Try` and `Throw` decline canonical source at every nesting, which is what
/// keeps them out of the source-language projections such as the workflow
/// graph. Textual `?` is unaffected.
#[test]
fn the_renderer_declines_try_and_throw_at_every_nesting() {
    let nested = Expr::Record(vec![(
        "field".into(),
        Expr::Binary {
            op: crate::BinaryOp::Add,
            left: Box::new(Expr::Number(1.0)),
            right: Box::new(exception_try(Expr::Number(2.0), None, Some(Expr::Null))),
        },
    )]);
    assert!(matches!(
        crate::canonical_expression_source(&nested)
            .expect_err("a nested try must decline canonical source"),
        crate::CanonicalSourceError::NonSourceableExpression { .. }
    ));

    let nested_throw = Expr::List(vec![Expr::Throw(Box::new(Expr::Number(1.0)))]);
    assert!(matches!(
        crate::canonical_expression_source(&nested_throw)
            .expect_err("a nested throw must decline canonical source"),
        crate::CanonicalSourceError::NonSourceableExpression { .. }
    ));

    let unwrap = Expr::ResultUnwrap(Box::new(Expr::Variable("value".into())));
    assert_eq!(
        crate::canonical_expression_source(&unwrap).expect("`?` renders"),
        "value?"
    );
}

/// A v2-shaped blob must fail closed rather than default its missing stacks,
/// and rolling the version back on an otherwise valid body is refused too.
#[tokio::test(flavor = "current_thread")]
async fn a_v2_shaped_continuation_fails_closed() {
    let program = compile_program(&exception_finish(exception_try(
        Expr::Throw(Box::new(Expr::String("boom".into()))),
        Some(("error", Expr::Variable("error".into()))),
        None,
    )));
    let live = find_instruction_continuation(&program, |continuation| {
        continuation.handler_stack.len() == 1
    })
    .await;

    let mut shaped = serde_json::to_value(&live).expect("wire");
    shaped["format_version"] = serde_json::json!(2);
    let object = shaped.as_object_mut().expect("object");
    object.remove("handler_stack");
    object.remove("finally_stack");
    let error = serde_json::from_value::<VmContinuation>(shaped)
        .expect_err("a v2-shaped continuation must be rejected");
    assert!(error.to_string().contains("handler_stack"), "{error}");

    let mut versioned = serde_json::to_value(&live).expect("wire");
    versioned["format_version"] = serde_json::json!(2);
    let error = serde_json::from_value::<VmContinuation>(versioned)
        .expect_err("a rolled-back format version must be rejected");
    assert!(error.to_string().contains("format version 2"), "{error}");
}
