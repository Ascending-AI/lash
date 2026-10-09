use lash_vm::{ExecutionHost, ExecutionOutcome, State};

/// The dialect front-end's own refusal. TypeScript resolves names at parse
/// (ADR 0096), so a program that names something the session never bound is
/// refused here rather than at link.
pub type ParseError = lash_typescript::Diagnostic;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ExecuteError {
    #[error("{0}")]
    Parse(ParseError),
    #[error(transparent)]
    Link(#[from] lash_vm::LinkError),
    #[error(transparent)]
    Runtime(#[from] lash_vm::RuntimeError),
    #[error("{0}")]
    InvalidAst(#[from] lash_vm::InvalidAst),
    #[error(transparent)]
    Artifact(lash_vm::ModuleArtifactError),
}

impl From<lash_typescript::Diagnostic> for ExecuteError {
    fn from(diagnostic: lash_typescript::Diagnostic) -> Self {
        Self::Parse(diagnostic)
    }
}

/// Lowers `source` through the TypeScript front-end and runs it against `state`.
///
/// The live session globals are handed to the lowerer because TypeScript binds
/// names at parse time: without them a cell could not read what an earlier cell
/// in the same `State` bound, which several of these tests assert.
pub async fn execute<H: ExecutionHost>(
    source: &str,
    state: &mut State,
    host: &H,
    environment: lash_vm::LashVmHostEnvironment,
) -> Result<ExecutionOutcome, ExecuteError> {
    let mut globals = environment.globals.clone();
    globals.extend(state.globals().iter().map(|(name, _)| name.to_string()));
    let program = lash_typescript::parse_with_globals(source, &globals)?;
    let compiled = if let Ok(linked) = lash_vm::LinkedModule::link(program.clone(), &environment) {
        lash_vm::compile(&linked.artifact, lash_vm::Entry::Main, Some(linked.spans()))?
    } else {
        lash_vm_compile_program(&program)?
    };
    lash_vm::execute(&compiled, state, host)
        .await
        .map_err(ExecuteError::Runtime)
}

/// Compiles a `Program` written against the IR directly — for the builtin
/// intrinsics no dialect spells — and runs it against `state`.
// Not every target that compiles this module calls it.
#[allow(dead_code)]
pub async fn execute_program<H: ExecutionHost>(
    program: &lash_vm::Program,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, ExecuteError> {
    let compiled = lash_vm_compile_program(program)?;
    lash_vm::execute(&compiled, state, host)
        .await
        .map_err(ExecuteError::Runtime)
}

/// Compiles an IR program as the main entry of the raw module artifact it
/// forms, through the one public compile entry.
fn lash_vm_compile_program(
    program: &lash_vm::Program,
) -> Result<lash_vm::CompiledProgram, ExecuteError> {
    let artifact =
        lash_vm::ModuleArtifact::from_program(program.clone()).map_err(|error| match error {
            lash_vm::ModuleArtifactError::InvalidAst(error) => ExecuteError::InvalidAst(error),
            other => ExecuteError::Artifact(other),
        })?;
    Ok(lash_vm::compile(
        &artifact,
        lash_vm::Entry::Main,
        Some(&program.spans),
    )?)
}
