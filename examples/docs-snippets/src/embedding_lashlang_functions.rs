//! Public AST-only Lashlang function embedding example.

use lashlang::{
    AbilityOp, AbilityResult, AssignTarget, BinaryOp, CatchClause, ExecutionHost,
    ExecutionHostError, ExecutionMode, ExecutionOutcome, Expr, FunctionExpr, Program, State,
    TryExpr, Value, Vm, VmRunOutcome, compile_ast, execute,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ProcessEvent(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected example effect")),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Process
    }
}

fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

fn function(body: Expr, captures: &[&str]) -> Expr {
    Expr::Function(Box::new(FunctionExpr {
        name: None,
        params: vec!["value".into()],
        captures: captures.iter().map(|name| (*name).into()).collect(),
        body: Box::new(body),
    }))
}

#[tokio::test(flavor = "current_thread")]
async fn ast_only_functions_compile_execute_map_and_checkpoint() {
    let program = Program::block(vec![
        assign("captured", Expr::Number(12.0)),
        assign(
            "add_captured",
            function(
                Expr::Binary {
                    left: Box::new(Expr::Variable("captured".into())),
                    op: BinaryOp::Add,
                    right: Box::new(Expr::Variable("value".into())),
                },
                &["captured"],
            ),
        ),
        Expr::Finish(Box::new(Expr::Map {
            items: Box::new(Expr::List(vec![Expr::Number(5.0)])),
            function: Box::new(Expr::Variable("add_captured".into())),
        })),
    ]);
    let compiled = compile_ast(&program);
    let outcome = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect("AST-only functions execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(vec![Value::Number(17.0)].into()))
    );

    let checkpoint_program = compile_ast(&Program::block(vec![
        assign(
            "checkpoint",
            function(Expr::Yield(Box::new(Expr::Variable("value".into()))), &[]),
        ),
        Expr::Finish(Box::new(Expr::Call {
            function: Box::new(Expr::Variable("checkpoint".into())),
            args: vec![Expr::Number(1.0)],
        })),
    ]));
    let mut state = State::new();
    let mut vm = Vm::from_state(&checkpoint_program, &mut state, &Host).expect("checkpoint VM");
    assert_eq!(
        vm.run_process_until_effect().await.expect("yield effect"),
        VmRunOutcome::EffectCompleted
    );
    assert_eq!(vm.suspend().expect("function checkpoint").frame_depth(), 1);
    assert_eq!(lashlang::DEFAULT_MAX_VM_FRAME_DEPTH.get(), 1_024);
}

#[tokio::test(flavor = "current_thread")]
async fn ast_only_exceptions_compile_and_execute() {
    let recovered = Expr::Try(Box::new(TryExpr {
        body: Box::new(Expr::Throw(Box::new(Expr::String("original".into())))),
        catch: Some(CatchClause {
            binding: "error".into(),
            body: Box::new(Expr::Variable("error".into())),
        }),
        finally: Some(Box::new(Expr::Null)),
    }));
    let compiled = compile_ast(&Program::block(vec![Expr::Finish(Box::new(recovered))]));
    let outcome = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect("AST-only exceptions execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("original".into()))
    );
}
