//! Link and runtime diagnostics for TypeScript-authored programs carry a
//! location.
//!
//! `format_link_diagnostic` and `format_runtime_diagnostic` print
//! `--> line N, column M`, the source line and a caret only when the error
//! carries a span, and every span they can reach comes out of the lowered
//! program's `expression_source_spans`. The excerpt is cut from the source
//! text the caller hands the renderer, which for a TypeScript cell is the
//! TypeScript the model wrote — so the assertions below pin the TypeScript
//! line, column and caret run, not a printed lashlang form.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, LinkedModule, State, TypeExpr,
};

fn environment() -> LashlangHostEnvironment {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "read_file",
            "read_file",
            TypeExpr::Any,
            TypeExpr::Str,
        )
        .expect("host catalog operation must not conflict");
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "echo",
            "echo",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    LashlangHostEnvironment::new(catalog, LashlangAbilities::all())
}

fn link_diagnostic(source: &str) -> String {
    let program = lash_typescript::parse(source).expect("TypeScript source lowers");
    let error = LinkedModule::link(program, environment()).expect_err("link fails");
    lashlang::format_link_diagnostic(source, &error)
}

/// The location block a diagnostic ends with: the arrow line, the excerpt and
/// the caret run, with the hint (if any) dropped.
fn location_block(diagnostic: &str) -> String {
    let arrow = diagnostic
        .find("\n--> ")
        .unwrap_or_else(|| panic!("diagnostic carries no location:\n{diagnostic}"));
    diagnostic[arrow + 1..]
        .lines()
        .take_while(|line| !line.starts_with("hint: "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn unknown_resource_operation_points_at_the_operation_call() {
    let source = "const first = 1;\nconst value = await tools.does_not_exist({});\n";
    let diagnostic = link_diagnostic(source);
    assert_eq!(
        location_block(&diagnostic),
        "--> line 2, column 21\nconst value = await tools.does_not_exist({});\n                    ^~~~~~~~~~~~~~~~~~~~~~~~",
        "{diagnostic}"
    );
}

/// A host whose one resource operation always answers with a failed result, so
/// the `?` the dialect puts on an awaited operation raises at runtime.
struct FailingOperationHost;

impl ExecutionHost for FailingOperationHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) => Err(ExecutionHostError::new("the operation failed")),
            AbilityOp::Print(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unsupported ability: {other:?}"
            ))),
        }
    }
}

async fn runtime_diagnostic(source: &str) -> String {
    let program = lash_typescript::parse(source).expect("TypeScript source lowers");
    let linked = LinkedModule::link(program, environment()).expect("program links");
    let compiled = lashlang::compile_linked(&linked);
    let mut state = State::new();
    let host = ExecutionEnvironment::new(&FailingOperationHost).traced();
    lashlang::execute(&compiled, &mut state, &host)
        .await
        .expect_err("execution fails");
    let failure = host
        .take_runtime_failure()
        .expect("traced host records the failure");
    lashlang::format_runtime_diagnostic(source, &failure.error, failure.span)
}

#[tokio::test(flavor = "current_thread")]
async fn failed_operation_unwrap_points_at_the_operation_call() {
    let source = "const label = \"read\";\nconst body = await tools.read_file({ path: label });\n";
    let diagnostic = runtime_diagnostic(source).await;
    assert_eq!(
        location_block(&diagnostic),
        "--> line 2, column 20\nconst body = await tools.read_file({ path: label });\n                   ^~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
        "{diagnostic}"
    );
}
