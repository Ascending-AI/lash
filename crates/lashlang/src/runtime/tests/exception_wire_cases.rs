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
        format_version: VM_CONTINUATION_FORMAT_VERSION,
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
