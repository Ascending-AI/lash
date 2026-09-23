//! Public compile/execute entry points for lashlang programs.
//!
//! Execution is artifact-only: [`compile`] is the one public way to turn a
//! program into bytecode, and it compiles an entry point of an admitted
//! [`ModuleArtifact`]. The raw-AST compiler under it is crate-private.

use std::collections::BTreeMap;

use crate::ast::{AstPath, AstRoot, Program};
use crate::span::Span;
use crate::tracking::LashlangExecutionContext;
use crate::{ModuleArtifact, ProcessRef};

use super::record::intern_symbol;
use super::{
    CompiledProgram, Compiler, ExecutionHost, ExecutionOutcome, ExecutionScratch, LASH_TYPE_KEY,
    ProjectedBindings, RuntimeError, SlotState, State, Vm,
};

/// Which program of a module artifact to compile.
#[derive(Clone, Copy, Debug)]
pub enum Entry<'a> {
    /// The module's top-level program.
    Main,
    /// One exported process, by its ref.
    Process(&'a ProcessRef),
}

/// Compiles one entry point of an admitted module artifact.
///
/// `source_spans` are the authored spans a linked module keeps beside its
/// artifact ([`crate::LinkedModule::spans`]), keyed by the artifact program's
/// AST paths; they only position runtime diagnostics and never change what is
/// compiled.
pub fn compile(
    artifact: &ModuleArtifact,
    entry: Entry<'_>,
    source_spans: Option<&BTreeMap<AstPath, Span>>,
) -> Result<CompiledProgram, RuntimeError> {
    let program = &artifact.ir();
    match entry {
        Entry::Main => Ok(compile_main(artifact, source_spans)),
        Entry::Process(process_ref) => {
            let process_name = artifact.process_name_for_ref(process_ref).ok_or_else(|| {
                RuntimeError::ProcessRefNotExported {
                    module_ref: artifact.module_ref().clone(),
                    process_ref: process_ref.clone(),
                }
            })?;
            let (index, process) = program
                .declarations
                .iter()
                .enumerate()
                .find_map(|(index, declaration)| match declaration {
                    crate::Declaration::Process(process)
                        if process.name.as_str() == process_name =>
                    {
                        Some((index, process))
                    }
                    _ => None,
                })
                .ok_or_else(|| RuntimeError::ArtifactProcessMissing {
                    module_ref: artifact.module_ref().clone(),
                    name: process_name.to_string(),
                })?;
            // The process body compiles as the program's main, so its spans
            // move from the declaration's root to `main`.
            let root = AstRoot::Declaration(u32::try_from(index).unwrap_or(u32::MAX));
            let spans = source_spans
                .map(|spans| {
                    spans
                        .iter()
                        .filter(|(path, _)| path.root == root)
                        .map(|(path, span)| (AstPath::main(path.steps.clone()), *span))
                        .collect()
                })
                .unwrap_or_default();
            let process_program = Program {
                language: program.language.clone(),
                declarations: program.declarations.clone(),
                main: process.body.clone(),
                // A process body's bindings never reach session globals.
                private_bindings: Default::default(),
                spans: BTreeMap::new(),
            };
            let (chunk, compile_stats) = Compiler::compile_linked_process_program(
                &process_program,
                spans,
                artifact.into(),
                LashlangExecutionContext::process(process_name),
            );
            Ok(CompiledProgram {
                chunk,
                compile_stats,
            })
        }
    }
}

/// [`compile`] of [`Entry::Main`], which cannot fail: the artifact's own
/// program is its main entry.
pub(crate) fn compile_main(
    artifact: &ModuleArtifact,
    source_spans: Option<&BTreeMap<AstPath, Span>>,
) -> CompiledProgram {
    let spans = source_spans
        .map(|spans| {
            spans
                .iter()
                .filter(|(path, _)| path.root == AstRoot::Main)
                .map(|(path, span)| (path.clone(), *span))
                .collect()
        })
        .unwrap_or_default();
    let (chunk, compile_stats) = Compiler::compile_linked_program(
        artifact.ir(),
        spans,
        artifact.into(),
        LashlangExecutionContext::main(),
    );
    CompiledProgram {
        chunk,
        compile_stats,
    }
}

