use lashlang::{
    AbilityOp, AbilityResult, AssignTarget, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    Expr, FunctionExpr, Program, State, Value, execute,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
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
                params: vec!["value".into()],
                captures: vec!["captured".into()],
                body: Box::new(Expr::Binary {
                    left: Box::new(Expr::Variable("captured".into())),
                    op: lashlang::BinaryOp::Add,
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
        execute(&program, &mut State::new(), &Host)
            .await
            .expect("public AST function executes"),
        ExecutionOutcome::Finished(Value::Number(17.0))
    );
}
