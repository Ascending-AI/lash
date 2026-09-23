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
    LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures, LinkedModule,
    ProcessType, Program, ProjectedBindings, ResourceOperation, ResourceOperationBatchLeaf,
    ResourceOperationResult, RuntimeError, RuntimeFailure, State, TypeExpr, TypeField, Value,
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
                Self::perform_resource_operation(*operation).map(AbilityResult::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = batch
                    .leaves
                    .iter()
                    .map(|leaf| match leaf {
                        ResourceOperationBatchLeaf::Operation(operation) => {
                            ResourceOperationResult::from_result(Self::perform_resource_operation(
                                operation.clone(),
                            ))
                        }
                        ResourceOperationBatchLeaf::Timer(_) => {
                            ResourceOperationResult::Value(Value::Undefined)
                        }
                    })
                    .collect();
                Ok(AbilityResult::ResourceOperationBatch(
                    batch.answer_in_leaf_order(results),
                ))
            }
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

/// FIG-2999: starting, signalling, cancelling and yielding are leaf tools, not
/// special forms, so a fixture that drives a process needs them in its
/// catalogue.
#[expect(
    clippy::expect_used,
    reason = "test-support fixture a #[test] fn calls; the catalogue is empty of these names"
)]
pub fn add_process_control_operations(resources: &mut LashlangHostCatalog) {
    // `start` types its `definition` slot as a process: that expected type is
    // what lifts a process literal out of the argument, so a fixture that
    // starts one links the way a real catalogue's `processes.start` does.
    resources
        .add_module_operation(
            ["processes"],
            "Processes",
            "start",
            "start",
            TypeExpr::Object(vec![TypeField {
                name: "definition".into(),
                ty: TypeExpr::Process(ProcessType::unknown()),
                optional: false,
            }]),
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    for operation in ["signal", "cancel", "emit"] {
        resources
            .add_module_operation(
                ["processes"],
                "Processes",
                operation,
                operation,
                TypeExpr::Any,
                TypeExpr::Any,
            )
            .expect("host catalog operation must not conflict");
    }
}

/// The host environment the scaffolding links against: a `tools` module with
/// `echo`, `err`, `missing` and `spawn`, a `processes` module carrying the
/// process control tools, and every ability granted.
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
    add_process_control_operations(&mut resources);
    LashlangHostEnvironment::new(resources, LashlangAbilities::all())
}

/// [`test_environment`] with `@label` annotations enabled.
pub fn labeled_test_environment() -> LashlangHostEnvironment {
    test_environment()
        .with_language_features(LashlangLanguageFeatures::default().with_label_annotations())
}

/// Compiles a linked module's main program through the one compile entry,
/// keeping its source spans for diagnostics.
#[expect(
    clippy::expect_used,
    reason = "test-support fixture a #[test] fn calls: a module's main entry always compiles"
)]
pub fn compile_linked_main(linked: &LinkedModule) -> CompiledProgram {
    crate::compile(&linked.artifact, crate::Entry::Main, Some(linked.spans()))
        .expect("a module's main entry compiles")
}

/// Compiles one exported process of a linked module, by name, through the one
/// compile entry.
pub fn compile_linked_process_named(
    linked: &LinkedModule,
    process_name: &str,
) -> Result<CompiledProgram, RuntimeError> {
    let process_ref = linked.artifact.process_ref(process_name).ok_or_else(|| {
        RuntimeError::ProcessNotExported {
            name: process_name.to_string(),
        }
    })?;
    crate::compile(
        &linked.artifact,
        crate::Entry::Process(process_ref),
        Some(linked.spans()),
    )
}

/// Compiles a program built directly as IR, as the main entry of the raw
/// module artifact it forms, keeping its spans for diagnostics. A program
/// that forms no module (an invalid AST, an incomplete process signature) is
/// the error.
pub fn try_compile_program(
    program: &Program,
) -> Result<CompiledProgram, crate::ModuleArtifactError> {
    let artifact = crate::ModuleArtifact::from_program(program.clone())?;
    Ok(compile_artifact_main(&artifact, &program.spans))
}

/// [`try_compile_program`] for a fixture program that forms a module.
#[expect(
    clippy::expect_used,
    reason = "test-support fixture a #[test] fn calls: the fixture program forms a module or its author is at fault, per the message"
)]
pub fn compile_program(program: &Program) -> CompiledProgram {
    try_compile_program(program).expect("the fixture program forms a module artifact")
}

#[expect(
    clippy::expect_used,
    reason = "test-support fixture: a module's main entry always compiles"
)]
fn compile_artifact_main(
    artifact: &crate::ModuleArtifact,
    spans: &std::collections::BTreeMap<crate::AstPath, crate::Span>,
) -> CompiledProgram {
    crate::compile(artifact, crate::Entry::Main, Some(spans))
        .expect("a module's main entry compiles")
}

/// Links and compiles `program` as a main program, with labels enabled.
pub fn compile_labeled_program(program: Program) -> CompiledProgram {
    compile_linked_main(&link_labeled(program))
}

/// Links and compiles one declared process of `program`, with labels enabled.
#[expect(
    clippy::expect_used,
    reason = "test-support fixture a #[test] fn calls: the runbook-style program compiles or the fixture author's assumption breaks, per the message"
)]
pub fn compile_labeled_process_program(program: Program, process_name: &str) -> CompiledProgram {
    compile_linked_process_named(&link_labeled(program), process_name)
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

pub async fn execute_compiled<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    crate::execute(program, state, host).await
}

pub async fn execute_compiled_with_projected_bindings<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
    projected: &ProjectedBindings,
) -> Result<ExecutionOutcome, RuntimeError> {
    let env = ExecutionEnvironment::new(host).with_projected_bindings(projected.clone());
    crate::execute(program, state, &env).await
}

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
