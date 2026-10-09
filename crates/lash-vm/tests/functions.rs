use lash_vm::{
    AbilityOp, AbilityOutcome, AssignTarget, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    Expr, FunctionExpr, Program, State, Value, execute,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected effect")),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn public_ast_constructs_and_calls_a_capturing_function() {
    let assign = |name: &str, expr: Expr| Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    };
    let program = Program::block(vec![
        assign("captured", Expr::Number(12.0)),
        assign(
            "add_captured",
            Expr::Function(Box::new(FunctionExpr {
                name: None,
                js_name: None,
                receiver: None,
                params: vec!["value".into()],
                captures: vec!["captured".into()],
                body: Box::new(Expr::CoercingBinary {
                    left: Box::new(Expr::Variable("captured".into())),
                    op: lash_vm::CoercingBinaryOp::Add,
                    right: Box::new(Expr::Variable("value".into())),
                }),
            })),
        ),
        Expr::Finish(Box::new(Expr::Call {
            function: Box::new(Expr::Variable("add_captured".into())),
            args: vec![Expr::Number(5.0)],
        })),
    ]);

    assert_eq!(
        execute(
            &lash_vm_compile_program(&program).expect("the program compiles"),
            &mut State::new(),
            &Host
        )
        .await
        .expect("public AST function executes"),
        ExecutionOutcome::Finished(Value::Number(17.0))
    );
}

/// Compiles an IR program as the main entry of the raw module artifact it
/// forms, through the one public compile entry.
fn lash_vm_compile_program(
    program: &lash_vm::Program,
) -> Result<lash_vm::CompiledProgram, Box<dyn std::error::Error>> {
    let artifact = lash_vm::ModuleArtifact::from_program(program.clone())?;
    Ok(lash_vm::compile(
        &artifact,
        lash_vm::Entry::Main,
        Some(&program.spans),
    )?)
}
