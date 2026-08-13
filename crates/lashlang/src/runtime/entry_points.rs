//! Public compile/execute entry points for lashlang programs.

use crate::ast::Program;
use crate::tracking::LashlangExecutionContext;
use crate::{LinkedModule, ModuleArtifact, ProcessRef};

use super::record::intern_symbol;
use super::{
    CompiledProgram, Compiler, ExecutionHost, ExecutionOutcome, ExecutionScratch, LASH_TYPE_KEY,
    ProjectedBindings, RuntimeError, SlotState, State, Vm,
};

pub enum ExecutableProgram<'program> {
    Program(&'program Program),
    Compiled(&'program CompiledProgram),
}

impl<'program> From<&'program Program> for ExecutableProgram<'program> {
    fn from(program: &'program Program) -> Self {
        Self::Program(program)
    }
}

impl<'program> From<&'program CompiledProgram> for ExecutableProgram<'program> {
    fn from(program: &'program CompiledProgram) -> Self {
        Self::Compiled(program)
    }
}

pub fn compile(source: &str) -> Result<CompiledProgram, crate::parser::ParseError> {
    crate::parse(source).map(|program| compile_program_internal(&program))
}

/// Compiles a program assembled through the AST API.
///
/// This is the entry point for AST-only nodes such as user functions, calls,
/// callback-driven maps and structured exception scopes, which intentionally
/// have no source syntax — and therefore no parser to bound how deeply a caller
/// nests them. The depth cap is applied here instead, so an over-deep tree is a
/// typed error rather than a stack overflow in a later AST walk.
pub fn compile_ast(program: &Program) -> Result<CompiledProgram, crate::ast::InvalidAst> {
    compile_ast_with_dialect(program, super::CompilationDialect::Lashlang)
}

/// Compiles an AST using the source dialect's value-semantics contract.
pub fn compile_ast_with_dialect(
    program: &Program,
    dialect: super::CompilationDialect,
) -> Result<CompiledProgram, crate::ast::InvalidAst> {
    crate::ast::validate_ast(program)?;
    let (chunk, compile_stats) = Compiler::compile_program_with_dialect(program, dialect);
    Ok(CompiledProgram {
        chunk,
        compile_stats,
        dialect,
    })
}

pub(crate) fn compile_program_internal(program: &Program) -> CompiledProgram {
    let (chunk, compile_stats) = Compiler::compile_program(program);
    CompiledProgram {
        chunk,
        compile_stats,
        dialect: super::CompilationDialect::Lashlang,
    }
}

pub fn compile_linked(linked: &LinkedModule) -> CompiledProgram {
    compile_linked_with_dialect(linked, super::CompilationDialect::Lashlang)
}

/// Compiles a linked shared-AST module using the requested language dialect.
pub fn compile_linked_with_dialect(
    linked: &LinkedModule,
    dialect: super::CompilationDialect,
) -> CompiledProgram {
    let (chunk, compile_stats) = Compiler::compile_linked_program_with_dialect(
        linked.program(),
        (&linked.artifact).into(),
        LashlangExecutionContext::main(linked.artifact.module_ref.clone()),
        dialect,
    );
    CompiledProgram {
        chunk,
        compile_stats,
        dialect,
    }
}

pub fn compile_process(
    program: &Program,
    process_name: &str,
) -> Result<CompiledProgram, RuntimeError> {
    crate::ast::check_ast_nesting_depth(program).map_err(|error| {
        RuntimeError::ValidationFailed {
            reason: error.to_string(),
        }
    })?;
    let process = program
        .process(process_name)
        .ok_or_else(|| RuntimeError::UnknownProcess {
            name: process_name.to_string(),
        })?;
    let process_program = Program {
        declarations: program.declarations.clone(),
        main: process.body.clone(),
        declaration_spans: program.declaration_spans.clone(),
        expression_spans: Vec::new(),
        expression_source_spans: Vec::new(),
    };
    Ok(compile_program_internal(&process_program))
}

