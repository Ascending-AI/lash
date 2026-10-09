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

use lash_vm::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, ExecutionOutcome, RuntimeError,
    State, Value,
};

const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityOutcome::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unsupported value-depth ability")),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::testing::compile(source).expect("TypeScript should compile");
    futures::executor::block_on(lash_vm::execute(&program, &mut State::new(), &Host))
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

/// The console observation renderer walks the same containers, so it carries
/// the same bound: an inspect step must refuse a value it cannot describe
/// rather than take the thread stack down with it (FIG-2767).
#[test]
fn console_observation_of_a_deeply_nested_array_is_a_typed_refusal() {
    on_stack_budget("value-depth-console-observation", || {
        let error = execute(&nested_array_source(3_000, "console.log(deep);"))
            .expect_err("an over-deep observation must refuse");
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

/// The ceiling is the durable one, and it is a ceiling rather than a cliff:
/// the export walk accepts a value nested at exactly the depth a snapshot
/// accepts, and refuses only past it.
///
/// Pinned at the boundary itself rather than a comfortable distance below it.
/// The slack a lower number leaves is what hid an export-cache off-by-one that
/// made a chain of exactly 64 export on a cold cache and refuse on a warm one.
#[test]
fn nesting_at_the_durable_ceiling_still_exports() {
    on_stack_budget("value-depth-accepted-export", || {
        // 63 wraps around the leaf array is exactly `MAX_SNAPSHOT_VALUE_DEPTH`
        // reference levels, the deepest graph the durable boundary accepts.
        let outcome = execute(&nested_array_source(63, "finish(1);"))
            .expect("a global at exactly the ceiling must export at terminal exit");
        assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));
        let error = execute(&nested_array_source(64, "finish(1);"))
            .expect_err("one level past the ceiling must refuse");
        assert!(
            matches!(error, RuntimeError::ValueDepthLimitExceeded { .. }),
            "{error}"
        );
    });
}

/// The coercion walk's ceiling is the export walk's: both measure reference
/// levels, so a value at exactly the durable ceiling coerces, and only a level
/// past it refuses.
#[test]
fn nesting_at_the_coercion_ceiling_still_coerces() {
    on_stack_budget("value-depth-accepted-coercion", || {
        // 63 wraps is 64 reference levels — `MAX_SNAPSHOT_VALUE_DEPTH` — the
        // deepest graph the export walk and the durable boundary accept.
        let outcome = execute(&nested_array_source(63, "finish(String(deep).length > 0);"))
            .expect("coercion at the export ceiling must execute");
        assert_eq!(outcome, ExecutionOutcome::Finished(Value::Bool(true)));
        let error = execute(&nested_array_source(64, "finish(String(deep));"))
            .expect_err("one level past the export ceiling must refuse coercion");
        assert!(
            matches!(error, RuntimeError::ValueDepthLimitExceeded { .. }),
            "{error}"
        );
    });
}
