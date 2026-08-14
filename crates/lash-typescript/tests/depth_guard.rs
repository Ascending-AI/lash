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

fn stack_budget_source(blocks: usize, parens: usize) -> String {
    format!(
        "{}finish({}1{});{}",
        "{".repeat(blocks),
        "(".repeat(parens),
        ")".repeat(parens),
        "}".repeat(blocks),
    )
}

fn delimiter_free_source(shape: &str, depth: usize) -> String {
    match shape {
        "not" => format!("finish({}1);", "!".repeat(depth)),
        "minus" => format!("finish({}1);", "- ".repeat(depth)),
        "typeof" => format!("finish({}1);", "typeof ".repeat(depth)),
        "ternary" => format!("finish({}1);", "1?1:".repeat(depth)),
        "binary" => format!("finish(1{});", "+1".repeat(depth)),
        _ => panic!("unknown delimiter-free nesting shape: {shape}"),
    }
}

fn mixed_delimiter_source(braces: usize, brackets: usize) -> String {
    format!(
        "{}finish({}1{});{}",
        "{".repeat(braces),
        "[".repeat(brackets),
        "]".repeat(brackets),
        "}".repeat(braces),
    )
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
fn delimiter_free_nesting_returns_a_named_diagnostic_without_aborting() {
    const CHILD_ENV: &str = "LASH_TS_DELIMITER_FREE_DEPTH_CHILD";
    const SHAPES: [&str; 5] = ["not", "minus", "typeof", "ternary", "binary"];
    if let Some(shape) = std::env::var_os(CHILD_ENV) {
        let shape = shape.to_string_lossy();
        let source = delimiter_free_source(&shape, 10_000);
        let error = lash_typescript::parse(&source).expect_err("nesting must be rejected");
        assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
        return;
    }

    for shape in SHAPES {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "delimiter_free_nesting_returns_a_named_diagnostic_without_aborting",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, shape)
            .status()
            .expect("depth child starts");
        assert!(
            status.success(),
            "{shape} depth child did not fail closed: {status}"
        );
    }
}

#[test]
fn mixed_delimiters_share_one_source_nesting_budget() {
    // The surrounding `finish(` call consumes one level of the total.
    lash_typescript::parse(&mixed_delimiter_source(13, 14))
        .expect("28 total delimiter levels should parse");
    let error = lash_typescript::parse(&mixed_delimiter_source(14, 14))
        .expect_err("29 total delimiter levels must reject");
    assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
}

#[test]
fn sequential_statement_forms_do_not_accumulate_source_nesting() {
    // Statement keywords open a recursive form that closes with the statement.
    // A flat sequence of them is one level deep however long the sequence is.
    for source in [
        "if (1) { const a = 1; } ".repeat(200),
        "while (0) { const a = 1; } ".repeat(200),
        "if (1) { const a = 1; } else { const b = 2; } ".repeat(200),
        "if (1) { if (0) { const a = 1; } } ".repeat(200),
        (0..200)
            .map(|index| {
                format!("function f{index}(): number {{ if (1) {{ return 1; }} return 0; }} ")
            })
            .collect::<String>(),
    ] {
        lash_typescript::parse(&source).unwrap_or_else(|error| {
            panic!("flat statement sequences parse: {}", error.code.as_str())
        });
    }
    // Statement forms outside the accepted surface still reject on their own
    // terms rather than on an accumulated nesting budget.
    let error = lash_typescript::parse(&"do { const a = 1; } while (0); ".repeat(200))
        .expect_err("do/while is outside the surface");
    assert_eq!(error.code.as_str(), "TS_DO_WHILE_UNSUPPORTED");
    let error = lash_typescript::parse(&format!("{}finish(1);", "if (1) ".repeat(200)))
        .expect_err("brace-free statement nesting must still reject");
    assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
}

#[test]
fn documented_source_nesting_limit_fits_the_two_mebibyte_stack_budget() {
    std::thread::Builder::new()
        .name("typescript-source-nesting-budget".to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let parens = DOCUMENTED_SOURCE_NESTING_LIMIT / 2;
            let blocks = DOCUMENTED_SOURCE_NESTING_LIMIT - parens - 1;
            // The blocks, grouping parentheses, and `finish(` call consume the
            // complete shared budget.
            let program = lash_typescript::compile(&stack_budget_source(blocks, parens))
                .expect("documented source nesting limit compiles");
            let outcome =
                futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
                    .expect("documented source nesting limit executes");
            assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));

            let error = lash_typescript::parse(&stack_budget_source(blocks + 1, parens))
                .expect_err("first over-limit source must be rejected");
            assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
        })
        .expect("stack-budget thread starts")
        .join()
        .expect("stack-budget thread does not abort or panic");
}
