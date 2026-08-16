//! Deeply nested values fail as a typed refusal, not as a stack overflow.
//!
//! Two runtime walks materialize a value by recursing once per nesting level:
//! JavaScript's object-to-primitive coercion (`String(value)`, template
//! interpolation) and the heap export that runs over every runtime global on
//! every terminal exit. Both had only a cycle guard, which a finite but deeply
//! nested container walks straight past, so a guest that nested a few thousand
//! arrays took the whole host process down with `SIGABRT` — from ordinary
//! guest code, on the default host.
//!
//! The bound is the one the durable boundary already enforced downstream
//! (`MAX_SNAPSHOT_VALUE_DEPTH`, 64): a value nested deeper than a snapshot will
//! accept has no future anyway, so refusing it at the first walk that touches
//! it costs nothing and is the earliest deterministic point to say so.
//!
//! Every case here runs on the 2 MiB product stack budget, because a guard that
//! only holds on a test harness's larger stack is not the guarantee.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, RuntimeError,
    State, Value,
};

const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unsupported value-depth ability")),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn on_stack_budget<T: Send + 'static>(name: &str, test: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(test)
        .expect("spawn the stack-budget thread")
        .join()
        .expect("the stack-budget thread must not abort")
}

/// `levels` nested arrays built one level at a time, so nothing but the final
/// walk is ever deep.
fn nested_array_source(levels: usize, tail: &str) -> String {
    format!(
        "let deep: unknown = [1];\nfor (let level = 0; level < {levels}; level++) {{\n  deep = [deep];\n}}\n{tail}\n"
    )
}

/// The coercion walk: `String(deeplyNested)` recursed through
/// `javascript_sequence_string` once per level and aborted the process.
#[test]
fn string_coercion_of_a_deeply_nested_array_is_a_typed_refusal() {
    on_stack_budget("value-depth-string-coercion", || {
        let error = execute(&nested_array_source(3_000, "finish(String(deep));"))
            .expect_err("an over-deep coercion must refuse");
        assert!(
            matches!(error, RuntimeError::ValueDepthLimitExceeded { .. }),
            "{error}"
        );
    });
}

/// Template interpolation reaches the same walk by a different opcode.
#[test]
fn template_interpolation_of_a_deeply_nested_array_is_a_typed_refusal() {
    on_stack_budget("value-depth-template", || {
        let error = execute(&nested_array_source(3_000, "finish(`${deep}`);"))
            .expect_err("an over-deep interpolation must refuse");
        assert!(
            matches!(error, RuntimeError::ValueDepthLimitExceeded { .. }),
            "{error}"
        );
    });
}

/// The terminal-exit walk: every runtime global is exported when the cell
/// finishes, so a deep global aborted the process without the guest ever
/// touching it again.
#[test]
fn exporting_a_deeply_nested_runtime_global_at_terminal_exit_is_a_typed_refusal() {
    on_stack_budget("value-depth-terminal-export", || {
        let error = execute(&nested_array_source(1_200, "finish(1);"))
            .expect_err("an over-deep runtime global must refuse at terminal exit");
        assert!(
            matches!(error, RuntimeError::ValueDepthLimitExceeded { .. }),
            "{error}"
        );
    });
}

/// The ceiling is the durable one, and it is a ceiling rather than a cliff:
/// nesting the boundary allows still works, through both walks.
#[test]
fn nesting_within_the_durable_ceiling_still_coerces_and_exports() {
    on_stack_budget("value-depth-accepted", || {
        // 60 nested arrays plus the leaf leaves room for the wrappers the
        // coercion of a finish value adds on top.
        let outcome = execute(&nested_array_source(60, "finish(String(deep).length > 0);"))
            .expect("nesting within the ceiling must execute");
        assert_eq!(outcome, ExecutionOutcome::Finished(Value::Bool(true)));
    });
}
