//! Public AST-only Lashlang function embedding example.

use lashlang::{
    AbilityOp, AbilityResult, AssignTarget, BinaryOp, CatchClause, Declaration, ErrorTaxonomy,
    ExecutionHost, ExecutionHostError, ExecutionMode, ExecutionOutcome, Expr, FunctionDecl,
    FunctionExpr, FunctionParam, InvalidAst, MAX_AST_NESTING_DEPTH, NestingTooDeep, Program,
    RuntimeError, State, TryExpr, TypeExpr, Value, Vm, VmContinuation,
    VmFinallyCompletionContinuation, VmFinallyContinuation, VmHandlerContinuation,
    VmPendingErrorOriginContinuation, VmRunOutcome, check_ast_nesting_depth, compile_ast, execute,
    validate_ast,
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
    let compiled = compile_ast(&program).expect("program nesting is within the cap");
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
    ]))
    .expect("program nesting is within the cap");
    let mut state = State::new();
    let mut vm = Vm::from_state(&checkpoint_program, &mut state, &Host).expect("checkpoint VM");
    assert_eq!(
        vm.run_process_until_effect().await.expect("yield effect"),
        VmRunOutcome::EffectCompleted
    );
    assert_eq!(vm.suspend().expect("function checkpoint").frame_depth(), 1);
    assert_eq!(lashlang::DEFAULT_MAX_VM_FRAME_DEPTH.get(), 1_024);
}

/// Declared `fn` is the source-level counterpart of the AST-only closures
/// above: a named, top-level, pure function that a host can also build
/// directly and that call sites reach by name rather than through a value.
///
/// This is host code rather than test code because the AST shape *is* the
/// documented surface: a reader copying it needs the construction, not an
/// assertion harness around it.
async fn declared_function_module() -> ExecutionOutcome {
    let mut program = Program::block(vec![Expr::Finish(Box::new(Expr::FunctionCall {
        function: "double".into(),
        args: vec![Expr::Number(21.0)],
    }))]);
    program.declarations = vec![Declaration::Function(FunctionDecl {
        name: "double".into(),
        params: vec![FunctionParam {
            name: "value".into(),
            ty: TypeExpr::Float,
        }],
        return_ty: TypeExpr::Float,
        body: Expr::Binary {
            left: Box::new(Expr::Variable("value".into())),
            op: BinaryOp::Add,
            right: Box::new(Expr::Variable("value".into())),
        },
    })];

    let compiled = compile_ast(&program).expect("program nesting is within the cap");
    let outcome = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect("declared functions execute");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(42.0)));
    outcome
}

#[tokio::test(flavor = "current_thread")]
async fn declared_functions_compile_and_execute() {
    declared_function_module().await;
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
    let compiled = compile_ast(&Program::block(vec![Expr::Finish(Box::new(recovered))]))
        .expect("program nesting is within the cap");
    let outcome = execute(&compiled, &mut State::new(), &Host)
        .await
        .expect("AST-only exceptions execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("original".into()))
    );
}

/// A host-built AST has no parser to bound it, so the construction entry points
/// validate what the parser would have caught: nesting depth and the placement
/// of loop control.
#[test]
fn ast_construction_is_validated_before_compilation() {
    let shallow = Program::block(vec![Expr::Finish(Box::new(Expr::Number(1.0)))]);
    assert_eq!(check_ast_nesting_depth(&shallow), Ok(()));
    assert_eq!(validate_ast(&shallow), Ok(()));

    let mut deep = Expr::Number(1.0);
    for _ in 0..MAX_AST_NESTING_DEPTH {
        deep = Expr::List(vec![deep]);
    }
    let deep = Program::block(vec![Expr::Finish(Box::new(deep))]);
    assert_eq!(
        check_ast_nesting_depth(&deep),
        Err(NestingTooDeep {
            limit: MAX_AST_NESTING_DEPTH
        })
    );
    assert_eq!(
        compile_ast(&deep).err(),
        Some(InvalidAst::NestingTooDeep {
            source: NestingTooDeep {
                limit: MAX_AST_NESTING_DEPTH
            }
        })
    );

    let stray = Program::block(vec![Expr::Break, Expr::Finish(Box::new(Expr::Null))]);
    assert_eq!(
        compile_ast(&stray).err(),
        Some(InvalidAst::LoopControlOutsideLoop { keyword: "break" })
    );
}

/// Every runtime failure carries a stable guest-facing code and a taxonomy row
/// that says whether a guest handler may see it at all.
#[test]
fn runtime_errors_carry_a_code_and_a_taxonomy() {
    assert_eq!(RuntimeError::LenUnsupported.code(), "LenUnsupported");
    assert_eq!(
        RuntimeError::LenUnsupported.taxonomy(),
        ErrorTaxonomy::Catchable
    );
    assert_eq!(
        RuntimeError::HostCancelled.taxonomy(),
        ErrorTaxonomy::UncatchableTerminal
    );
    assert_eq!(
        RuntimeError::UnwrappedModuleOperationFailed {
            source: ExecutionHostError::new("upstream refused")
        }
        .taxonomy(),
        ErrorTaxonomy::EffectFailure
    );
}

/// A continuation captured while a cleanup chain is in flight exposes the
/// exception state a host stores: the handler that is still installed, the
/// finally that is running, and the typed failure it will re-raise.
#[tokio::test(flavor = "current_thread")]
async fn a_suspended_cleanup_chain_exposes_its_exception_state() {
    let failing = Expr::BuiltinCall {
        name: "len".into(),
        args: vec![Expr::Number(1.0)],
    };
    let program = compile_ast(&Program::block(vec![Expr::Finish(Box::new(Expr::Try(
        Box::new(TryExpr {
            body: Box::new(Expr::Try(Box::new(TryExpr {
                body: Box::new(failing),
                catch: None,
                finally: Some(Box::new(Expr::Yield(Box::new(Expr::Number(0.0))))),
            }))),
            catch: Some(CatchClause {
                binding: "error".into(),
                body: Box::new(Expr::Variable("error".into())),
            }),
            finally: None,
        }),
    )))]))
    .expect("the example program is within the AST limits");

    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &Host).expect("example VM");
    assert_eq!(
        vm.run_process_until_effect().await.expect("yield effect"),
        VmRunOutcome::EffectCompleted
    );
    let continuation: VmContinuation = vm.suspend().expect("cleanup continuation");

    let handler: &VmHandlerContinuation = continuation
        .handler_stack
        .first()
        .expect("the outer catch is still installed");
    assert!(handler.catches);

    let finally: &VmFinallyContinuation = continuation
        .finally_stack
        .first()
        .expect("the cleanup chain is in flight");
    let VmFinallyCompletionContinuation::Throw { origin, .. } = &finally.completion else {
        panic!("a cleanup chain carries a pending throw")
    };
    let origin: &VmPendingErrorOriginContinuation = origin
        .as_ref()
        .expect("a routed runtime failure keeps its origin");
    assert_eq!(origin.error, RuntimeError::LenUnsupported);
    assert_eq!(finally.handler_stack_depth, 1);
}
