//! Compile-and-run scaffolding for tests that need a real VM.
//!
//! This crate's own unit tests grew a host, a host catalog and a handful of
//! execute wrappers. The TypeScript lens needs exactly those to prove that the
//! execution sites the compiler emits correlate with the workflow nodes the
//! projector mints — a proof that cannot live in `lashlang/src`, because a
//! `[dev-dependencies]` edge back on `lash-typescript` compiles a second
//! instance of this crate and its `Program` is then a different type (FIG-3057).
//!
//! So the scaffolding lives here, published behind the `testing` feature, and
//! the unit tests use it rather than keeping a second copy.

use crate::{
    AbilityOp, AbilityResult, CompiledProgram, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionOutcome, LashlangAbilities, LashlangExecutionSite,
    LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures, LinkedModule, Program,
    ProjectedBindings, ResourceOperation, ResourceOperationBatchResult, ResourceOperationResult,
    RuntimeError, RuntimeFailure, State, TypeExpr, Value,
};

/// A host that answers the four `tools.*` operations [`test_environment`]
/// publishes, and nothing else.
///
/// `echo` returns its `value` argument, `err` fails, and any other operation is
/// an unknown-operation error — enough for a program to reach a resource call,
/// a failure path or a batch without a real effect host.
pub struct EchoHost;

impl EchoHost {
    /// Answers one resource operation, for a host that delegates its default
    /// case here while overriding another ability.
    pub fn perform_resource_operation(
        operation: ResourceOperation,
    ) -> Result<Value, ExecutionHostError> {
        match operation.operation.as_str() {
            "echo" => {
                let value = operation
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(value)
            }
            "err" => Err(ExecutionHostError::new("boom")),
            other => Err(ExecutionHostError::new(format!(
                "unknown module operation: {other}"
            ))),
        }
    }
}

impl ExecutionHost for EchoHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                Self::perform_resource_operation(operation).map(AbilityResult::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .into_iter()
                        .map(|operation| {
                            ResourceOperationResult::from_result(Self::perform_resource_operation(
                                operation,
                            ))
                        })
                        .collect(),
                ),
            )),
            AbilityOp::Await(handle) => match handle {
                Value::Record(_) => Ok(AbilityResult::Value(Value::Null)),
                _ => Err(ExecutionHostError::new("expected handle record")),
            },
            AbilityOp::Print(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

/// The host environment the scaffolding links against: a `tools` module with
/// `echo`, `err`, `missing` and `spawn`, and every ability granted.
#[expect(
    clippy::expect_used,
    reason = "test-support fixture a #[test] fn calls; the clippy.toml exemptions reach #[test] fns, not this helper"
)]
pub fn test_environment() -> LashlangHostEnvironment {
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

/// [`test_environment`] with `@label` annotations enabled.
pub fn labeled_test_environment() -> LashlangHostEnvironment {
    test_environment()
        .with_language_features(LashlangLanguageFeatures::default().with_label_annotations())
}

/// Links and compiles `program` as a main program, with labels enabled.
pub fn compile_labeled_program(program: Program) -> CompiledProgram {
    crate::compile_linked(&link_labeled(program))
}

/// Links and compiles one declared process of `program`, with labels enabled.
#[expect(
    clippy::expect_used,
    reason = "test-support fixture a #[test] fn calls: the runbook-style program compiles or the fixture author's assumption breaks, per the message"
)]
pub fn compile_labeled_process_program(program: Program, process_name: &str) -> CompiledProgram {
    crate::compile_linked_process(&link_labeled(program), process_name)
        .expect("process should compile")
}

/// Links `program` against [`labeled_test_environment`].
#[expect(
    clippy::expect_used,
    reason = "test-support fixture a #[test] fn calls: linking against the labeled test environment succeeds or the fixture author is at fault, per the message"
)]
pub fn link_labeled(program: Program) -> LinkedModule {
    LinkedModule::link(program, labeled_test_environment()).expect("program should link")
}

/// Runs a compiled program against `host`.
pub async fn execute_compiled<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    crate::execute(program, state, host).await
}

/// Runs a compiled program with `projected` bindings in scope.
pub async fn execute_compiled_with_projected_bindings<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
    projected: &ProjectedBindings,
) -> Result<ExecutionOutcome, RuntimeError> {
    let env = ExecutionEnvironment::new(host).with_projected_bindings(projected.clone());
    crate::execute(program, state, &env).await
}

/// Runs a compiled program with tracing on, so a failure carries its span.
pub async fn execute_compiled_traced<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeFailure> {
    let env = ExecutionEnvironment::new(host).traced();
    match crate::execute(program, state, &env).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => Err(env
            .take_runtime_failure()
            .unwrap_or(RuntimeFailure { error, span: None })),
    }
}

/// Every execution site a compiled program's instructions carry, in
/// instruction order.
///
/// A compiled program's chunk is private, and the execution sites are the one
/// thing a correlation proof outside this crate has to read off it: the whole
/// question is whether a site the VM emits names the workflow node the
/// projector minted for the same source position.
pub fn compiled_execution_sites(compiled: &CompiledProgram) -> Vec<&LashlangExecutionSite> {
    compiled
        .chunk
        .lashlang_execution_sites
        .iter()
        .flatten()
        .collect()
}