pub fn compile_linked_process(
    linked: &LinkedModule,
    process_name: &str,
) -> Result<CompiledProgram, RuntimeError> {
    let linked_program = linked.program();
    let process =
        linked_program
            .process(process_name)
            .ok_or_else(|| RuntimeError::UnknownProcess {
                name: process_name.to_string(),
            })?;
    let process_program = Program {
        declarations: linked_program.declarations.clone(),
        main: process.body.clone(),
        declaration_spans: linked_program.declaration_spans.clone(),
        expression_spans: Vec::new(),
        expression_source_spans: Vec::new(),
    };
    let process_ref = linked
        .artifact
        .process_ref(process_name)
        .cloned()
        .ok_or_else(|| RuntimeError::ProcessNotExported {
            name: process_name.to_string(),
        })?;
    let (chunk, compile_stats) = Compiler::compile_linked_process_program(
        &process_program,
        (&linked.artifact).into(),
        LashlangExecutionContext::process(
            linked.artifact.module_ref.clone(),
            process_ref,
            process_name,
        ),
    );
    Ok(CompiledProgram {
        chunk,
        compile_stats,
        dialect: super::CompilationDialect::Lashlang,
    })
}

pub fn compile_module_artifact_process(
    artifact: &ModuleArtifact,
    process_ref: &ProcessRef,
) -> Result<CompiledProgram, RuntimeError> {
    let process_name = artifact.process_name_for_ref(process_ref).ok_or_else(|| {
        RuntimeError::ProcessRefNotExported {
            module_ref: artifact.module_ref.clone(),
            process_ref: process_ref.clone(),
        }
    })?;
    let process = artifact.canonical_ir.process(process_name).ok_or_else(|| {
        RuntimeError::ArtifactProcessMissing {
            module_ref: artifact.module_ref.clone(),
            name: process_name.to_string(),
        }
    })?;
    let process_program = Program {
        declarations: artifact.canonical_ir.declarations.clone(),
        main: process.body.clone(),
        declaration_spans: artifact.canonical_ir.declaration_spans.clone(),
        expression_spans: Vec::new(),
        expression_source_spans: Vec::new(),
    };
    let (chunk, compile_stats) = Compiler::compile_linked_process_program(
        &process_program,
        artifact.into(),
        LashlangExecutionContext::process(
            artifact.module_ref.clone(),
            process_ref.clone(),
            process_name,
        ),
    );
    Ok(CompiledProgram {
        chunk,
        compile_stats,
        dialect: super::CompilationDialect::Lashlang,
    })
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

pub async fn execute<'program, H: ExecutionHost>(
    program: impl Into<ExecutableProgram<'program>>,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    match program.into() {
        ExecutableProgram::Program(program) => {
            let compiled = compile_program_internal(program);
            execute_compiled_internal(&compiled, state, host).await
        }
        ExecutableProgram::Compiled(compiled) => {
            execute_compiled_internal(compiled, state, host).await
        }
    }
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
    if program.dialect == super::CompilationDialect::Typescript {
        state.reference_semantics = true;
    }
    if let Some(scratch) = scratch {
        let (globals, heap) = state.take_runtime();
        let slots = SlotState::from_globals_with_scratch(
            globals,
            &program.chunk.slot_names,
            scratch,
            projected,
        );
        let mut vm = Vm::new_with_scratch_and_mode(
            &program.chunk,
            slots,
            host,
            scratch,
            host.execution_mode(),
        );
        if state.reference_semantics {
            vm.reference_semantics = true;
        }
        vm.install_heap(heap);
        let result = run_vm(program, host, &mut vm).await;
        let (runtime_globals, heap) = vm.recycle_into_state_parts(scratch)?;
        state.install_runtime(runtime_globals, heap)?;
        result
    } else {
        let (globals, heap) = state.take_runtime();
        let slots = SlotState::from_globals(globals, &program.chunk.slot_names, projected);
        let mut vm = Vm::new_with_mode(&program.chunk, slots, host, host.execution_mode());
        if state.reference_semantics {
            vm.reference_semantics = true;
        }
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
