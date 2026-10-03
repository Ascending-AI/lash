use super::*;

// Laws that keep declared `fn` invisible to durability.
//
// A declared function is the one construct that adds a call frame to programs
// written in plain lashlang, so it is the construct most likely to disturb
// effect identity or the shape of a captured continuation. Both properties are
// load-bearing for exactly-once replay, so they are pinned here rather than
// left to follow from the linker's effect ban by argument alone.

pub(crate) fn declared(
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
            Expr::CoercingBinary {
                op: CoercingBinaryOp::Multiply,
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
