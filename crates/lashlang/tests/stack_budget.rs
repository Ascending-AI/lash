use lashlang::{
    AbilityOp, AbilityResult, CatchClause, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    Expr, LashlangAbilities, LashlangHostEnvironment, Program, Record, State, TryExpr, Value,
    compile_linked, execute, parse,
};
use std::sync::Arc;

const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

#[test]
fn stack_budget_lashlang_parse_link_compile_execute_process_fanout() {
    run_on_stack_budget("stack-budget-lashlang-process-fanout", || {
        let test = Box::pin(async {
            let program = parse(
                r#"
process child(value: str) {
  finish { value: value, lookup: "lookup:" + value }
}

left = start child(value: "left")
right = start child(value: "right")
joined = await { left: left, right: right }
sleep for "0ms"
finish {
  left: joined.left.lookup,
  right: joined.right.lookup,
  final: "stack-budget"
}
"#,
            )
            .expect("program parses");
            let surface = LashlangHostEnvironment::new(
                lashlang::LashlangHostCatalog::new(),
                LashlangAbilities::all(),
            );
            let linked = lashlang::LinkedModule::link(program, surface).expect("program links");
            let compiled = compile_linked(&linked);
            let mut state = State::new();

            let outcome = execute(&compiled, &mut state, &StackBudgetHost)
                .await
                .expect("program executes");
            let ExecutionOutcome::Finished(value) = outcome else {
                panic!("expected final value");
            };

            assert_eq!(
                serde_json::to_value(&value).expect("value json"),
                serde_json::json!({
                    "left": {
                        "ok": true,
                        "value": "lookup:left",
                    },
                    "right": {
                        "ok": true,
                        "value": "lookup:right",
                    },
                    "final": "stack-budget",
                })
            );
        });

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(test)
    });
}

/// The parser caps syntactic nesting (`MAX_NESTING_DEPTH`) precisely so that a
/// legal program can never drive a downstream AST walker off the stack: the cap
/// is only meaningful if link, compile and execute each cost no more per level
/// than the parser that admitted the program. This runs the deepest program the
/// parser will ever accept through the whole pipeline on the same 2 MiB budget a
/// host thread gets, so a walker that regrows its per-level frame fails here
/// instead of aborting a host process.
#[test]
fn stack_budget_lashlang_max_nesting_depth_parse_link_compile_execute() {
    run_on_stack_budget("stack-budget-lashlang-max-nesting", || {
        let deepest = deepest_accepted_nesting();
        assert!(
            deepest >= 39,
            "expected the parser to accept at least 39 nested levels, got {deepest}; \
             if MAX_NESTING_DEPTH moved on purpose, move this floor with it"
        );

        let program = parse(&nested_program(deepest)).expect("deepest program parses");
        let surface = LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            LashlangAbilities::all(),
        );
        let linked = lashlang::LinkedModule::link(program, surface).expect("program links");
        let compiled = compile_linked(&linked);
        let mut state = State::new();

        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(execute(&compiled, &mut state, &StackBudgetHost))
            .expect("program executes");
        let ExecutionOutcome::Finished(value) = outcome else {
            panic!("expected final value");
        };
        assert_eq!(
            serde_json::to_value(&value).expect("value json"),
            serde_json::json!({ "leaf": 0 })
        );
    });
}

/// `depth` levels of AST-only `try/catch/finally` around a constant. The parser
/// has no production for these nodes, so nothing about the source grammar
/// bounds how deep a dialect can build them.
fn nested_try_program(depth: usize) -> Program {
    let mut expr = Expr::Number(7.0);
    for level in 0..depth {
        expr = Expr::Try(Box::new(TryExpr {
            body: Box::new(expr),
            catch: Some(CatchClause {
                binding: format!("error_{level}").into(),
                body: Box::new(Expr::Number(-1.0)),
            }),
            finally: Some(Box::new(Expr::Null)),
        }));
    }
    Program::block(vec![Expr::Finish(Box::new(expr))])
}

