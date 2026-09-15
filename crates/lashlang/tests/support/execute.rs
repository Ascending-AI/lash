use lashlang::{ExecutionHost, ExecutionOutcome, State};

/// The dialect front-end's own refusal. TypeScript resolves names at parse
/// (ADR 0096), so a program that names something the session never bound is
/// refused here rather than at link.
pub type ParseError = lash_typescript::Diagnostic;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ExecuteError {
    #[error("{0}")]
    Parse(ParseError),
    #[error(transparent)]
    Link(#[from] lashlang::LinkError),
    #[error(transparent)]
    Runtime(#[from] lashlang::RuntimeError),
    #[error("{0}")]
    InvalidAst(#[from] lashlang::InvalidAst),
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
    environment: lashlang::LashlangHostEnvironment,
) -> Result<ExecutionOutcome, ExecuteError> {
    let mut globals = environment.globals.clone();
    globals.extend(state.globals().iter().map(|(name, _)| name.to_string()));
    let program = lash_typescript::parse_with_globals(source, &globals)?;
    let compiled = if let Ok(linked) = lashlang::LinkedModule::link(program.clone(), &environment) {
        lashlang::compile_linked(&linked)
    } else {
        lashlang::compile_ast(&program)?
    };
    lashlang::execute(&compiled, state, host)
        .await
        .map_err(ExecuteError::Runtime)
}
