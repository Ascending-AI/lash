use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

const DOCUMENTED_SOURCE_NESTING_LIMIT: usize = 28;
const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected ability in depth test")),
        }
    }
}

fn nested_if_source(depth: usize) -> String {
    let mut source = "if (true) {".repeat(depth);
    source.push_str("finish(1);");
    source.push_str(&"}".repeat(depth));
    source
}

#[test]
fn ten_thousand_nested_parens_return_a_named_diagnostic_without_aborting() {
    const CHILD_ENV: &str = "LASH_TS_DEPTH_GUARD_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        let source = format!("finish({}1{});", "(".repeat(10_000), ")".repeat(10_000));
        let error = lash_typescript::parse(&source).expect_err("nesting must be rejected");
        assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
        return;
    }

    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "ten_thousand_nested_parens_return_a_named_diagnostic_without_aborting",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .status()
        .expect("depth child starts");
    assert!(
        status.success(),
        "depth child did not fail closed: {status}"
    );
}

#[test]
fn documented_source_nesting_limit_fits_the_two_mebibyte_stack_budget() {
    std::thread::Builder::new()
        .name("typescript-source-nesting-budget".to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let program =
                lash_typescript::compile(&nested_if_source(DOCUMENTED_SOURCE_NESTING_LIMIT))
                    .expect("documented source nesting limit compiles");
            let outcome =
                futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
                    .expect("documented source nesting limit executes");
            assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));

            let error =
                lash_typescript::parse(&nested_if_source(DOCUMENTED_SOURCE_NESTING_LIMIT + 1))
                    .expect_err("first over-limit source must be rejected");
            assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
        })
        .expect("stack-budget thread starts")
        .join()
        .expect("stack-budget thread does not abort or panic");
}