/// Compiles a program assembled through the AST API, with no module around
/// it: the unit-test primitive the VM's own tests drive. Every compile outside
/// this crate's tests goes through [`compile`].
#[cfg(test)]
pub(crate) fn compile_ast(program: &Program) -> Result<CompiledProgram, crate::ast::InvalidAst> {
    crate::ast::validate_ast(program)?;
    Ok(compile_program_internal(program))
}

#[cfg(test)]
pub(crate) fn compile_program_internal(program: &Program) -> CompiledProgram {
    let (chunk, compile_stats) = Compiler::compile_program(program);
    CompiledProgram {
        chunk,
        compile_stats,
    }
}

pub fn prewarm() {
    for name in [
        "ok",
        "value",
        "error",
        "__handle__",
        "handle",
        LASH_TYPE_KEY,
        "type",
        "properties",
        "required",
        "items",
        "enum",
        "id",
        "label",
        "size",
        "width",
        "height",
    ] {
        intern_symbol(name);
    }
}

pub async fn execute<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    execute_compiled_internal(program, state, host).await
}

pub(crate) async fn execute_compiled_internal<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    state.validate_program(program)?;
    let projected = host.projected_bindings();
    if let Some(mut scratch) = host.take_scratch() {
        let result =
            execute_with_optional_scratch(program, state, host, &projected, Some(&mut scratch))
                .await;
        host.store_scratch(scratch);
        result
    } else {
        execute_with_optional_scratch(program, state, host, &projected, None).await
    }
}

async fn execute_with_optional_scratch<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
    projected: &ProjectedBindings,
    scratch: Option<&mut ExecutionScratch>,
) -> Result<ExecutionOutcome, RuntimeError> {
    if let Some(scratch) = scratch {
        let (mut globals, mut heap) = state.take_runtime();
        // A snapshot restore leaves placeholders wherever a projection was
        // nested inside a container or a heap object; slot-name rebinding alone
        // never revisits those (FIG-2865).
        crate::runtime::projected_refresh::refresh_record(&mut globals, projected);
        crate::runtime::projected_refresh::refresh_heap(&mut heap, projected);
        let slots = SlotState::from_globals(
            globals,
            &program.chunk.slot_names,
            &program.chunk.private_slots,
            projected,
            std::mem::take(&mut scratch.slot_values),
        );
        let mut vm = Vm::new(
            &program.chunk,
            slots,
            host,
            Some(scratch),
            host.execution_mode(),
        );
        vm.install_heap(heap);
        let result = run_vm(program, host, &mut vm).await;
        let (runtime_globals, heap) = vm.recycle_into_state_parts(scratch)?;
        state.install_runtime(runtime_globals, heap)?;
        result
    } else {
        let (mut globals, mut heap) = state.take_runtime();
        crate::runtime::projected_refresh::refresh_record(&mut globals, projected);
        crate::runtime::projected_refresh::refresh_heap(&mut heap, projected);
        let slots = SlotState::from_globals(
            globals,
            &program.chunk.slot_names,
            &program.chunk.private_slots,
            projected,
            Vec::new(),
        );
        let mut vm = Vm::new(&program.chunk, slots, host, None, host.execution_mode());
        vm.install_heap(heap);
        let result = run_vm(program, host, &mut vm).await;
        let (runtime_globals, heap) = vm.into_state_parts()?;
        state.install_runtime(runtime_globals, heap)?;
        result
    }
}

async fn run_vm<H: ExecutionHost>(
    program: &CompiledProgram,
    host: &H,
    vm: &mut Vm<'_, H>,
) -> Result<ExecutionOutcome, RuntimeError> {
    if host.profile_execution() {
        vm.enable_profile();
    }

    let result = if host.trace_runtime_errors() {
        vm.run_traced_for_mode().await.map_err(|failure| {
            let error = failure.error.clone();
            host.observe_runtime_failure(failure);
            error
        })
    } else {
        vm.run_for_mode().await
    };

    if host.profile_execution() {
        let mut profile = vm.take_profile();
        profile.compile_stats = program.compile_stats;
        host.observe_profile(profile);
    }

    result
}
