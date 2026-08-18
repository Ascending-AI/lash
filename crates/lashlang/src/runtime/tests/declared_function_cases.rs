// Laws that keep declared `fn` invisible to durability.
//
// A declared function is the one construct that adds a call frame to programs
// written in plain lashlang, so it is the construct most likely to disturb
// effect identity or the shape of a captured continuation. Both properties are
// load-bearing for exactly-once replay, so they are pinned here rather than
// left to follow from the linker's effect ban by argument alone.

use crate::ast::{Declaration, FunctionDecl, FunctionParam, TypeExpr};

fn declared(
    name: &str,
    params: &[(&str, TypeExpr)],
    return_ty: TypeExpr,
    body: Expr,
) -> Declaration {
    Declaration::Function(FunctionDecl {
        name: name.into(),
        params: params
            .iter()
            .map(|(name, ty)| FunctionParam {
                name: (*name).into(),
                ty: ty.clone(),
            })
            .collect(),
        return_ty,
        body,
    })
}

fn with_declarations(declarations: Vec<Declaration>, main: Vec<Expr>) -> Program {
    let mut program = Program::block(main);
    program.declarations = declarations;
    program
}

/// `print "a"`, then a declared call, then `print "b"`, then finish.
fn effects_around_a_call() -> Program {
    with_declarations(
        vec![declared(
            "twice",
            &[("n", TypeExpr::Float)],
            TypeExpr::Float,
            Expr::Binary {
                op: BinaryOp::Multiply,
                left: Box::new(Expr::Variable("n".into())),
                right: Box::new(Expr::Number(2.0)),
            },
        )],
        vec![
            Expr::Print(Box::new(Expr::String("a".into()))),
            Expr::Assign {
                target: AssignTarget::variable("doubled".into()),
                expr: Box::new(Expr::FunctionCall {
                    function: "twice".into(),
                    args: vec![Expr::Number(21.0)],
                }),
            },
            Expr::Print(Box::new(Expr::String("b".into()))),
            Expr::Finish(Box::new(Expr::Variable("doubled".into()))),
        ],
    )
}

/// A smoke check, not the law. The law is the linker's effect ban: a body that
/// cannot contain an effect cannot suspend on one, and this program's body is
/// one such body, so the sweep can only confirm the expected shape -- it can
/// never catch a violation, because no program that reaches the VM is allowed
/// to express one. `a_process_name_is_rejected_in_a_function` and the rest of
/// the `ForbiddenInFunction` suite are what actually hold the line.
#[tokio::test(flavor = "current_thread")]
async fn effect_suspensions_around_a_declared_call_keep_an_empty_frame_stack() {
    // Confirms the shape a mid-turn continuation has when a declared function
    // is in play: the root frame is the active one at every effect boundary,
    // exactly as it was before the feature existed.
    let program = compile_program_internal(&effects_around_a_call());
    for effects in 1..=2 {
        let host = Host;
        let mut vm = continuation_test_vm(&program, &host);
        vm.suspend_after_effects(effects);
        assert_eq!(
            vm.run_for_mode().await.expect("execution should suspend"),
            ExecutionOutcome::Continued
        );
        let continuation = vm.suspend().expect("VM state should be capturable");
        assert!(
            continuation.frame_stack.is_empty(),
            "effect {effects} suspended inside a declared-function frame: {:?}",
            continuation.frame_stack
        );
        assert_eq!(continuation.active_function, None);
        assert_eq!(
            round_trip_and_resume(&program, continuation).await,
            ExecutionOutcome::Finished(Value::Number(42.0))
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_declared_call_still_round_trips_from_any_instruction_boundary() {
    // Effects cannot suspend inside the body, but a budget-driven capture can
    // land anywhere, so the frame has to survive serialization all the same.
    let program = compile_program_internal(&effects_around_a_call());
    let mut saw_frame = false;
    for budget in 1..=program.chunk.code.len() * 4 {
        let host = Host;
        let mut vm = continuation_test_vm(&program, &host);
        vm.suspend_after_instructions(budget);
        if vm.run_for_mode().await.expect("execution should not fail")
            != ExecutionOutcome::Continued
        {
            break;
        }
        let continuation = vm.suspend().expect("VM state should be capturable");
        saw_frame |= !continuation.frame_stack.is_empty();
        assert_eq!(
            round_trip_and_resume(&program, continuation).await,
            ExecutionOutcome::Finished(Value::Number(42.0)),
            "resume diverged at budget {budget}"
        );
    }
    assert!(
        saw_frame,
        "the sweep never entered the function, so it proved nothing"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn declaring_a_function_leaves_the_root_chunk_unchanged() {
    // Declarations compile after the root code is recorded, so adding one
    // cannot shift a root instruction pointer — which is what keeps a
    // continuation captured by an older revision resumable against the same
    // main.
    let main = vec![
        Expr::Print(Box::new(Expr::String("a".into()))),
        Expr::Finish(Box::new(Expr::Number(1.0))),
    ];
    let bare = compile_program_internal(&Program::block(main.clone()));
    let with_function = compile_program_internal(&with_declarations(
        vec![declared(
            "unused",
            &[("n", TypeExpr::Float)],
            TypeExpr::Float,
            Expr::Variable("n".into()),
        )],
        main,
    ));

    let root = bare.chunk.root_code_len;
    assert_eq!(with_function.chunk.root_code_len, root);
    // `Instruction` is deliberately not `PartialEq` (it is a versioned
    // surface), so the root stream is compared through the two side tables
    // that carry everything replay keys on: the effect-site identity of each
    // instruction and its source span.
    assert_eq!(
        with_function.chunk.lashlang_execution_sites[..root],
        bare.chunk.lashlang_execution_sites[..root],
        "declaring a function moved a root effect site"
    );
    assert_eq!(
        with_function.chunk.spans[..root],
        bare.chunk.spans[..root],
        "declaring a function rewrote the root instruction stream"
    );
    assert_eq!(
        with_function.chunk.slot_names.len(),
        bare.chunk.slot_names.len(),
        "declaring a function added a root slot"
    );
    assert!(
        with_function.chunk.code.len() > root,
        "the function body should have been compiled after the root code"
    );
}
