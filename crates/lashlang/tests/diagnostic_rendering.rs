//! What the link and runtime diagnostic renderers print for a real program.
//!
//! ADR 0096 makes TypeScript the sole authored RLM dialect, so the two
//! diagnostics below are exercised against TypeScript lowered through
//! `lash_typescript` rather than against the retired Lashlang surface. They
//! keep the case names the unit-test corpus used, so the corpus entry and its
//! replacement read side by side.
//!
//! FIG-3065 carries TypeScript spans into link diagnostics: `lash_typescript`
//! now fills its span tables, so both renderers emit the `--> line N, column M`
//! block, the source line and the caret run for a TypeScript program. These
//! tests therefore assert the message, the hint and the location block exactly.
//! The renderer's location machinery is pinned in addition, on stated span
//! tables, in `src/runtime/tests.rs`.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, State, TypeExpr,
};

struct DiagnosticHost;

impl ExecutionHost for DiagnosticHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) if operation.operation == "err" => {
                Err(ExecutionHostError::new("boom"))
            }
            AbilityOp::ResourceOperation(operation) => Ok(AbilityResult::Value(
                operation
                    .args
                    .first()
                    .cloned()
                    .unwrap_or(lashlang::Value::Null),
            )),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

/// The same `tools` surface the unit-test corpus linked against, so the
/// "available operations" hint below is the one the corpus recorded.
#[expect(
    clippy::expect_used,
    reason = "fixture catalog registers each operation once into a fresh catalog, per the message"
)]
fn environment() -> LashlangHostEnvironment {
    let mut resources = LashlangHostCatalog::new();
    for operation in ["echo", "err", "missing", "spawn"] {
        resources
            .add_module_operation(
                ["tools"],
                "Tools",
                operation,
                operation,
                TypeExpr::Any,
                TypeExpr::Any,
            )
            .expect("host catalog operation must not conflict");
    }
    LashlangHostEnvironment::new(resources, LashlangAbilities::all())
}

#[expect(
    clippy::expect_used,
    reason = "the fixture lowers a known TypeScript source, per the message"
)]
fn lower(source: &str) -> lashlang::Program {
    let environment = environment();
    lash_typescript::parse_with_globals(source, &environment.globals)
        .expect("diagnostic source should lower")
}

/// Was `link_unknown_resource_operation` in the unit-test diagnostic corpus.
#[test]
fn link_unknown_resource_operation() {
    let source = "finish(await tools.does_not_exist({}));";
    let error = lashlang::LinkedModule::link(lower(source), environment())
        .expect_err("unknown operation should not link");
    let diagnostic = lashlang::format_link_diagnostic(source, &error);

    assert!(
        diagnostic.contains("resource type `Tools` does not expose operation `does_not_exist`"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(
            "hint: available operations: `tools.echo`, `tools.err`, `tools.missing`, `tools.spawn`"
        ),
        "{diagnostic}"
    );
    // FIG-3065 carries TypeScript spans into link diagnostics.
    assert!(
        diagnostic.contains(
            "--> line 1, column 14\nfinish(await tools.does_not_exist({}));\n             ^~~~~~~~~~~~~~~~~~~~~~~~"
        ),
        "{diagnostic}"
    );
}

/// Was `runtime_failed_resource_operation_unwrap` in the unit-test diagnostic
/// corpus.
#[tokio::test(flavor = "current_thread")]
async fn runtime_failed_resource_operation_unwrap() {
    let source = "finish(await tools.err({}));";
    let program = lower(source);
    let linked = lashlang::LinkedModule::link(program, environment()).expect("program should link");
    let compiled = lashlang::compile_linked(&linked);
    let mut state = State::new();
    let host = ExecutionEnvironment::new(&DiagnosticHost).traced();
    lashlang::execute(&compiled, &mut state, &host)
        .await
        .expect_err("the failed tool call should abort the program");
    let failure = host
        .take_runtime_failure()
        .expect("the traced host records the failure");
    let diagnostic = lashlang::format_runtime_diagnostic(source, &failure.error, failure.span);

    assert!(
        diagnostic.contains("`?` unwrapped failed module operation: boom"),
        "{diagnostic}"
    );
    // FIG-3065 carries TypeScript spans into runtime diagnostics.
    assert!(
        diagnostic.contains(
            "--> line 1, column 14\nfinish(await tools.err({}));\n             ^~~~~~~~~~~~~"
        ),
        "{diagnostic}"
    );
}