fn stack_budget_environment() -> LashlangHostEnvironment {
    LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::all(),
    )
}

/// The AST-only exception nodes must meet the same 2 MiB budget as parsed
/// shapes at the depth the parser admits.
#[test]
fn stack_budget_ast_try_finally_at_parser_max_depth() {
    run_on_stack_budget("stack-budget-ast-try-finally", || {
        let program = nested_try_program(deepest_accepted_nesting());
        let linked =
            lashlang::LinkedModule::link(program, stack_budget_environment()).expect("links");
        let compiled = compile_linked(&linked);
        let mut state = State::new();
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(execute(&compiled, &mut state, &StackBudgetHost))
            .expect("program executes");
        assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));
    });
}

/// AST-only nodes have no source grammar to bound them, so the depth cap has to
/// live on the AST-construction entry points. Without it a dialect that lowers
/// a deeply nested `try` aborts the host process on a stack overflow instead of
/// returning a typed error, which a child process is the only way to observe.
#[test]
fn ast_only_nesting_beyond_the_cap_is_a_typed_error_not_an_abort() {
    if std::env::var("LASH_STACK_BUDGET_AST_DEPTH").is_ok() {
        let depth: usize = std::env::var("LASH_STACK_BUDGET_AST_DEPTH")
            .expect("depth")
            .parse()
            .expect("depth parses");
        run_on_stack_budget("stack-budget-ast-depth-child", move || {
            let program = nested_try_program(depth);
            let error = lashlang::LinkedModule::link(program.clone(), stack_budget_environment())
                .expect_err("linking must refuse the nesting");
            assert!(error.to_string().contains("nesting too deep"), "{error}");
        });
        return;
    }
    let exe = std::env::current_exe().expect("test binary");
    for depth in [80usize, 200, 1000] {
        let status = std::process::Command::new(&exe)
            .args([
                "ast_only_nesting_beyond_the_cap_is_a_typed_error_not_an_abort",
                "--exact",
                "--nocapture",
            ])
            .env("LASH_STACK_BUDGET_AST_DEPTH", depth.to_string())
            .status()
            .expect("spawn the child process");
        assert!(
            status.success(),
            "depth {depth} did not fail closed: {status}"
        );
    }
}

/// Walks up until the parser refuses, so the guard tracks the real cap rather
/// than a copy of it.
fn deepest_accepted_nesting() -> usize {
    let mut deepest = 0;
    for depth in 1..=128 {
        if parse(&nested_program(depth)).is_err() {
            break;
        }
        deepest = depth;
    }
    deepest
}

/// A record literal nested `depth` levels deep, read back through a field chain
/// of the same depth: the deepest shape the parser admits, in both the value it
/// builds and the path it walks.
fn nested_program(depth: usize) -> String {
    let mut literal = String::from("0");
    for _ in 0..depth {
        literal = format!("{{ next: {literal} }}");
    }
    let chain = ".next".repeat(depth);
    format!("tree = {literal}\nfinish {{ leaf: tree{chain} }}\n")
}

fn run_on_stack_budget(name: &str, test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(test)
        .expect("spawn stack-budget thread")
        .join()
        .expect("stack-budget thread");
}

struct StackBudgetHost;

impl ExecutionHost for StackBudgetHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::StartProcess(start) => {
                let Value::String(value) = start
                    .args
                    .get("value")
                    .cloned()
                    .unwrap_or(Value::String("unknown".into()))
                else {
                    return Err(ExecutionHostError::new("expected string value"));
                };
                let mut record = Record::new();
                record.insert("value".to_string(), Value::String(value.clone()));
                record.insert(
                    "lookup".to_string(),
                    Value::String(format!("lookup:{value}").into()),
                );
                Ok(AbilityResult::Value(Value::Record(Arc::new(record))))
            }
            AbilityOp::Await(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Sleep(_) => Ok(AbilityResult::Value(Value::Null)),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unsupported stack-budget host ability",
            )),
        }
    }
}
