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

/// The Lashlang dialect shares the cell boundary with the TypeScript dialect
/// (FIG-1562): the state survives while each program is compiled on its own, so
/// a closure left behind by one program must not be validated against the next
/// one's function table.
#[tokio::test(flavor = "current_thread")]
async fn a_closure_from_an_earlier_program_does_not_fail_the_next_one() {
    let assign = |name: &str, expr: Expr| Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    };
    let first = Program::block(vec![
        assign("captured", Expr::Number(12.0)),
        assign(
            "add_captured",
            Expr::Function(Box::new(FunctionExpr {
                name: None,
                params: vec!["value".into()],
                captures: vec!["captured".into()],
                body: Box::new(Expr::Variable("captured".into())),
            })),
        ),
        assign("kept", Expr::Number(7.0)),
    ]);
    // A program with no functions at all: every stale closure index is out of
    // range for it, which is the shape the live failure took.
    let second = Program::block(vec![Expr::Finish(Box::new(Expr::Number(42.0)))]);

    let mut state = State::new();
    execute(&first, &mut state, &Host)
        .await
        .expect("the first program executes");
    assert_eq!(
        execute(&second, &mut state, &Host)
            .await
            .expect("a later program must not inherit the earlier program's closures"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
    assert_eq!(state.globals().get("kept"), Some(&Value::Number(7.0)));
}

/// The same law for a *declared* function, written in the surface syntax a cell
/// actually contains.
///
/// The test above builds its closure through the public AST, with explicit
/// captures. This one does not build a closure at all, on the face of it: `fn`
/// is a declaration, and Lashlang has no first-class function value. The closure
/// appears anyway, because a declared call is materialized at its call site as a
/// capture-free closure over the chunk function — so an ordinary `fn` cell
/// leaves one on the heap and the next program is validated against it.
///
/// Red before the FIG-1562 fix with `UnknownFunction { index: 0 }`. It is worth
/// having next to the RLM cell suite (`lash-protocol-rlm`'s
/// `testing::cell_conformance`) rather than only inside it: through an RLM
/// Lashlang cell the defect is latent rather than certain, because whether a
/// stale index resolves depends on what the *next* cell's chunk happens to
/// contain. Here the next program has no functions at all, so there is nothing
/// for a stale index to land on and the law is asserted rather than sampled.
#[tokio::test(flavor = "current_thread")]
async fn a_declared_function_from_an_earlier_program_does_not_fail_the_next_one() {
    let first = lashlang::compile("fn scale(n: float) -> float { n * 2 }\nscaled = scale(3)")
        .expect("the declaring program compiles");
    let second = lashlang::compile("finish 42").expect("the next program compiles");

    let mut state = State::new();
    execute(&first, &mut state, &Host)
        .await
        .expect("the declaring program executes");
    assert_eq!(
        execute(&second, &mut state, &Host)
            .await
            .expect("a later program must not inherit the earlier program's closures"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
    assert_eq!(state.globals().get("scaled"), Some(&Value::Number(6.0)));
}
