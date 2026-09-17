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

/// Compiles a program assembled through the AST API.
///
/// This is the entry point for AST-only nodes such as user functions, calls,
/// callback-driven maps and structured exception scopes, which intentionally
/// have no source syntax — and therefore no parser to bound how deeply a caller
/// nests them. The depth cap is applied here instead, so an over-deep tree is a
/// typed error rather than a stack overflow in a later AST walk.
pub fn compile_ast(program: &Program) -> Result<CompiledProgram, crate::ast::InvalidAst> {
    crate::ast::validate_ast(program)?;
    let (chunk, compile_stats) = Compiler::compile_program(program);
    Ok(CompiledProgram {
        chunk,
        compile_stats,
    })
}

pub(crate) fn compile_program_internal(program: &Program) -> CompiledProgram {
    let (chunk, compile_stats) = Compiler::compile_program(program);
    CompiledProgram {
        chunk,
        compile_stats,
    }
}

pub fn compile_linked(linked: &LinkedModule) -> CompiledProgram {
    let (chunk, compile_stats) = Compiler::compile_linked_program(
        linked.program(),
        (&linked.artifact).into(),
        LashlangExecutionContext::main(linked.artifact.module_ref.clone()),
    );
    CompiledProgram {
        chunk,
        compile_stats,
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
        spans: Default::default(),
    };
    compile_ast(&process_program).map_err(|error| RuntimeError::ValidationFailed {
        reason: error.to_string(),
    })
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
        spans: Default::default(),
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
        spans: Default::default(),
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
        let slots =
            SlotState::from_globals(globals, &program.chunk.slot_names, projected, Vec::new());
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
